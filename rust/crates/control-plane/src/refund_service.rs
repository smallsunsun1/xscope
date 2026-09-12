use crate::{
    billing::{balance_and_held, conflict, invalid, lock_account},
    error::{ServiceError, ServiceResult},
    payment_service::provider_error,
    repository::{Repository, insert_balanced_entries},
};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QuerySelect,
    TransactionTrait,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use xscope_domain::{CreateRefundRequest, MICROS_PER_MINOR_UNIT};
use xscope_entities::{
    billing_order as order, ledger_entry, ledger_transaction, payment,
    payment_checkout as checkout, provider_receipt as receipt, refund,
};
use xscope_payments::{Alipay, VerifiedRefund};

impl Repository {
    pub async fn import_refund_evidence(
        &self,
        id: &str,
        bytes: &[u8],
        provider: &Alipay,
    ) -> ServiceResult<Value> {
        let row = refund::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let payment = payment::Entity::find_by_id(&row.payment_id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let proof = provider
            .verify_refund_response(
                bytes,
                &payment.order_id,
                &payment.provider_reference,
                id,
                row.amount,
            )
            .map_err(provider_error)?;
        self.complete_alipay_refund(provider, proof).await
    }
    pub async fn refund_payment(&self, id: &str) -> ServiceResult<payment::Model> {
        let row = refund::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)?;
        payment::Entity::find_by_id(row.payment_id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)
    }
    /// Durable debit to refund_pending precedes any external money movement.
    /// The ledger, not a mutable process flag, protects funds across restarts.
    pub async fn prepare_alipay_refund(
        &self,
        request: CreateRefundRequest,
        provider: &Alipay,
    ) -> ServiceResult<Value> {
        if request.id.is_empty()
            || request.id.len() > 64
            || !request
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || request.reason.trim().is_empty()
            || request.reason.len() > 256
            || request.amount.currency != "CNY"
        {
            return Err(invalid("invalid refund ID, reason or currency"));
        }
        xscope_payments::format_amount(request.amount.amount).map_err(provider_error)?;
        let tx = self.db.begin().await?;
        let payment = payment::Entity::find_by_id(&request.payment_id)
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let order = order::Entity::find_by_id(&payment.order_id)
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let checkout = checkout::Entity::find_by_id(&order.id)
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if payment.provider != "alipay"
            || payment.status != "succeeded"
            || payment.currency != "CNY"
            || checkout.profile != provider.profile()
            || checkout.environment != "production"
            || provider.environment() != "production"
        {
            return Err(conflict(
                "refund requires captured production Alipay payment and matching merchant",
            ));
        }
        if let Some(prior) = refund::Entity::find_by_id(&request.id).one(&tx).await? {
            if prior.payment_id != payment.id
                || prior.amount != request.amount.amount
                || prior.currency != request.amount.currency
                || prior.reason != request.reason
            {
                return Err(conflict("refund ID already bound to another request"));
            }
            tx.commit().await?;
            return Ok(json!({"id":prior.id,"state":prior.status,"duplicate":true}));
        }
        let prior = crate::billing::total(
            refund::Entity::find()
                .filter(refund::Column::PaymentId.eq(&payment.id))
                .filter(refund::Column::Status.is_in(["pending", "succeeded"])),
            refund::Column::Amount,
            &tx,
        )
        .await?;
        if prior
            .checked_add(request.amount.amount)
            .is_none_or(|n| n > payment.amount)
        {
            return Err(conflict("refund total exceeds captured amount"));
        }
        let account = lock_account(&tx, &order.project_id).await?;
        let (balance, held) = balance_and_held(&tx, &account.id).await?;
        let debit = request
            .amount
            .amount
            .checked_mul(MICROS_PER_MINOR_UNIT)
            .ok_or_else(|| invalid("refund overflow"))?;
        // Refunds must never use credit allowance, even if inference is postpaid.
        if balance
            .checked_sub(held)
            .is_none_or(|available| available < debit)
        {
            return Err(ServiceError::InsufficientFunds(
                "refund would consume spent or reserved balance".into(),
            ));
        }
        let time = chrono::Utc::now().fixed_offset();
        refund::ActiveModel {
            id: Set(request.id.clone()),
            payment_id: Set(payment.id),
            amount: Set(request.amount.amount),
            currency: Set("CNY".into()),
            reason: Set(request.reason),
            status: Set("pending".into()),
            provider_reference: Set(None),
            created_at: Set(time),
            completed_at: Set(None),
        }
        .insert(&tx)
        .await?;
        insert_balanced_entries(
            &tx,
            &account,
            &order.tenant_id,
            "refund_pending",
            "refund",
            &request.id,
            &format!("refund-hold:{}", request.id),
            "Alipay refund funds pending provider evidence",
            -debit,
            "refund_pending",
        )
        .await?;
        tx.commit().await?;
        Ok(json!({"id":request.id,"state":"pending","duplicate":false}))
    }
    pub async fn alipay_refund_operation(
        &self,
        id: &str,
        provider: &Alipay,
        submit: bool,
    ) -> ServiceResult<Value> {
        let row = refund::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let payment = payment::Entity::find_by_id(&row.payment_id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let checkout = checkout::Entity::find_by_id(&payment.order_id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if payment.provider != "alipay"
            || provider.environment() != "production"
            || checkout.profile != provider.profile()
        {
            return Err(conflict("refund merchant mismatch"));
        }
        if row.status == "succeeded" {
            return Ok(json!({"id":id,"state":"succeeded","duplicate":true}));
        }
        if row.status != "pending" {
            return Err(conflict("refund is not pending"));
        }
        if submit {
            // An ambiguous HTTP outcome intentionally leaves the same debit/ID.
            provider
                .submit_refund(&payment.order_id, id, row.amount)
                .await
                .map_err(provider_error)?;
            return Ok(json!({"id":id,"state":"pending","query_after_seconds":10}));
        }
        let verified = provider
            .query_refund(
                &payment.order_id,
                &payment.provider_reference,
                id,
                row.amount,
            )
            .await
            .map_err(provider_error)?;
        self.complete_alipay_refund(provider, verified).await
    }
    async fn complete_alipay_refund(
        &self,
        provider: &Alipay,
        proof: VerifiedRefund,
    ) -> ServiceResult<Value> {
        let tx = self.db.begin().await?;
        let row = refund::Entity::find_by_id(proof.request_id())
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let payment = payment::Entity::find_by_id(&row.payment_id)
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let order = order::Entity::find_by_id(&payment.order_id)
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let checkout = checkout::Entity::find_by_id(&order.id)
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if provider.environment() != "production"
            || checkout.environment != "production"
            || row.amount != proof.amount_minor()
            || payment.provider_reference != proof.trade_id()
            || order.id != proof.order_id()
            || checkout.profile != provider.profile()
            || payment.provider != "alipay"
        {
            return Err(conflict("refund evidence does not match durable request"));
        }
        if row.status == "succeeded" {
            tx.commit().await?;
            return Ok(json!({"id":row.id,"state":"succeeded","duplicate":true}));
        }
        if row.status != "pending" {
            return Err(conflict("refund no longer pending"));
        }
        let account = lock_account(&tx, &order.project_id).await?;
        let time = chrono::Utc::now().fixed_offset();
        let evidence =
            serde_json::to_vec(proof.evidence()).map_err(|_| invalid("invalid refund evidence"))?;
        let digest = format!("{:x}", Sha256::digest(&evidence));
        receipt::ActiveModel {
            id: Set(format!("refund-{digest}")),
            order_id: Set(order.id.clone()),
            profile: Set(provider.profile()),
            trade_id: Set(proof.trade_id().into()),
            state: Set("REFUND_SUCCESS".into()),
            amount_minor: Set(row.amount),
            proof_sha256: Set(digest),
            evidence: Set(proof.evidence().clone()),
            created_at: Set(time),
        }
        .insert(&tx)
        .await?;
        let transaction_id = format!("txn-{}", uuid::Uuid::now_v7());
        ledger_transaction::ActiveModel {
            id: Set(transaction_id.clone()),
            tenant_id: Set(order.tenant_id),
            kind: Set("refund_settled".into()),
            reference_type: Set("refund".into()),
            reference_id: Set(row.id.clone()),
            idempotency_key: Set(format!("refund-settle:{}", row.id)),
            currency: Set(row.currency.clone()),
            description: Set("Verified Alipay refund clearing".into()),
            created_at: Set(time),
        }
        .insert(&tx)
        .await?;
        let amount = row
            .amount
            .checked_mul(MICROS_PER_MINOR_UNIT)
            .ok_or_else(|| invalid("refund overflow"))?;
        for (name, delta) in [("refund_pending", -amount), ("cash_clearing", amount)] {
            ledger_entry::ActiveModel {
                id: Set(format!("entry-{}", uuid::Uuid::now_v7())),
                transaction_id: Set(transaction_id.clone()),
                billing_account_id: Set(account.id.clone()),
                ledger_account: Set(name.into()),
                amount_microunits: Set(delta),
                currency: Set(row.currency.clone()),
                created_at: Set(time),
            }
            .insert(&tx)
            .await?;
        }
        crate::billing::append_event(&tx,&account.id,"ledger.posted",&transaction_id,json!({"kind":"refund_settled","reference_type":"refund","reference_id":row.id,"customer_delta_microunits":0,"currency":"CNY"})).await?;
        let id = row.id.clone();
        let mut active: refund::ActiveModel = row.into();
        active.status = Set("succeeded".into());
        active.provider_reference = Set(Some(proof.trade_id().into()));
        active.completed_at = Set(Some(time));
        active.update(&tx).await?;
        tx.commit().await?;
        Ok(json!({"id":id,"state":"succeeded","duplicate":false}))
    }
}
