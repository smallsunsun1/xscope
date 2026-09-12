//! Durable mutation intent precedes execution. Completion is a separate record:
//! missing completion explicitly means unknown, never proof of no mutation.
use crate::{billing::invalid, error::ServiceResult, repository::Repository};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect,
};
use serde::Deserialize;
use serde_json::{Value, json};
use xscope_entities::operation_audit as audit;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditQuery {
    pub after: Option<String>,
    pub limit: Option<u64>,
}
impl Repository {
    pub async fn audit_operation(
        &self,
        actor: &str,
        operation: &str,
        route: &str,
        payload: Value,
    ) -> ServiceResult<String> {
        let id = uuid::Uuid::now_v7().to_string();
        audit::ActiveModel {
            id: Set(id.clone()),
            actor_id: Set(actor.into()),
            action: Set(operation.into()),
            resource_id: Set(route.into()),
            payload: Set(payload),
            created_at: Set(chrono::Utc::now().fixed_offset()),
        }
        .insert(&self.db)
        .await?;
        Ok(id)
    }
    pub async fn list_operation_audits(&self, query: AuditQuery) -> ServiceResult<Value> {
        let limit = query.limit.unwrap_or(50);
        if !(1..=100).contains(&limit)
            || query
                .after
                .as_ref()
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
        {
            return Err(invalid("invalid audit page"));
        }
        let mut select = audit::Entity::find();
        if let Some(after) = query.after {
            select = select.filter(audit::Column::Id.gt(after));
        }
        let rows = select
            .order_by_asc(audit::Column::Id)
            .limit(limit + 1)
            .all(&self.db)
            .await?;
        let more = rows.len() as u64 > limit;
        let rows: Vec<_> = rows.into_iter().take(limit as usize).collect();
        Ok(
            json!({"next_after":if more { rows.last().map(|row| row.id.clone()) } else { None },"data":rows}),
        )
    }
}
