use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "billing_reservations", schema_name = "xscope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub billing_account_id: String,
    pub project_id: String,
    pub tenant_id: String,
    pub api_key_id: String,
    pub request_id: String,
    pub spec: Json,
    pub price: Json,
    pub reserved_microunits: i64,
    pub settled_microunits: Option<i64>,
    pub state: String,
    pub completion: Option<Json>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
