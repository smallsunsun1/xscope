use sea_orm::entity::prelude::*;
use serde::Serialize;
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
#[sea_orm(table_name = "route_revisions", schema_name = "xscope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub project_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub model: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub revision: i64,
    pub spec: Json,
    pub actor: String,
    pub operation: String,
    pub created_at: DateTimeWithTimeZone,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
