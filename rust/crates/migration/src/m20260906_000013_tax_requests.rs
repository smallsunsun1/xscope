use sea_orm_migration::prelude::*;
#[derive(DeriveMigrationName)]
pub struct Migration;
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = |name: &str| (Alias::new("xscope"), Alias::new(name));
        manager
            .create_table(
                Table::create()
                    .table(schema("tax_requests"))
                    .col(
                        ColumnDef::new(Alias::new("invoice_id"))
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Alias::new("title")).string().not_null())
                    .col(ColumnDef::new(Alias::new("taxpayer_id")).string())
                    .col(ColumnDef::new(Alias::new("state")).string().not_null())
                    .col(
                        ColumnDef::new(Alias::new("created_at"))
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("tax_request_statement_fk")
                            .from(schema("tax_requests"), Alias::new("invoice_id"))
                            .to(schema("invoices"), Alias::new("id"))
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .check(Expr::col(Alias::new("state")).eq("pending_provider"))
                    .to_owned(),
            )
            .await?;
        Ok(())
    }
    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "tax requests require explicit retention migration".into(),
        ))
    }
}
