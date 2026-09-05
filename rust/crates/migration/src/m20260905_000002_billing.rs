use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        create_billing_accounts(manager).await?;
        create_orders(manager).await?;
        create_payments(manager).await?;
        create_refunds(manager).await?;
        create_ledger(manager).await?;
        create_invoices(manager).await?;
        create_idempotency_records(manager).await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

async fn create_billing_accounts(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .create_table(
            Table::create()
                .table((XscopeSchema::Xscope, BillingAccounts::Table))
                .if_not_exists()
                .col(
                    ColumnDef::new(BillingAccounts::Id)
                        .string()
                        .not_null()
                        .primary_key(),
                )
                .col(
                    ColumnDef::new(BillingAccounts::TenantId)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(BillingAccounts::ProjectId)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(BillingAccounts::Currency)
                        .string()
                        .not_null()
                        .default("CNY"),
                )
                .col(
                    ColumnDef::new(BillingAccounts::EnforceBalance)
                        .boolean()
                        .not_null()
                        .default(false),
                )
                .col(
                    ColumnDef::new(BillingAccounts::Status)
                        .string()
                        .not_null()
                        .default("active"),
                )
                .col(timestamp_column(BillingAccounts::CreatedAt))
                .col(timestamp_column(BillingAccounts::UpdatedAt))
                .foreign_key(
                    ForeignKey::create()
                        .name("billing_accounts_project_fk")
                        .from(
                            (XscopeSchema::Xscope, BillingAccounts::Table),
                            BillingAccounts::ProjectId,
                        )
                        .to((XscopeSchema::Xscope, Projects::Table), Projects::Id)
                        .on_update(ForeignKeyAction::Cascade)
                        .on_delete(ForeignKeyAction::Restrict),
                )
                .index(
                    Index::create()
                        .name("billing_accounts_project_key")
                        .col(BillingAccounts::ProjectId)
                        .unique(),
                )
                .to_owned(),
        )
        .await
}

async fn create_orders(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .create_table(
            Table::create()
                .table((XscopeSchema::Xscope, BillingOrders::Table))
                .if_not_exists()
                .col(string_pk(BillingOrders::Id))
                .col(ColumnDef::new(BillingOrders::TenantId).string().not_null())
                .col(ColumnDef::new(BillingOrders::ProjectId).string().not_null())
                .col(ColumnDef::new(BillingOrders::Kind).string().not_null())
                .col(
                    ColumnDef::new(BillingOrders::Amount)
                        .big_integer()
                        .not_null(),
                )
                .col(ColumnDef::new(BillingOrders::Currency).string().not_null())
                .col(ColumnDef::new(BillingOrders::Status).string().not_null())
                .col(
                    ColumnDef::new(BillingOrders::Description)
                        .string()
                        .not_null()
                        .default(""),
                )
                .col(timestamp_column(BillingOrders::CreatedAt))
                .col(timestamp_column(BillingOrders::UpdatedAt))
                .foreign_key(
                    ForeignKey::create()
                        .name("billing_orders_project_fk")
                        .from(
                            (XscopeSchema::Xscope, BillingOrders::Table),
                            BillingOrders::ProjectId,
                        )
                        .to((XscopeSchema::Xscope, Projects::Table), Projects::Id)
                        .on_delete(ForeignKeyAction::Restrict),
                )
                .to_owned(),
        )
        .await
}

async fn create_payments(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .create_table(
            Table::create()
                .table((XscopeSchema::Xscope, Payments::Table))
                .if_not_exists()
                .col(string_pk(Payments::Id))
                .col(ColumnDef::new(Payments::OrderId).string().not_null())
                .col(ColumnDef::new(Payments::Provider).string().not_null())
                .col(
                    ColumnDef::new(Payments::ProviderReference)
                        .string()
                        .not_null(),
                )
                .col(ColumnDef::new(Payments::Amount).big_integer().not_null())
                .col(ColumnDef::new(Payments::Currency).string().not_null())
                .col(ColumnDef::new(Payments::Status).string().not_null())
                .col(ColumnDef::new(Payments::PaidAt).timestamp_with_time_zone())
                .col(timestamp_column(Payments::CreatedAt))
                .foreign_key(
                    ForeignKey::create()
                        .name("payments_order_fk")
                        .from((XscopeSchema::Xscope, Payments::Table), Payments::OrderId)
                        .to(
                            (XscopeSchema::Xscope, BillingOrders::Table),
                            BillingOrders::Id,
                        )
                        .on_delete(ForeignKeyAction::Restrict),
                )
                .index(
                    Index::create()
                        .name("payments_provider_reference_key")
                        .col(Payments::Provider)
                        .col(Payments::ProviderReference)
                        .unique(),
                )
                .to_owned(),
        )
        .await
}

