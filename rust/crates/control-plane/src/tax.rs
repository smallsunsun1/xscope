//! A request intake, never an invoice issuer. Provider-specific issuance must
//! replace the pending-only constraint through a reviewed additive migration.
use crate::{
    billing::{conflict, invalid},
    error::{ServiceError, ServiceResult},
    repository::Repository,
};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait, QuerySelect, TransactionTrait};
use serde::Deserialize;
use serde_json::{Value, json};
use xscope_entities::{invoice, tax_request};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaxRequest {
    pub title: String,
    pub taxpayer_id: Option<String>,
}
impl Repository {
    pub async fn statement(&self, id: &str) -> ServiceResult<invoice::Model> {
        invoice::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)
    }
    pub async fn tax_request_status(&self, id: &str) -> ServiceResult<Value> {
        let row = tax_request::Entity::find_by_id(id).one(&self.db).await?;
        Ok(
            json!({"invoice_id":id,"jurisdiction":"CN","state":row.as_ref().map(|r|r.state.as_str()).unwrap_or("not_requested"),"tax_invoice_issued":false,"provider_configured":false,"created_at":row.map(|r|r.created_at)}),
        )
    }
    pub async fn request_tax_invoice(&self, id: &str, request: TaxRequest) -> ServiceResult<Value> {
        if request.title.trim().is_empty()
            || request.title.len() > 256
            || request.title.chars().any(char::is_control)
            || request.taxpayer_id.as_deref().is_some_and(|v| {
                v.is_empty() || v.len() > 32 || !v.bytes().all(|b| b.is_ascii_alphanumeric())
            })
        {
            return Err(invalid(
                "invalid bounded invoice title or taxpayer identity",
            ));
        }
        let tx = self.db.begin().await?;
        let invoice = invoice::Entity::find_by_id(id)
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if invoice.currency != "CNY" || invoice.amount <= 0 {
            return Err(conflict("positive CNY billing statement required"));
        }
        if let Some(prior) = tax_request::Entity::find_by_id(id).one(&tx).await? {
            if prior.title != request.title || prior.taxpayer_id != request.taxpayer_id {
                return Err(conflict(
                    "tax request identity already bound; provider correction workflow required",
                ));
            }
        } else {
            tax_request::ActiveModel {
                invoice_id: Set(id.into()),
                title: Set(request.title),
                taxpayer_id: Set(request.taxpayer_id),
                state: Set("pending_provider".into()),
                created_at: Set(chrono::Utc::now().fixed_offset()),
            }
            .insert(&tx)
            .await?;
        }
        tx.commit().await?;
        self.tax_request_status(id).await
    }
}
