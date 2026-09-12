use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "billing_jobs", schema_name = "xscope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub billing_account_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub consumer: String,
    pub acknowledged: i64,
    pub target_sequence: i64,
    pub lease_epoch: i64,
    pub lease_through: i64,
    pub lease_token: Option<String>,
    pub lease_until: Option<DateTimeWithTimeZone>,
    pub available_at: DateTimeWithTimeZone,
    pub state: String,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub updated_at: DateTimeWithTimeZone,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
