use sea_orm_migration::prelude::*;
#[derive(DeriveMigrationName)]
pub struct Migration;
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = |name: &str| (Alias::new("xscope"), Alias::new(name));
        let mut jobs = Table::create();
        jobs.table(schema("billing_jobs"));
        for name in ["billing_account_id", "consumer", "state"] {
            jobs.col(ColumnDef::new(Alias::new(name)).string().not_null());
        }
        for name in [
            "acknowledged",
            "target_sequence",
            "lease_epoch",
            "lease_through",
        ] {
            jobs.col(ColumnDef::new(Alias::new(name)).big_integer().not_null());
        }
        jobs.col(ColumnDef::new(Alias::new("lease_token")).string())
            .col(ColumnDef::new(Alias::new("lease_until")).timestamp_with_time_zone())
            .col(ColumnDef::new(Alias::new("last_error")).string())
            .col(ColumnDef::new(Alias::new("attempts")).integer().not_null());
        for name in ["available_at", "updated_at"] {
            jobs.col(
                ColumnDef::new(Alias::new(name))
                    .timestamp_with_time_zone()
                    .not_null(),
            );
        }
        jobs.primary_key(
            Index::create()
                .col(Alias::new("billing_account_id"))
                .col(Alias::new("consumer")),
        )
        .check(Expr::col(Alias::new("acknowledged")).gte(0))
        .check(Expr::col(Alias::new("target_sequence")).gte(Expr::col(Alias::new("acknowledged"))))
        .check(Expr::col(Alias::new("state")).is_in(["pending", "running", "idle", "dead"]))
        .foreign_key(
            ForeignKey::create()
                .name("billing_job_account_fk")
                .from(schema("billing_jobs"), Alias::new("billing_account_id"))
                .to(schema("billing_accounts"), Alias::new("id"))
                .on_delete(ForeignKeyAction::Restrict),
        );
        manager.create_table(jobs).await?;
        manager
            .create_index(
                Index::create()
                    .name("billing_jobs_due_idx")
                    .table(schema("billing_jobs"))
                    .col(Alias::new("consumer"))
                    .col(Alias::new("state"))
                    .col(Alias::new("available_at"))
                    .to_owned(),
            )
            .await?;
        let mut effects = Table::create();
        effects.table(schema("billing_effects"));
        for name in ["billing_account_id", "consumer", "payload_sha256", "kind"] {
            effects.col(ColumnDef::new(Alias::new(name)).string().not_null());
        }
        effects
            .col(
                ColumnDef::new(Alias::new("sequence"))
                    .big_integer()
                    .not_null(),
            )
            .col(
                ColumnDef::new(Alias::new("processed_at"))
                    .timestamp_with_time_zone()
                    .not_null(),
            )
            .primary_key(
                Index::create()
                    .col(Alias::new("billing_account_id"))
                    .col(Alias::new("consumer"))
                    .col(Alias::new("sequence")),
            )
            .foreign_key(
                ForeignKey::create()
                    .name("billing_effect_event_fk")
                    .from(
                        schema("billing_effects"),
                        (Alias::new("billing_account_id"), Alias::new("sequence")),
                    )
                    .to(
                        schema("billing_events"),
                        (Alias::new("billing_account_id"), Alias::new("sequence")),
                    )
                    .on_delete(ForeignKeyAction::Restrict),
            );
        manager.create_table(effects).await?;
        let mut backfill = Query::insert();
        backfill.into_table(schema("billing_jobs")).columns(
            [
                "billing_account_id",
                "consumer",
                "target_sequence",
                "acknowledged",
                "lease_epoch",
                "lease_through",
                "available_at",
                "state",
                "attempts",
                "updated_at",
            ]
            .map(Alias::new),
        );
        backfill
            .select_from(
                Query::select()
                    .column(Alias::new("billing_account_id"))
                    .expr(Expr::val("ledger.audit.v1"))
                    .expr(Expr::col(Alias::new("sequence")).max())
                    .expr(Expr::val(0_i64))
                    .expr(Expr::val(0_i64))
                    .expr(Expr::val(0_i64))
                    .expr(Expr::current_timestamp())
                    .expr(Expr::val("pending"))
                    .expr(Expr::val(0_i32))
                    .expr(Expr::current_timestamp())
                    .from(schema("billing_events"))
                    .group_by_col(Alias::new("billing_account_id"))
                    .to_owned(),
            )
            .map_err(|e| DbErr::Custom(e.to_string()))?;
        manager.exec_stmt(backfill).await?;
        manager
            .create_table(
                Table::create()
                    .table(schema("operation_audits"))
                    .col(
                        ColumnDef::new(Alias::new("id"))
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Alias::new("actor_id")).string().not_null())
                    .col(ColumnDef::new(Alias::new("action")).string().not_null())
                    .col(
                        ColumnDef::new(Alias::new("resource_id"))
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("payload"))
                            .json_binary()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("created_at"))
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        Ok(())
    }
    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "worker/audit history requires an explicit archival migration".into(),
        ))
    }
}
