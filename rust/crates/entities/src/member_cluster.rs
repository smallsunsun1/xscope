use sea_orm::entity::prelude::*;
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "member_clusters", schema_name = "xscope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub namespace: String,
    pub credential_hash: String,
    pub credential_epoch: i64,
    pub expires_at: DateTimeWithTimeZone,
    pub revoked_at: Option<DateTimeWithTimeZone>,
    pub desired_version: i64,
    pub acknowledged_version: i64,
    pub delivery_version: i64,
    pub delivery_lease: Option<String>,
    pub lease_until: Option<DateTimeWithTimeZone>,
    pub heartbeat_at: Option<DateTimeWithTimeZone>,
    pub last_error: Option<String>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
