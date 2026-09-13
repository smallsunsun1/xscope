use sea_orm_migration::prelude::*;
#[derive(DeriveMigrationName)]
pub struct Migration;
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let table = (Alias::new("xscope"), Alias::new("ops_alerts"));
        let mut create = Table::create();
        create.table(table.clone()).col(
            ColumnDef::new(Alias::new("id"))
                .string()
                .not_null()
                .primary_key(),
        );
        for name in ["name", "severity", "summary", "state"] {
            create.col(ColumnDef::new(Alias::new(name)).string().not_null());
        }
        for name in ["starts_at", "received_at"] {
            create.col(
                ColumnDef::new(Alias::new(name))
                    .timestamp_with_time_zone()
                    .not_null(),
            );
        }
        for name in ["ends_at", "acknowledged_at"] {
            create.col(ColumnDef::new(Alias::new(name)).timestamp_with_time_zone());
        }
        create.col(ColumnDef::new(Alias::new("acknowledged_by")).string());
        m.create_table(create).await?;
        m.create_index(
            Index::create()
                .name("ops_alerts_state_id")
                .table(table)
                .col(Alias::new("state"))
                .col(Alias::new("id"))
                .to_owned(),
        )
        .await?;
        Ok(())
    }
    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "alert history removal requires explicit maintenance".into(),
        ))
    }
}
