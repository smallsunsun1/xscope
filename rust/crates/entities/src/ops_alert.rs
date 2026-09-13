use sea_orm::entity::prelude::*;
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
#[sea_orm(table_name = "ops_alerts", schema_name = "xscope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub name: String,
    pub severity: String,
    pub summary: String,
    pub state: String,
    pub starts_at: DateTimeWithTimeZone,
    pub ends_at: Option<DateTimeWithTimeZone>,
    pub received_at: DateTimeWithTimeZone,
    pub acknowledged_by: Option<String>,
    pub acknowledged_at: Option<DateTimeWithTimeZone>,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
