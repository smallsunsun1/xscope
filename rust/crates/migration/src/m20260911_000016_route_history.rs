use sea_orm_migration::prelude::*;
#[derive(DeriveMigrationName)]
pub struct Migration;
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = |name: &str| (Alias::new("xscope"), Alias::new(name));
        manager
            .create_table(
                Table::create()
                    .table(schema("route_revisions"))
                    .col(ColumnDef::new(Alias::new("project_id")).string().not_null())
                    .col(ColumnDef::new(Alias::new("model")).string().not_null())
                    .col(
                        ColumnDef::new(Alias::new("revision"))
                            .big_integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Alias::new("spec")).json_binary().not_null())
                    .col(ColumnDef::new(Alias::new("actor")).string().not_null())
                    .col(ColumnDef::new(Alias::new("operation")).string().not_null())
                    .col(
                        ColumnDef::new(Alias::new("created_at"))
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(Alias::new("project_id"))
                            .col(Alias::new("model"))
                            .col(Alias::new("revision")),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("route_revision_project_fk")
                            .from(schema("route_revisions"), Alias::new("project_id"))
                            .to(schema("projects"), Alias::new("id"))
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .to_owned(),
            )
            .await?;
        // Only the pre-migration current revision is known. Do not fabricate
        // earlier release history or attribute it to a real administrator.
        manager.get_connection().execute_unprepared(
            "INSERT INTO xscope.route_revisions (project_id,model,revision,spec,actor,operation,created_at)
             SELECT project_id,model,revision,spec,'migration','legacy_snapshot',updated_at FROM xscope.route_policies;
             CREATE TRIGGER route_history_append_only BEFORE UPDATE OR DELETE ON xscope.route_revisions FOR EACH ROW EXECUTE FUNCTION xscope.protect_operation_history();
             CREATE TRIGGER route_history_no_truncate BEFORE TRUNCATE ON xscope.route_revisions FOR EACH STATEMENT EXECUTE FUNCTION xscope.protect_operation_history();"
        ).await?;
        Ok(())
    }
    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom("release history must not be dropped".into()))
    }
}
