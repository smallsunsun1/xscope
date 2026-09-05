use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_index(
                Index::create()
                    .name("reservation_pending_page_idx")
                    .table((Alias::new("xscope"), Alias::new("billing_reservations")))
                    .col(Alias::new("billing_account_id"))
                    .col(Alias::new("state"))
                    .col(Alias::new("created_at"))
                    .col(Alias::new("id"))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Never delete financial history on application rollback.
        Ok(())
    }
}
