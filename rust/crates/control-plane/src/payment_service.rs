use crate::{
    billing::{conflict, invalid, lock_account},
    error::{ServiceError, ServiceResult},
    repository::{Repository, insert_balanced_entries},
};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait, QuerySelect, TransactionTrait};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use xscope_entities::{
    billing_order as order, payment, payment_checkout as checkout, provider_receipt as receipt,
};
use xscope_payments::{Alipay, VerifiedTrade};

pub fn provider_error(error: xscope_payments::Error) -> ServiceError {
    match error {
        xscope_payments::Error::Invalid(_) | xscope_payments::Error::Signature => {
            invalid("invalid or unverified payment provider evidence")
        }
        xscope_payments::Error::Rejected => {
            conflict("provider rejected the operation; query current state")
        }
        xscope_payments::Error::Unavailable => {
            ServiceError::Dependency("payment outcome unknown; query before retrying".into())
        }
    }
}
impl Repository {
    pub async fn payment_order(&self, id: &str) -> ServiceResult<order::Model> {
        order::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)
    }
    pub async fn alipay_checkout(&self, id: &str, provider: &Alipay) -> ServiceResult<Value> {
        let tx = self.db.begin().await?;
        let order = order::Entity::find_by_id(id)
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if order.status != "pending" || order.currency != "CNY" {
            return Err(conflict("checkout requires a pending CNY order"));
        }
        xscope_payments::format_amount(order.amount).map_err(provider_error)?;
        let time = chrono::Utc::now().fixed_offset();
        if let Some(prior) = checkout::Entity::find_by_id(id).one(&tx).await? {
            if prior.profile != provider.profile() {
                return Err(conflict(
                    "order bound to another payment merchant/environment",
                ));
            }
            if let Some(qr) = prior.qr_code {
                tx.commit().await?;
                return Ok(
                    json!({"state":prior.state,"qr_code":qr,"environment":prior.environment}),
                );
            }
        } else {
            checkout::ActiveModel {
                order_id: Set(id.into()),
                profile: Set(provider.profile()),
                environment: Set(provider.environment().into()),
                state: Set("creating".into()),
                qr_code: Set(None),
                created_at: Set(time),
                updated_at: Set(time),
            }
            .insert(&tx)
            .await?;
        }
        tx.commit().await?; // No database lock is held during external HTTP.
        let qr = provider
            .precreate(id, order.amount)
            .await
            .map_err(provider_error)?;
        let tx = self.db.begin().await?;
        let _order = order::Entity::find_by_id(id)
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let row = checkout::Entity::find_by_id(id)
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let state = if row.state == "creating" {
            "pending".to_owned()
        } else {
            row.state.clone()
        };
        let mut active: checkout::ActiveModel = row.into();
        active.state = Set(state.clone());
        active.qr_code = Set(Some(qr.clone()));
        active.updated_at = Set(chrono::Utc::now().fixed_offset());
        active.update(&tx).await?;
        tx.commit().await?;
        Ok(json!({"state":state,"qr_code":qr,"environment":provider.environment()}))
    }
    pub async fn accept_alipay_trade(
        &self,
        provider: &Alipay,
        trade: VerifiedTrade,
    ) -> ServiceResult<Value> {
        let tx = self.db.begin().await?;
        let order = order::Entity::find_by_id(trade.order_id())
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let checkout = checkout::Entity::find_by_id(&order.id)
            .one(&tx)
            .await?
            .ok_or_else(|| conflict("no checkout was initiated for this order"))?;
        if checkout.profile != provider.profile()
            || checkout.environment != provider.environment()
            || order.currency != "CNY"
            || order.amount != trade.amount_minor()
        {
            return Err(conflict(
                "verified trade does not match bound checkout amount or merchant",
            ));
        }
        let time = chrono::Utc::now().fixed_offset();
        let receipt_id = format!(
            "{:x}",
            Sha256::digest(format!(
                "{}:{}:{}",
                provider.profile(),
                order.id,
                trade.proof_sha256()
            ))
        );
        if receipt::Entity::find_by_id(&receipt_id)
            .one(&tx)
            .await?
            .is_none()
        {
            receipt::ActiveModel {
                id: Set(receipt_id),
                order_id: Set(order.id.clone()),
                profile: Set(provider.profile()),
                trade_id: Set(trade.trade_id().into()),
                state: Set(trade.state().into()),
                amount_minor: Set(trade.amount_minor()),
                proof_sha256: Set(trade.proof_sha256().into()),
                evidence: Set(trade.evidence().clone()),
                created_at: Set(time),
            }
            .insert(&tx)
            .await?;
        }
        // Sandbox observations are retained but never mint spendable balance.
        if provider.environment() != "production" {
            tx.commit().await?;
            return Ok(json!({"observed":true,"environment":"sandbox","credited":false}));
        }
        let payment_id = format!(
            "alipay-{:x}",
            Sha256::digest(format!("{}:{}", provider.profile(), trade.trade_id()))
        );
        if !trade.paid() {
            // Late pending/closed notifications never downgrade a paid order.
            if trade.state() == "TRADE_CLOSED" && order.status == "pending" {
                let mut active: checkout::ActiveModel = checkout.into();
                active.state = Set("closed".into());
                active.updated_at = Set(time);
                active.update(&tx).await?;
            }
            tx.commit().await?;
            return Ok(json!({"observed":true,"credited":false}));
        }
        if let Some(existing) = payment::Entity::find_by_id(&payment_id).one(&tx).await? {
            if existing.order_id != order.id
                || existing.amount != order.amount
                || existing.status != "succeeded"
            {
                return Err(conflict(
                    "provider trade already belongs to another payment",
                ));
            }
            tx.commit().await?;
            return Ok(json!({"credited":true,"duplicate":true,"payment_id":payment_id}));
        }
        if order.status != "pending" {
            return Err(conflict("order already paid with different evidence"));
        }
        let account = lock_account(&tx, &order.project_id).await?;
        payment::ActiveModel {
            id: Set(payment_id.clone()),
            order_id: Set(order.id.clone()),
            provider: Set("alipay".into()),
            provider_reference: Set(trade.trade_id().into()),
            amount: Set(order.amount),
            currency: Set("CNY".into()),
            status: Set("succeeded".into()),
            paid_at: Set(Some(time)),
            created_at: Set(time),
        }
        .insert(&tx)
        .await?;
        let amount = order
            .amount
            .checked_mul(xscope_domain::MICROS_PER_MINOR_UNIT)
            .ok_or_else(|| invalid("credit overflow"))?;
        insert_balanced_entries(
            &tx,
            &account,
            &order.tenant_id,
            "top_up",
            "payment",
            &payment_id,
            &format!("alipay:{payment_id}"),
            "Verified Alipay credit",
            amount,
            "cash_clearing",
        )
        .await?;
        let mut active: order::ActiveModel = order.into();
        active.status = Set("paid".into());
        active.updated_at = Set(time);
        active.update(&tx).await?;
        let mut active: checkout::ActiveModel = checkout.into();
        active.state = Set("paid".into());
        active.updated_at = Set(time);
        active.update(&tx).await?;
        tx.commit().await?;
        xscope_telemetry::background_event("payment", "credited");
        Ok(json!({"credited":true,"duplicate":false,"payment_id":payment_id}))
    }
}
