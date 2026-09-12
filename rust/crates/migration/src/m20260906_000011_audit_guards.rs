use sea_orm_migration::prelude::*;
#[derive(DeriveMigrationName)]
pub struct Migration;
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // PostgreSQL-specific trigger DDL, not a business query. Application
        // records use SeaORM. Administrators can still disable database guards.
        manager
            .get_connection()
            .execute_unprepared(
                r#"
CREATE FUNCTION xscope.protect_operation_history() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'operation history is append only'; END $$;
CREATE TRIGGER operation_audit_append_only BEFORE UPDATE OR DELETE ON xscope.operation_audits
FOR EACH ROW EXECUTE FUNCTION xscope.protect_operation_history();
CREATE TRIGGER operation_audit_no_truncate BEFORE TRUNCATE ON xscope.operation_audits
FOR EACH STATEMENT EXECUTE FUNCTION xscope.protect_operation_history();
CREATE TRIGGER billing_effect_append_only BEFORE UPDATE OR DELETE ON xscope.billing_effects
FOR EACH ROW EXECUTE FUNCTION xscope.protect_operation_history();
CREATE TRIGGER billing_effect_no_truncate BEFORE TRUNCATE ON xscope.billing_effects
FOR EACH STATEMENT EXECUTE FUNCTION xscope.protect_operation_history();
CREATE TRIGGER cluster_revision_append_only BEFORE UPDATE OR DELETE ON xscope.cluster_revisions
FOR EACH ROW EXECUTE FUNCTION xscope.protect_operation_history();
CREATE TRIGGER cluster_revision_no_truncate BEFORE TRUNCATE ON xscope.cluster_revisions
FOR EACH STATEMENT EXECUTE FUNCTION xscope.protect_operation_history();
"#,
            )
            .await?;
        Ok(())
    }
    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "append-only guards require an explicit maintenance migration".into(),
        ))
    }
}
