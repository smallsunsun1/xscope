use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table((Alias::new("xscope"), RoutePolicies::Table))
                    .col(ColumnDef::new(RoutePolicies::ProjectId).string().not_null())
                    .col(ColumnDef::new(RoutePolicies::Model).string().not_null())
                    .col(
                        ColumnDef::new(RoutePolicies::Revision)
                            .big_integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(RoutePolicies::Spec).json_binary().not_null())
                    .col(
                        ColumnDef::new(RoutePolicies::UpdatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(RoutePolicies::ProjectId)
                            .col(RoutePolicies::Model),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("route_policies_project_fk")
                            .from(
                                (Alias::new("xscope"), RoutePolicies::Table),
                                RoutePolicies::ProjectId,
                            )
                            .to(
                                (Alias::new("xscope"), Alias::new("projects")),
                                Alias::new("id"),
                            )
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

#[derive(DeriveIden)]
enum RoutePolicies {
    Table,
    ProjectId,
    Model,
    Revision,
    Spec,
    UpdatedAt,
}
