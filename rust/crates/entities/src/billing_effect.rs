use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "billing_effects", schema_name = "xscope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub billing_account_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub consumer: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub sequence: i64,
    pub payload_sha256: String,
    pub kind: String,
    pub processed_at: DateTimeWithTimeZone,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
