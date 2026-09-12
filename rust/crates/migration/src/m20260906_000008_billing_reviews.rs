use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = |name: &str| (Alias::new("xscope"), Alias::new(name));
        let mut reviews = Table::create();
        reviews.table(schema("billing_reviews"));
        for name in [
            "id",
            "reservation_id",
            "project_id",
            "submitted_by",
            "evidence_sha256",
            "state",
        ] {
            let mut column = ColumnDef::new(Alias::new(name));
            column.string().not_null();
            if name == "id" {
                column.primary_key();
            }
            reviews.col(&mut column);
        }
        reviews
            .col(
                ColumnDef::new(Alias::new("evidence"))
                    .json_binary()
                    .not_null(),
            )
            .col(ColumnDef::new(Alias::new("reviewed_by")).string())
            .col(ColumnDef::new(Alias::new("decision")).json_binary())
            .col(
                ColumnDef::new(Alias::new("created_at"))
                    .timestamp_with_time_zone()
                    .not_null(),
            )
            .col(
                ColumnDef::new(Alias::new("updated_at"))
                    .timestamp_with_time_zone()
                    .not_null(),
            )
            .check(Expr::col(Alias::new("state")).is_in(["submitted", "rejected", "settled"]))
            .foreign_key(
                ForeignKey::create()
                    .name("billing_review_reservation_fk")
                    .from(schema("billing_reviews"), Alias::new("reservation_id"))
                    .to(schema("billing_reservations"), Alias::new("id"))
                    .on_delete(ForeignKeyAction::Restrict),
            );
        manager.create_table(reviews).await?;
        manager
            .create_index(
                Index::create()
                    .name("billing_reviews_reservation_page_idx")
                    .table(schema("billing_reviews"))
                    .col(Alias::new("reservation_id"))
                    .col(Alias::new("id"))
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(schema("billing_review_audit"))
                    .col(ColumnDef::new(Alias::new("review_id")).string().not_null())
                    .col(ColumnDef::new(Alias::new("sequence")).integer().not_null())
                    .col(ColumnDef::new(Alias::new("kind")).string().not_null())
                    .col(ColumnDef::new(Alias::new("actor_id")).string().not_null())
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
                            .col(Alias::new("review_id"))
                            .col(Alias::new("sequence")),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("billing_review_audit_parent_fk")
                            .from(schema("billing_review_audit"), Alias::new("review_id"))
                            .to(schema("billing_reviews"), Alias::new("id"))
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .to_owned(),
            )
            .await?;
        // PostgreSQL-specific DDL only; all business reads/writes use SeaORM.
        // Protect evidence/audit from ordinary UPDATE/DELETE/TRUNCATE, not a
        // database administrator capable of disabling triggers or dropping tables.
        manager
            .get_connection()
            .execute_unprepared(
                r#"
CREATE FUNCTION xscope.protect_billing_review() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP <> 'UPDATE' THEN RAISE EXCEPTION 'billing evidence cannot be removed'; END IF;
  IF OLD.state <> 'submitted' OR NEW.state NOT IN ('rejected','settled') OR
     NEW.reviewed_by IS NULL OR NEW.reviewed_by = OLD.submitted_by OR NEW.decision IS NULL OR
     (to_jsonb(OLD) - ARRAY['state','reviewed_by','decision','updated_at']) IS DISTINCT FROM
     (to_jsonb(NEW) - ARRAY['state','reviewed_by','decision','updated_at']) THEN
    RAISE EXCEPTION 'immutable billing evidence or invalid decision';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER billing_review_immutable BEFORE UPDATE OR DELETE ON xscope.billing_reviews
FOR EACH ROW EXECUTE FUNCTION xscope.protect_billing_review();
CREATE TRIGGER billing_review_no_truncate BEFORE TRUNCATE ON xscope.billing_reviews
FOR EACH STATEMENT EXECUTE FUNCTION xscope.protect_billing_review();
CREATE FUNCTION xscope.protect_billing_review_audit() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'billing review audit is append only'; END $$;
CREATE TRIGGER billing_review_audit_immutable BEFORE UPDATE OR DELETE ON xscope.billing_review_audit
FOR EACH ROW EXECUTE FUNCTION xscope.protect_billing_review_audit();
CREATE TRIGGER billing_review_audit_no_truncate BEFORE TRUNCATE ON xscope.billing_review_audit
FOR EACH STATEMENT EXECUTE FUNCTION xscope.protect_billing_review_audit();
"#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "billing evidence/audit cannot be automatically removed".into(),
        ))
    }
}
