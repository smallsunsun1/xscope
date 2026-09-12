use sea_orm_migration::prelude::*;
#[derive(DeriveMigrationName)]
pub struct Migration;
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = |name: &str| (Alias::new("xscope"), Alias::new(name));
        let mut table = Table::create();
        table.table(schema("member_clusters"));
        table.col(
            ColumnDef::new(Alias::new("id"))
                .string()
                .not_null()
                .primary_key(),
        );
        for name in ["namespace", "credential_hash"] {
            table.col(ColumnDef::new(Alias::new(name)).string().not_null());
        }
        for name in [
            "credential_epoch",
            "desired_version",
            "acknowledged_version",
            "delivery_version",
        ] {
            table.col(ColumnDef::new(Alias::new(name)).big_integer().not_null());
        }
        for name in ["expires_at", "created_at", "updated_at"] {
            table.col(
                ColumnDef::new(Alias::new(name))
                    .timestamp_with_time_zone()
                    .not_null(),
            );
        }
        for name in ["revoked_at", "lease_until", "heartbeat_at"] {
            table.col(ColumnDef::new(Alias::new(name)).timestamp_with_time_zone());
        }
        for name in ["delivery_lease", "last_error"] {
            table.col(ColumnDef::new(Alias::new(name)).string());
        }
        table
            .check(Expr::col(Alias::new("acknowledged_version")).gte(0))
            .check(
                Expr::col(Alias::new("desired_version"))
                    .gte(Expr::col(Alias::new("acknowledged_version"))),
            );
        manager.create_table(table).await?;
        manager
            .create_table(
                Table::create()
                    .table(schema("cluster_revisions"))
                    .col(ColumnDef::new(Alias::new("cluster_id")).string().not_null())
                    .col(
                        ColumnDef::new(Alias::new("version"))
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("payload"))
                            .json_binary()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Alias::new("sha256")).string().not_null())
                    .col(ColumnDef::new(Alias::new("created_by")).string().not_null())
                    .col(
                        ColumnDef::new(Alias::new("created_at"))
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(Alias::new("cluster_id"))
                            .col(Alias::new("version")),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("cluster_revision_identity_fk")
                            .from(schema("cluster_revisions"), Alias::new("cluster_id"))
                            .to(schema("member_clusters"), Alias::new("id"))
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .to_owned(),
            )
            .await?;
        Ok(())
    }
    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "cluster identity and desired history require explicit archival migration".into(),
        ))
    }
}
