use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "billing_review_audit", schema_name = "xscope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub review_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub sequence: i32,
    pub kind: String,
    pub actor_id: String,
    pub payload: Json,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