async fn create_refunds(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .create_table(
            Table::create()
                .table((XscopeSchema::Xscope, Refunds::Table))
                .if_not_exists()
                .col(string_pk(Refunds::Id))
                .col(ColumnDef::new(Refunds::PaymentId).string().not_null())
                .col(ColumnDef::new(Refunds::Amount).big_integer().not_null())
                .col(ColumnDef::new(Refunds::Currency).string().not_null())
                .col(ColumnDef::new(Refunds::Reason).string().not_null())
                .col(ColumnDef::new(Refunds::Status).string().not_null())
                .col(ColumnDef::new(Refunds::ProviderReference).string())
                .col(timestamp_column(Refunds::CreatedAt))
                .col(ColumnDef::new(Refunds::CompletedAt).timestamp_with_time_zone())
                .foreign_key(
                    ForeignKey::create()
                        .name("refunds_payment_fk")
                        .from((XscopeSchema::Xscope, Refunds::Table), Refunds::PaymentId)
                        .to((XscopeSchema::Xscope, Payments::Table), Payments::Id)
                        .on_delete(ForeignKeyAction::Restrict),
                )
                .to_owned(),
        )
        .await
}

#[allow(clippy::too_many_lines)]
async fn create_ledger(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .create_table(
            Table::create()
                .table((XscopeSchema::Xscope, LedgerTransactions::Table))
                .if_not_exists()
                .col(string_pk(LedgerTransactions::Id))
                .col(
                    ColumnDef::new(LedgerTransactions::TenantId)
                        .string()
                        .not_null(),
                )
                .col(ColumnDef::new(LedgerTransactions::Kind).string().not_null())
                .col(
                    ColumnDef::new(LedgerTransactions::ReferenceType)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(LedgerTransactions::ReferenceId)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(LedgerTransactions::IdempotencyKey)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(LedgerTransactions::Currency)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(LedgerTransactions::Description)
                        .string()
                        .not_null(),
                )
                .col(timestamp_column(LedgerTransactions::CreatedAt))
                .index(
                    Index::create()
                        .name("ledger_transactions_idempotency_key")
                        .col(LedgerTransactions::IdempotencyKey)
                        .unique(),
                )
                .to_owned(),
        )
        .await?;

    manager
        .create_table(
            Table::create()
                .table((XscopeSchema::Xscope, LedgerEntries::Table))
                .if_not_exists()
                .col(string_pk(LedgerEntries::Id))
                .col(
                    ColumnDef::new(LedgerEntries::TransactionId)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(LedgerEntries::BillingAccountId)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(LedgerEntries::LedgerAccount)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(LedgerEntries::AmountMicrounits)
                        .big_integer()
                        .not_null(),
                )
                .col(ColumnDef::new(LedgerEntries::Currency).string().not_null())
                .col(timestamp_column(LedgerEntries::CreatedAt))
                .foreign_key(
                    ForeignKey::create()
                        .name("ledger_entries_transaction_fk")
                        .from(
                            (XscopeSchema::Xscope, LedgerEntries::Table),
                            LedgerEntries::TransactionId,
                        )
                        .to(
                            (XscopeSchema::Xscope, LedgerTransactions::Table),
                            LedgerTransactions::Id,
                        )
                        .on_delete(ForeignKeyAction::Restrict),
                )
                .foreign_key(
                    ForeignKey::create()
                        .name("ledger_entries_billing_account_fk")
                        .from(
                            (XscopeSchema::Xscope, LedgerEntries::Table),
                            LedgerEntries::BillingAccountId,
                        )
                        .to(
                            (XscopeSchema::Xscope, BillingAccounts::Table),
                            BillingAccounts::Id,
                        )
                        .on_delete(ForeignKeyAction::Restrict),
                )
                .to_owned(),
        )
        .await?;

    manager
        .create_index(
            Index::create()
                .name("ledger_entries_account_created_idx")
                .table((XscopeSchema::Xscope, LedgerEntries::Table))
                .col(LedgerEntries::BillingAccountId)
                .col(LedgerEntries::CreatedAt)
                .if_not_exists()
                .to_owned(),
        )
        .await
}

