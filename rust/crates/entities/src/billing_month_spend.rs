use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "billing_month_spend", schema_name = "xscope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub api_key_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub month: Date,
    pub spent_microunits: i64,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
