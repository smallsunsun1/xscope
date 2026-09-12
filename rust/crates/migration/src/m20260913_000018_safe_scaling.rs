use sea_orm_migration::prelude::*;
#[derive(DeriveMigrationName)] pub struct Migration;
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let s=|n:&str| (Alias::new("xscope"),Alias::new(n));
        let mut alter=Table::alter(); alter.table(s("gateway_sessions"));
        for n in ["identity","runtime","proof"] { alter.add_column(ColumnDef::new(Alias::new(n)).json_binary()); }
        alter.add_column(ColumnDef::new(Alias::new("proof_nonce")).string());
        alter.add_column(ColumnDef::new(Alias::new("proof_until")).timestamp_with_time_zone());
        m.alter_table(alter).await?;
        m.alter_table(Table::alter().table(s("managed_pools")).add_column(ColumnDef::new(Alias::new("scale_operation")).json_binary()).to_owned()).await?;
        m.create_table(Table::create().table(s("traffic_grants"))
            .col(ColumnDef::new(Alias::new("token_hash")).string().not_null().primary_key())
            .col(ColumnDef::new(Alias::new("session_id")).string().not_null())
            .col(ColumnDef::new(Alias::new("expires_at")).timestamp_with_time_zone().not_null()).to_owned()).await?;
        m.create_index(Index::create().name("traffic_grants_expiry").table(s("traffic_grants")).col(Alias::new("session_id")).col(Alias::new("expires_at")).to_owned()).await?;
        Ok(())
    }
    async fn down(&self,_:&SchemaManager)->Result<(),DbErr> { Err(DbErr::Custom("termination evidence requires an explicit maintenance migration".into())) }
}
