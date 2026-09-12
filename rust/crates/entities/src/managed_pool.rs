use sea_orm::entity::prelude::*;
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "managed_pools", schema_name = "xscope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub cluster_id: String,
    pub deployment: String,
    pub generation: i64,
    pub state: String,
    pub binding: Json,
    pub endpoint: Json,
    pub expected_spec: Json,
    pub desired_version: i64,
    pub deployment_uid: Option<String>,
    pub ready_until: Option<DateTimeWithTimeZone>,
    pub observation_nonce: Option<String>,
    pub observation_until: Option<DateTimeWithTimeZone>,
    pub observation_code: String,
    pub observed_at: Option<DateTimeWithTimeZone>,
    pub updated_at: DateTimeWithTimeZone,
    pub scale_operation: Option<Json>,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