async fn create_invoices(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .create_table(
            Table::create()
                .table((XscopeSchema::Xscope, Invoices::Table))
                .if_not_exists()
                .col(string_pk(Invoices::Id))
                .col(ColumnDef::new(Invoices::TenantId).string().not_null())
                .col(ColumnDef::new(Invoices::ProjectId).string().not_null())
                .col(
                    ColumnDef::new(Invoices::PeriodStart)
                        .timestamp_with_time_zone()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(Invoices::PeriodEnd)
                        .timestamp_with_time_zone()
                        .not_null(),
                )
                .col(ColumnDef::new(Invoices::Amount).big_integer().not_null())
                .col(ColumnDef::new(Invoices::Currency).string().not_null())
                .col(ColumnDef::new(Invoices::Status).string().not_null())
                .col(ColumnDef::new(Invoices::Title).string().not_null())
                .col(timestamp_column(Invoices::IssuedAt))
                .foreign_key(
                    ForeignKey::create()
                        .name("invoices_project_fk")
                        .from((XscopeSchema::Xscope, Invoices::Table), Invoices::ProjectId)
                        .to((XscopeSchema::Xscope, Projects::Table), Projects::Id)
                        .on_delete(ForeignKeyAction::Restrict),
                )
                .to_owned(),
        )
        .await
}

async fn create_idempotency_records(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .create_table(
            Table::create()
                .table((XscopeSchema::Xscope, IdempotencyRecords::Table))
                .if_not_exists()
                .col(
                    ColumnDef::new(IdempotencyRecords::Key)
                        .string()
                        .not_null()
                        .primary_key(),
                )
                .col(
                    ColumnDef::new(IdempotencyRecords::Operation)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(IdempotencyRecords::RequestHash)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(IdempotencyRecords::ResponseStatus)
                        .integer()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(IdempotencyRecords::ResponseBody)
                        .json_binary()
                        .not_null(),
                )
                .col(timestamp_column(IdempotencyRecords::CreatedAt))
                .col(
                    ColumnDef::new(IdempotencyRecords::ExpiresAt)
                        .timestamp_with_time_zone()
                        .not_null(),
                )
                .to_owned(),
        )
        .await
}

fn string_pk<T: IntoIden>(name: T) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.string().not_null().primary_key();
    column
}

fn timestamp_column<T: IntoIden>(name: T) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column
        .timestamp_with_time_zone()
        .not_null()
        .default(Expr::current_timestamp());
    column
}

#[derive(DeriveIden)]
enum XscopeSchema {
    Xscope,
}

#[derive(DeriveIden)]
enum Projects {
    Table,
    Id,
}

#[derive(DeriveIden)]
enum BillingAccounts {
    Table,
    Id,
    TenantId,
    ProjectId,
    Currency,
    EnforceBalance,
    Status,
    CreatedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum BillingOrders {
    Table,
    Id,
    TenantId,
    ProjectId,
    Kind,
    Amount,
    Currency,
    Status,
    Description,
    CreatedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum Payments {
    Table,
    Id,
    OrderId,
    Provider,
    ProviderReference,
    Amount,
    Currency,
    Status,
    PaidAt,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Refunds {
    Table,
    Id,
    PaymentId,
    Amount,
    Currency,
    Reason,
    Status,
    ProviderReference,
    CreatedAt,
    CompletedAt,
}

#[derive(DeriveIden)]
enum LedgerTransactions {
    Table,
    Id,
    TenantId,
    Kind,
    ReferenceType,
    ReferenceId,
    IdempotencyKey,
    Currency,
    Description,
    CreatedAt,
}

#[derive(DeriveIden)]
enum LedgerEntries {
    Table,
    Id,
    TransactionId,
    BillingAccountId,
    LedgerAccount,
    AmountMicrounits,
    Currency,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Invoices {
    Table,
    Id,
    TenantId,
    ProjectId,
    PeriodStart,
    PeriodEnd,
    Amount,
    Currency,
    Status,
    Title,
    IssuedAt,
}

#[derive(DeriveIden)]
enum IdempotencyRecords {
    Table,
    Key,
    Operation,
    RequestHash,
    ResponseStatus,
    ResponseBody,
    CreatedAt,
    ExpiresAt,
}
