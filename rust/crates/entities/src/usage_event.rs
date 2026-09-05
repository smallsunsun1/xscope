use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "usage_events", schema_name = "xscope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub event_id: String,
    pub request_id: String,
    pub occurred_at: DateTimeWithTimeZone,
    pub tenant_id: String,
    pub project_id: String,
    pub api_key_id: String,
    pub model_id: String,
    pub model_revision: String,
    pub endpoint_id: String,
    pub region: String,
    pub price_version: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
    pub latency_ms: i64,
    pub status: String,
    pub cost_amount: i64,
    pub cost_microunits: i64,
    pub currency: String,
    pub received_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
