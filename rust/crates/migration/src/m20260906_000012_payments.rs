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
                    .table(schema("payment_checkouts"))
                    .col(
                        ColumnDef::new(Alias::new("order_id"))
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Alias::new("profile")).string().not_null())
                    .col(
                        ColumnDef::new(Alias::new("environment"))
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Alias::new("state")).string().not_null())
                    .col(ColumnDef::new(Alias::new("qr_code")).text())
                    .col(
                        ColumnDef::new(Alias::new("created_at"))
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("updated_at"))
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("payment_checkout_order_fk")
                            .from(schema("payment_checkouts"), Alias::new("order_id"))
                            .to(schema("billing_orders"), Alias::new("id"))
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .check(
                        Expr::col(Alias::new("state"))
                            .is_in(["creating", "pending", "paid", "closed"]),
                    )
                    .to_owned(),
            )
            .await?;
        let mut table = Table::create();
        table.table(schema("provider_receipts"));
        table.col(
            ColumnDef::new(Alias::new("id"))
                .string()
                .not_null()
                .primary_key(),
        );
        for field in ["order_id", "profile", "trade_id", "state", "proof_sha256"] {
            table.col(ColumnDef::new(Alias::new(field)).string().not_null());
        }
        table
            .col(
                ColumnDef::new(Alias::new("evidence"))
                    .json_binary()
                    .not_null(),
            )
            .col(
                ColumnDef::new(Alias::new("amount_minor"))
                    .big_integer()
                    .not_null(),
            )
            .col(
                ColumnDef::new(Alias::new("created_at"))
                    .timestamp_with_time_zone()
                    .not_null(),
            )
            .foreign_key(
                ForeignKey::create()
                    .name("provider_receipt_order_fk")
                    .from(schema("provider_receipts"), Alias::new("order_id"))
                    .to(schema("payment_checkouts"), Alias::new("order_id"))
                    .on_delete(ForeignKeyAction::Restrict),
            );
        manager.create_table(table).await?;
        manager
            .get_connection()
            .execute_unprepared(
                r#"
CREATE TRIGGER provider_receipt_append_only BEFORE UPDATE OR DELETE ON xscope.provider_receipts
FOR EACH ROW EXECUTE FUNCTION xscope.protect_operation_history();
CREATE TRIGGER provider_receipt_no_truncate BEFORE TRUNCATE ON xscope.provider_receipts
FOR EACH STATEMENT EXECUTE FUNCTION xscope.protect_operation_history();
"#,
            )
            .await?;
        Ok(())
    }
    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "provider evidence requires explicit archival migration".into(),
        ))
    }
}
