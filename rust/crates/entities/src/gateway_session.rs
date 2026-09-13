use sea_orm::entity::prelude::*;
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "gateway_sessions", schema_name = "xscope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub sequence: i64,
    pub closed: bool,
    pub nonce: String,
    pub reported: Option<Json>,
    pub delivered_routes: Json,
    pub acknowledged_routes: Json,
    pub delivered_at: DateTimeWithTimeZone,
    pub identity: Option<Json>,
    pub runtime: Option<Json>,
    pub proof: Option<Json>,
    pub proof_nonce: Option<String>,
    pub proof_until: Option<DateTimeWithTimeZone>,
    pub gateway_cluster_id: Option<String>,
    pub proof_complete: bool,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
