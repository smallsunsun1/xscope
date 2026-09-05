use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Empty derived tables only. Backfill is lazy and account-locked in
        // the application. Cutover MUST exclude old non-projecting writers.
        for (name, key, parent, values, monthly) in [
            (
                "billing_balances",
                "billing_account_id",
                "billing_accounts",
                vec!["balance_microunits", "held_microunits"],
                false,
            ),
            (
                "billing_key_holds",
                "api_key_id",
                "api_keys",
                vec!["held_microunits"],
                false,
            ),
            (
                "billing_month_spend",
                "api_key_id",
                "api_keys",
                vec!["spent_microunits"],
                true,
            ),
        ] {
            let mut table = Table::create();
            table
                .table((Alias::new("xscope"), Alias::new(name)))
                .col(ColumnDef::new(Alias::new(key)).string().not_null());
            let mut pk = Index::create();
            pk.col(Alias::new(key));
            if monthly {
                table.col(ColumnDef::new(Alias::new("month")).date().not_null());
                pk.col(Alias::new("month"));
            }
            table.primary_key(&mut pk);
            for value in values {
                table.col(ColumnDef::new(Alias::new(value)).big_integer().not_null());
                if value != "balance_microunits" {
                    table.check(Expr::col(Alias::new(value)).gte(0));
                }
            }
            table
                .col(
                    ColumnDef::new(Alias::new("updated_at"))
                        .timestamp_with_time_zone()
                        .not_null(),
                )
                .foreign_key(
                    ForeignKey::create()
                        .name(format!("{name}_parent_fk"))
                        .from((Alias::new("xscope"), Alias::new(name)), Alias::new(key))
                        .to((Alias::new("xscope"), Alias::new(parent)), Alias::new("id"))
                        .on_delete(ForeignKeyAction::Restrict),
                );
            manager.create_table(table).await?;
        }
        manager
            .create_index(
                Index::create()
                    .name("billing_month_period_idx")
                    .table((Alias::new("xscope"), Alias::new("billing_month_spend")))
                    .col(Alias::new("month"))
                    .col(Alias::new("api_key_id"))
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Downgrading to non-projecting writers requires an explicit cutover;
        // do not silently remove accounting evidence or derived state.
        Ok(())
    }
}
