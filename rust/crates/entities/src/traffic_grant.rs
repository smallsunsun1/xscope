use sea_orm::entity::prelude::*;
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "traffic_grants", schema_name = "xscope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)] pub token_hash: String,
    pub session_id: String,
    pub expires_at: DateTimeWithTimeZone,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
