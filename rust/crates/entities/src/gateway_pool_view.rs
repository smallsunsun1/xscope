use sea_orm::entity::prelude::*;
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "gateway_pool_views", schema_name = "xscope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub pool_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub session_id: String,
    pub delivered_generation: i64,
    pub acknowledged_generation: i64,
    pub active_requests: i64,
    pub retired: bool,
    pub reported_at: Option<DateTimeWithTimeZone>,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
