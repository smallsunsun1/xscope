use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let mut reservation = Table::create();
        reservation.table((Alias::new("xscope"), Alias::new("billing_reservations")));
        for name in [
            "id",
            "billing_account_id",
            "project_id",
            "tenant_id",
            "api_key_id",
            "request_id",
            "state",
        ] {
            reservation.col(ColumnDef::new(Alias::new(name)).string().not_null());
        }
        reservation.primary_key(Index::create().col(Alias::new("id")));
        reservation.index(
            Index::create()
                .name("reservation_request_unique")
                .unique()
                .col(Alias::new("project_id"))
                .col(Alias::new("request_id")),
        );
        for name in ["spec", "price"] {
            reservation.col(ColumnDef::new(Alias::new(name)).json_binary().not_null());
        }
        reservation.col(ColumnDef::new(Alias::new("completion")).json_binary());
        reservation.col(
            ColumnDef::new(Alias::new("reserved_microunits"))
                .big_integer()
                .not_null(),
        );
        reservation.col(ColumnDef::new(Alias::new("settled_microunits")).big_integer());
        for name in ["created_at", "updated_at"] {
            reservation.col(
                ColumnDef::new(Alias::new(name))
                    .timestamp_with_time_zone()
                    .not_null(),
            );
        }
        reservation.foreign_key(
            ForeignKey::create()
                .name("reservation_account_fk")
                .from(
                    (Alias::new("xscope"), Alias::new("billing_reservations")),
                    Alias::new("billing_account_id"),
                )
                .to(
                    (Alias::new("xscope"), Alias::new("billing_accounts")),
                    Alias::new("id"),
                )
                .on_delete(ForeignKeyAction::Restrict),
        );
        manager.create_table(reservation).await?;
        manager
            .create_index(
                Index::create()
                    .name("reservation_key_state_idx")
                    .table((Alias::new("xscope"), Alias::new("billing_reservations")))
                    .col(Alias::new("api_key_id"))
                    .col(Alias::new("state"))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("usage_events_project_request_idx")
                    .table((Alias::new("xscope"), Alias::new("usage_events")))
                    .col(Alias::new("project_id"))
                    .col(Alias::new("request_id"))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("reservation_account_state_idx")
                    .table((Alias::new("xscope"), Alias::new("billing_reservations")))
                    .col(Alias::new("billing_account_id"))
                    .col(Alias::new("state"))
                    .to_owned(),
            )
            .await?;

        let mut event = Table::create();
        event
            .table((Alias::new("xscope"), Alias::new("billing_events")))
            .col(
                ColumnDef::new(Alias::new("billing_account_id"))
                    .string()
                    .not_null(),
            )
            .col(
                ColumnDef::new(Alias::new("sequence"))
                    .big_integer()
                    .not_null(),
            )
            .col(ColumnDef::new(Alias::new("kind")).string().not_null())
            .col(
                ColumnDef::new(Alias::new("aggregate_id"))
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
            .primary_key(
                Index::create()
                    .col(Alias::new("billing_account_id"))
                    .col(Alias::new("sequence")),
            )
            .foreign_key(
                ForeignKey::create()
                    .name("billing_event_account_fk")
                    .from(
                        (Alias::new("xscope"), Alias::new("billing_events")),
                        Alias::new("billing_account_id"),
                    )
                    .to(
                        (Alias::new("xscope"), Alias::new("billing_accounts")),
                        Alias::new("id"),
                    )
                    .on_delete(ForeignKeyAction::Restrict),
            );
        manager.create_table(event).await?;
        manager
            .create_table(
                Table::create()
                    .table((Alias::new("xscope"), Alias::new("billing_consumers")))
                    .col(
                        ColumnDef::new(Alias::new("billing_account_id"))
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Alias::new("consumer")).string().not_null())
                    .col(
                        ColumnDef::new(Alias::new("acknowledged"))
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("delivered"))
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Alias::new("updated_at"))
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(Alias::new("billing_account_id"))
                            .col(Alias::new("consumer")),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("billing_consumer_account_fk")
                            .from(
                                (Alias::new("xscope"), Alias::new("billing_consumers")),
                                Alias::new("billing_account_id"),
                            )
                            .to(
                                (Alias::new("xscope"), Alias::new("billing_accounts")),
                                Alias::new("id"),
                            )
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Accounting evidence is never silently erased by rollback.
        Ok(())
    }
}
