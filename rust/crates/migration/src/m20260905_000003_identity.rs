use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table((XscopeSchema::Xscope, PlatformUsers::Table))
                    .if_not_exists()
                    .col(
                        ColumnDef::new(PlatformUsers::Id)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(PlatformUsers::ExternalSubject)
                            .string()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(PlatformUsers::Username).string().not_null())
                    .col(ColumnDef::new(PlatformUsers::Email).string().not_null())
                    .col(
                        ColumnDef::new(PlatformUsers::Status)
                            .string()
                            .not_null()
                            .default("active"),
                    )
                    .col(timestamp_column(PlatformUsers::LastLoginAt))
                    .col(timestamp_column(PlatformUsers::CreatedAt))
                    .col(timestamp_column(PlatformUsers::UpdatedAt))
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table((XscopeSchema::Xscope, TenantMemberships::Table))
                    .if_not_exists()
                    .col(
                        ColumnDef::new(TenantMemberships::Id)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(TenantMemberships::UserId)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TenantMemberships::TenantId)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(TenantMemberships::Role).string().not_null())
                    .col(timestamp_column(TenantMemberships::CreatedAt))
                    .foreign_key(
                        ForeignKey::create()
                            .name("tenant_memberships_user_fk")
                            .from(
                                (XscopeSchema::Xscope, TenantMemberships::Table),
                                TenantMemberships::UserId,
                            )
                            .to(
                                (XscopeSchema::Xscope, PlatformUsers::Table),
                                PlatformUsers::Id,
                            )
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .index(
                        Index::create()
                            .name("tenant_memberships_user_tenant_key")
                            .col(TenantMemberships::UserId)
                            .col(TenantMemberships::TenantId)
                            .unique(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
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
enum PlatformUsers {
    Table,
    Id,
    ExternalSubject,
    Username,
    Email,
    Status,
    LastLoginAt,
    CreatedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum TenantMemberships {
    Table,
    Id,
    UserId,
    TenantId,
    Role,
    CreatedAt,
}
