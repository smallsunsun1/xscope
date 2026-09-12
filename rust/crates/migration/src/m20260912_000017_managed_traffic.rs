use sea_orm_migration::prelude::*;
#[derive(DeriveMigrationName)]
pub struct Migration;
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = |n: &str| (Alias::new("xscope"), Alias::new(n));
        manager
            .create_table(
                Table::create()
                    .table(schema("traffic_registry_guard"))
                    .col(
                        ColumnDef::new(Alias::new("id"))
                            .integer()
                            .primary_key()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .get_connection()
            .execute_unprepared("INSERT INTO xscope.traffic_registry_guard (id) VALUES (1)")
            .await?;
        let mut pool = Table::create();
        pool.table(schema("managed_pools")).col(
            ColumnDef::new(Alias::new("id"))
                .string()
                .primary_key()
                .not_null(),
        );
        for n in ["cluster_id", "deployment", "state", "observation_code"] {
            pool.col(ColumnDef::new(Alias::new(n)).string().not_null());
        }
        for n in ["generation", "desired_version"] {
            pool.col(ColumnDef::new(Alias::new(n)).big_integer().not_null());
        }
        for n in ["binding", "endpoint", "expected_spec"] {
            pool.col(ColumnDef::new(Alias::new(n)).json_binary().not_null());
        }
        for n in ["deployment_uid", "observation_nonce"] {
            pool.col(ColumnDef::new(Alias::new(n)).string());
        }
        for n in ["ready_until", "observation_until", "observed_at"] {
            pool.col(ColumnDef::new(Alias::new(n)).timestamp_with_time_zone());
        }
        pool.col(
            ColumnDef::new(Alias::new("updated_at"))
                .timestamp_with_time_zone()
                .not_null(),
        )
        .foreign_key(
            ForeignKey::create()
                .name("managed_pool_cluster_fk")
                .from(schema("managed_pools"), Alias::new("cluster_id"))
                .to(schema("member_clusters"), Alias::new("id"))
                .on_delete(ForeignKeyAction::Restrict),
        )
        .index(
            Index::create()
                .name("managed_deployment_identity")
                .unique()
                .col(Alias::new("cluster_id"))
                .col(Alias::new("deployment")),
        );
        manager.create_table(pool).await?;
        manager
            .create_table(
                Table::create()
                    .table(schema("gateway_sessions"))
                    .col(
                        ColumnDef::new(Alias::new("id"))
                            .string()
                            .primary_key()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("sequence"))
                            .big_integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Alias::new("closed")).boolean().not_null())
                    .col(ColumnDef::new(Alias::new("nonce")).string().not_null())
                    .col(ColumnDef::new(Alias::new("reported")).json_binary())
                    .col(
                        ColumnDef::new(Alias::new("delivered_routes"))
                            .json_binary()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("acknowledged_routes"))
                            .json_binary()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("delivered_at"))
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(schema("gateway_pool_views"))
                    .col(ColumnDef::new(Alias::new("pool_id")).string().not_null())
                    .col(ColumnDef::new(Alias::new("session_id")).string().not_null())
                    .col(
                        ColumnDef::new(Alias::new("delivered_generation"))
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("acknowledged_generation"))
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("active_requests"))
                            .big_integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Alias::new("retired")).boolean().not_null())
                    .col(ColumnDef::new(Alias::new("reported_at")).timestamp_with_time_zone())
                    .primary_key(
                        Index::create()
                            .col(Alias::new("pool_id"))
                            .col(Alias::new("session_id")),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("gateway_pool_fk")
                            .from(schema("gateway_pool_views"), Alias::new("pool_id"))
                            .to(schema("managed_pools"), Alias::new("id"))
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("gateway_session_fk")
                            .from(schema("gateway_pool_views"), Alias::new("session_id"))
                            .to(schema("gateway_sessions"), Alias::new("id"))
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("gateway_views_by_session")
                    .table(schema("gateway_pool_views"))
                    .col(Alias::new("session_id"))
                    .to_owned(),
            )
            .await?;
        Ok(())
    }
    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "traffic participants cannot be discarded as proof of drain".into(),
        ))
    }
}
