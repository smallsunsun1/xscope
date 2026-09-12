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
                    .table(schema("model_catalog"))
                    .col(
                        ColumnDef::new(Alias::new("id"))
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("revision"))
                            .big_integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Alias::new("enabled")).boolean().not_null())
                    .col(ColumnDef::new(Alias::new("default_pool")).string())
                    .col(
                        ColumnDef::new(Alias::new("definition"))
                            .json_binary()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("updated_at"))
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .check(Expr::col(Alias::new("revision")).gt(0))
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(schema("model_prices"))
                    .col(ColumnDef::new(Alias::new("model_id")).string().not_null())
                    .col(ColumnDef::new(Alias::new("version")).string().not_null())
                    .col(
                        ColumnDef::new(Alias::new("definition"))
                            .json_binary()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("created_at"))
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(Alias::new("model_id"))
                            .col(Alias::new("version")),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("model_prices_model_fk")
                            .from(schema("model_prices"), Alias::new("model_id"))
                            .to(schema("model_catalog"), Alias::new("id"))
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(schema("serving_endpoints"))
                    .col(
                        ColumnDef::new(Alias::new("id"))
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("generation"))
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("definition"))
                            .json_binary()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("updated_at"))
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .check(Expr::col(Alias::new("generation")).gt(0))
                    .to_owned(),
            )
            .await?;
        // PostgreSQL trigger DDL only; business access remains SeaORM.
        manager.get_connection().execute_unprepared(
            "CREATE TRIGGER model_price_append_only BEFORE UPDATE OR DELETE ON xscope.model_prices FOR EACH ROW EXECUTE FUNCTION xscope.protect_operation_history();
             CREATE TRIGGER model_price_no_truncate BEFORE TRUNCATE ON xscope.model_prices FOR EACH STATEMENT EXECUTE FUNCTION xscope.protect_operation_history();"
        ).await?;
        Ok(())
    }
    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "model price evidence must not be dropped".into(),
        ))
    }
}
