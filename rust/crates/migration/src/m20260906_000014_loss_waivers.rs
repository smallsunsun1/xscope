use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // PostgreSQL DDL only. Existing evidence and audits remain immutable;
        // all monetary transitions and reads continue to use SeaORM.
        manager
            .get_connection()
            .execute_unprepared(
                r#"
CREATE INDEX reservation_pending_monitor_idx ON xscope.billing_reservations (state, created_at)
WHERE state IN ('reserved','dispatched');
ALTER TABLE xscope.billing_reviews DROP CONSTRAINT billing_reviews_state_check;
ALTER TABLE xscope.billing_reviews ADD CONSTRAINT billing_reviews_state_check
CHECK (state IN ('submitted','rejected','settled','waived'));
CREATE OR REPLACE FUNCTION xscope.protect_billing_review() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP <> 'UPDATE' THEN RAISE EXCEPTION 'billing evidence cannot be removed'; END IF;
  IF OLD.state <> 'submitted' OR NEW.state NOT IN ('rejected','settled','waived') OR
     NEW.reviewed_by IS NULL OR NEW.reviewed_by = OLD.submitted_by OR NEW.decision IS NULL OR
     (NEW.state = 'waived' AND COALESCE(OLD.evidence->>'kind','') <> 'loss_waiver') OR
     (NEW.state = 'settled' AND OLD.evidence->>'kind' = 'loss_waiver') OR
     (to_jsonb(OLD) - ARRAY['state','reviewed_by','decision','updated_at']) IS DISTINCT FROM
     (to_jsonb(NEW) - ARRAY['state','reviewed_by','decision','updated_at']) THEN
    RAISE EXCEPTION 'immutable billing evidence or invalid decision';
  END IF;
  RETURN NEW;
END $$;
"#,
            )
            .await?;
        Ok(())
    }
    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "loss waiver audit cannot be automatically removed".into(),
        ))
    }
}
