use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
#[allow(clippy::too_many_lines)]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // SeaQuery does not currently expose CREATE SCHEMA, so keep this single
        // bootstrap statement inside the migration boundary. All tables,
        // indexes, constraints and application queries use SeaORM/SeaQuery.
        manager
            .get_connection()
            .execute_unprepared("CREATE SCHEMA IF NOT EXISTS xscope")
            .await?;

        manager
            .create_table(
                Table::create()
                    .table((XscopeSchema::Xscope, Projects::Table))
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Projects::Id)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Projects::TenantId).string().not_null())
                    .col(ColumnDef::new(Projects::Name).string().not_null())
                    .col(
                        ColumnDef::new(Projects::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table((XscopeSchema::Xscope, ApiKeys::Table))
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ApiKeys::Id)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(ApiKeys::TenantId).string().not_null())
                    .col(ColumnDef::new(ApiKeys::ProjectId).string().not_null())
                    .col(ColumnDef::new(ApiKeys::Name).string().not_null())
                    .col(ColumnDef::new(ApiKeys::SecretHash).binary().not_null())
                    .col(
                        ColumnDef::new(ApiKeys::Scopes)
                            .array(ColumnType::String(StringLen::None))
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ApiKeys::AllowedModels)
                            .array(ColumnType::String(StringLen::None))
                            .not_null(),
                    )
                    .col(ColumnDef::new(ApiKeys::ExpiresAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(ApiKeys::RateLimitRpm).integer().not_null())
                    .col(
                        ColumnDef::new(ApiKeys::RateLimitTpm)
                            .big_integer()
                            .not_null()
                            .default(60_000),
                    )
                    .col(
                        ColumnDef::new(ApiKeys::MonthlyBudgetAmount)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(ApiKeys::Currency)
                            .string()
                            .not_null()
                            .default("CNY"),
                    )
                    .col(
                        ColumnDef::new(ApiKeys::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(ColumnDef::new(ApiKeys::RevokedAt).timestamp_with_time_zone())
                    .foreign_key(
                        ForeignKey::create()
                            .name("api_keys_project_fk")
                            .from((XscopeSchema::Xscope, ApiKeys::Table), ApiKeys::ProjectId)
                            .to((XscopeSchema::Xscope, Projects::Table), Projects::Id)
                            .on_update(ForeignKeyAction::Cascade)
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table((XscopeSchema::Xscope, UsageEvents::Table))
                    .if_not_exists()
                    .col(
                        ColumnDef::new(UsageEvents::EventId)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(UsageEvents::RequestId).string().not_null())
                    .col(
                        ColumnDef::new(UsageEvents::OccurredAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(UsageEvents::TenantId).string().not_null())
                    .col(ColumnDef::new(UsageEvents::ProjectId).string().not_null())
                    .col(ColumnDef::new(UsageEvents::ApiKeyId).string().not_null())
                    .col(ColumnDef::new(UsageEvents::ModelId).string().not_null())
                    .col(
                        ColumnDef::new(UsageEvents::ModelRevision)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(UsageEvents::EndpointId).string().not_null())
                    .col(ColumnDef::new(UsageEvents::Region).string().not_null())
                    .col(
                        ColumnDef::new(UsageEvents::PriceVersion)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UsageEvents::InputTokens)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UsageEvents::OutputTokens)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UsageEvents::CachedInputTokens)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UsageEvents::LatencyMs)
                            .big_integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(UsageEvents::Status).string().not_null())
                    .col(
                        ColumnDef::new(UsageEvents::CostAmount)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UsageEvents::CostMicrounits)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(UsageEvents::Currency)
                            .string()
                            .not_null()
                            .default("CNY"),
                    )
                    .col(
                        ColumnDef::new(UsageEvents::ReceivedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("usage_events_project_time_idx")
                    .table((XscopeSchema::Xscope, UsageEvents::Table))
                    .col(UsageEvents::ProjectId)
                    .col(UsageEvents::OccurredAt)
                    .if_not_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("usage_events_key_time_idx")
                    .table((XscopeSchema::Xscope, UsageEvents::Table))
                    .col(UsageEvents::ApiKeyId)
                    .col(UsageEvents::OccurredAt)
                    .if_not_exists()
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

#[derive(DeriveIden)]
enum XscopeSchema {
    Xscope,
}

#[derive(DeriveIden)]
enum Projects {
    Table,
    Id,
    TenantId,
    Name,
    CreatedAt,
}

#[derive(DeriveIden)]
enum ApiKeys {
    Table,
    Id,
    TenantId,
    ProjectId,
    Name,
    SecretHash,
    Scopes,
    AllowedModels,
    ExpiresAt,
    RateLimitRpm,
    RateLimitTpm,
    MonthlyBudgetAmount,
    Currency,
    CreatedAt,
    RevokedAt,
}

#[derive(DeriveIden)]
enum UsageEvents {
    Table,
    EventId,
    RequestId,
    OccurredAt,
    TenantId,
    ProjectId,
    ApiKeyId,
    ModelId,
    ModelRevision,
    EndpointId,
    Region,
    PriceVersion,
    InputTokens,
    OutputTokens,
    CachedInputTokens,
    LatencyMs,
    Status,
    CostAmount,
    CostMicrounits,
    Currency,
    ReceivedAt,
}
