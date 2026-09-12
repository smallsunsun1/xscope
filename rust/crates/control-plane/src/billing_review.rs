//! Evidence settlement or explicit two-person loss waiver. No timeout release.
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use xscope_domain::billing::SettleRequest;
use xscope_entities::{
    billing_reservation as reservation, billing_review as review, billing_review_audit as audit,
};

use crate::{
    billing::{append_event, conflict, identifier, invalid, lock_account, settle_locked},
    error::{ServiceError, ServiceResult},
    repository::Repository,
};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRequest {
    pub id: String,
    /// Identifies the provider/runtime producing the evidence, not an arbitrary URL.
    pub source: String,
    pub source_request_id: String,
    /// Original final usage receipt/log, UTF-8 and bounded. Never fetched from a URL.
    pub document: String,
    pub explanation: String,
    pub usage: SettleRequest,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRequest {
    pub evidence_sha256: String,
    pub action: String,
    pub reason: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaiverRequest {
    pub id: String,
    pub reason: String,
    /// An incident/ticket identity, not an arbitrary URL or invented usage receipt.
    pub incident_reference: String,
    pub request_terminated: bool,
    pub platform_absorbs_loss: bool,
}

fn validate_waiver(request: &WaiverRequest) -> ServiceResult<()> {
    if !identifier(&request.id)
        || !identifier(&request.incident_reference)
        || request.reason.trim().is_empty()
        || request.reason.len() > 2048
        || !request.request_terminated
        || !request.platform_absorbs_loss
    {
        return Err(invalid(
            "loss waiver requires an incident, reason, terminated request and explicit platform loss acceptance",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {
    pub after_id: Option<String>,
    pub limit: Option<u64>,
}

fn validate_evidence(request: &EvidenceRequest) -> ServiceResult<()> {
    if !identifier(&request.id)
        || !identifier(&request.source)
        || !identifier(&request.source_request_id)
        || request.document.trim().is_empty()
        || request.document.len() > 65_536
        || request.explanation.trim().is_empty()
        || request.explanation.len() > 2_048
        || request.usage.input_tokens < 0
        || request.usage.output_tokens < 0
        || request.usage.latency_ms < 0
        || !identifier(&request.usage.endpoint_id)
        || !identifier(&request.usage.region)
        || !["succeeded", "cancelled", "provider_error"].contains(&request.usage.status.as_str())
    {
        return Err(invalid(
            "bounded original evidence, explanation and known final usage are required",
        ));
    }
    Ok(())
}

fn fingerprint(value: &Value) -> ServiceResult<String> {
    // Hash the complete normalized submission, including target-bound receipt
    // identity and proposed usage, not only a caller-provided document hash.
    Ok(format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(value).map_err(|e| ServiceError::Internal(e.to_string()))?
        )
    ))
}

fn summary(row: &review::Model) -> Value {
    json!({"id":row.id,"reservation_id":row.reservation_id,"project_id":row.project_id,
        "kind":row.evidence.get("kind").and_then(Value::as_str).unwrap_or("usage_evidence"),
        "submitted_by":row.submitted_by,"reviewed_by":row.reviewed_by,"state":row.state,
        "evidence_sha256":row.evidence_sha256,"created_at":row.created_at,"updated_at":row.updated_at,
        "decision":row.decision})
}

async fn append_audit(
    tx: &DatabaseTransaction,
    row: &review::Model,
    sequence: i32,
    actor: &str,
    subject: &str,
    payload: Value,
) -> ServiceResult<()> {
    audit::ActiveModel {
        review_id: Set(row.id.clone()),
        sequence: Set(sequence),
        kind: Set(row.state.clone()),
        actor_id: Set(actor.into()),
        payload: Set(
            json!({"subject":subject,"evidence_sha256":row.evidence_sha256,"details":payload}),
        ),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(tx)
    .await?;
    Ok(())
}

impl Repository {
    pub async fn submit_billing_evidence(
        &self,
        project: &str,
        reservation_id: &str,
        actor: &str,
        subject: &str,
        request: EvidenceRequest,
    ) -> ServiceResult<Value> {
        validate_evidence(&request)?;
        let evidence =
            serde_json::to_value(request).map_err(|e| ServiceError::Internal(e.to_string()))?;
        self.submit_review(project, reservation_id, actor, subject, evidence, false)
            .await
    }

    pub async fn submit_billing_waiver(
        &self,
        project: &str,
        reservation_id: &str,
        actor: &str,
        subject: &str,
        request: WaiverRequest,
    ) -> ServiceResult<Value> {
        validate_waiver(&request)?;
        let mut evidence =
            serde_json::to_value(request).map_err(|e| ServiceError::Internal(e.to_string()))?;
        evidence["kind"] = json!("loss_waiver");
        self.submit_review(project, reservation_id, actor, subject, evidence, true)
            .await
    }

    async fn submit_review(
        &self,
        project: &str,
        reservation_id: &str,
        actor: &str,
        subject: &str,
        evidence: Value,
        waiver: bool,
    ) -> ServiceResult<Value> {
        let tx = self.db.begin().await?;
        let account = lock_account(&tx, project).await?;
        let id = evidence["id"]
            .as_str()
            .ok_or_else(|| invalid("missing review identity"))?
            .to_owned();
        if let Some(existing) = review::Entity::find_by_id(&id).one(&tx).await? {
            if existing.project_id != project
                || existing.reservation_id != reservation_id
                || existing.submitted_by != actor
                || existing.evidence != evidence
            {
                return Err(conflict("evidence ID is bound to a different submission"));
            }
            tx.commit().await?;
            return Ok(summary(&existing));
        }
        let held = reservation::Entity::find_by_id(reservation_id)
            .filter(reservation::Column::ProjectId.eq(project))
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if held.state != "dispatched" && !(waiver && held.state == "reserved") {
            return Err(conflict(
                "review requires a pending reservation (usage evidence requires dispatched)",
            ));
        }
        let hash = fingerprint(
            &json!({"project":project,"reservation":reservation_id,"submission":evidence}),
        )?;
        let now = Utc::now().fixed_offset();
        let row = review::ActiveModel {
            id: Set(id.clone()),
            reservation_id: Set(reservation_id.into()),
            project_id: Set(project.into()),
            submitted_by: Set(actor.into()),
            evidence: Set(evidence),
            evidence_sha256: Set(hash),
            state: Set("submitted".into()),
            reviewed_by: Set(None),
            decision: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&tx)
        .await
        .map_err(crate::repository::conflict_or_database)?;
        append_audit(
            &tx,
            &row,
            1,
            actor,
            subject,
            json!({"reservation_id":reservation_id}),
        )
        .await?;
        append_event(&tx, &account.id, "review.submitted", &id, json!({"reservation_id":reservation_id,"actor_id":actor,"evidence_sha256":row.evidence_sha256})).await?;
        tx.commit().await?;
        Ok(summary(&row))
    }

    pub async fn billing_reviews(
        &self,
        project: &str,
        reservation_id: &str,
        request: ListRequest,
    ) -> ServiceResult<Value> {
        let limit = request.limit.unwrap_or(20);
        if !(1..=100).contains(&limit)
            || request.after_id.as_ref().is_some_and(|id| !identifier(id))
        {
            return Err(invalid("invalid review cursor or limit"));
        }
        let mut query = review::Entity::find()
            .filter(review::Column::ProjectId.eq(project))
            .filter(review::Column::ReservationId.eq(reservation_id));
        if let Some(id) = request.after_id {
            query = query.filter(review::Column::Id.gt(id));
        }
        let mut rows = query
            .order_by_asc(review::Column::Id)
            .limit(limit + 1)
            .all(&self.db)
            .await?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let next = if more {
            rows.last().map(|row| row.id.clone())
        } else {
            None
        };
        Ok(json!({"data":rows.iter().map(summary).collect::<Vec<_>>(),"next":next}))
    }

    pub async fn billing_review_detail(&self, project: &str, id: &str) -> ServiceResult<Value> {
        // One read transaction snapshot avoids returning a decision without its
        // audit row during an approval commit.
        let tx = self
            .db
            .begin_with_config(
                Some(sea_orm::IsolationLevel::RepeatableRead),
                Some(sea_orm::AccessMode::ReadOnly),
            )
            .await?;
        let row = review::Entity::find_by_id(id)
            .filter(review::Column::ProjectId.eq(project))
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let audits = audit::Entity::find()
            .filter(audit::Column::ReviewId.eq(id))
            .order_by_asc(audit::Column::Sequence)
            .limit(2)
            .all(&tx)
            .await?;
        let mut detail = summary(&row);
        detail["evidence"] = row.evidence;
        detail["audit"] = json!(audits);
        tx.commit().await?;
        Ok(detail)
    }

    pub async fn decide_billing_review(
        &self,
        project: &str,
        id: &str,
        actor: &str,
        subject: &str,
        request: DecisionRequest,
    ) -> ServiceResult<Value> {
        if !["approve", "reject"].contains(&request.action.as_str())
            || request.reason.trim().is_empty()
            || request.reason.len() > 2_048
            || request.evidence_sha256.len() != 64
            || !request
                .evidence_sha256
                .bytes()
                .all(|c| c.is_ascii_hexdigit())
        {
            return Err(invalid(
                "decision requires action, exact evidence SHA-256 and reason",
            ));
        }
        let tx = self.db.begin().await?;
        let account = lock_account(&tx, project).await?;
        let row = review::Entity::find_by_id(id)
            .filter(review::Column::ProjectId.eq(project))
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if row.submitted_by == actor {
            return Err(ServiceError::Forbidden);
        }
        if row.evidence_sha256 != request.evidence_sha256 {
            return Err(conflict("reviewed evidence SHA-256 changed"));
        }
        let approve = request.action == "approve";
        let decision =
            serde_json::to_value(request).map_err(|e| ServiceError::Internal(e.to_string()))?;
        if row.state != "submitted" {
            if row.reviewed_by.as_deref() != Some(actor) || row.decision.as_ref() != Some(&decision)
            {
                return Err(conflict(
                    "review already decided with another payload/actor",
                ));
            }
            tx.commit().await?;
            return Ok(summary(&row));
        }
        let waiver = row.evidence["kind"].as_str() == Some("loss_waiver");
        if approve {
            let held = reservation::Entity::find_by_id(&row.reservation_id)
                .filter(reservation::Column::ProjectId.eq(project))
                .one(&tx)
                .await?
                .ok_or(ServiceError::NotFound)?;
            if waiver {
                if !matches!(held.state.as_str(), "reserved" | "dispatched") {
                    return Err(conflict("only pending reservations can be waived"));
                }
                crate::billing_projection::change_hold(
                    &tx,
                    &account.id,
                    &held.api_key_id,
                    -held.reserved_microunits,
                )
                .await?;
                let held_id = held.id.clone();
                // Keep unknown cost NULL. A waiver is NOT a zero-token settlement,
                // a customer credit, or an estimate of actual provider expense.
                let mut active: reservation::ActiveModel = held.into();
                active.state = Set("waived".into());
                active.updated_at = Set(Utc::now().fixed_offset());
                active.update(&tx).await?;
                append_event(
                    &tx,
                    &account.id,
                    "reservation.waived",
                    &held_id,
                    json!({"review_id":id,"actor_id":actor,"evidence_sha256":row.evidence_sha256}),
                )
                .await?;
            } else {
                let evidence: EvidenceRequest = serde_json::from_value(row.evidence.clone())
                    .map_err(|e| ServiceError::Internal(e.to_string()))?;
                settle_locked(&tx, &account, held, evidence.usage).await?;
            }
        }
        let mut active: review::ActiveModel = row.into();
        active.state = Set(if !approve {
            "rejected"
        } else if waiver {
            "waived"
        } else {
            "settled"
        }
        .into());
        active.reviewed_by = Set(Some(actor.into()));
        active.decision = Set(Some(decision.clone()));
        active.updated_at = Set(Utc::now().fixed_offset());
        let row = active.update(&tx).await?;
        append_audit(&tx, &row, 2, actor, subject, decision).await?;
        append_event(&tx, &account.id, if !approve { "review.rejected" } else if waiver { "review.waived" } else { "review.settled" }, id,
            json!({"reservation_id":row.reservation_id,"actor_id":actor,"evidence_sha256":row.evidence_sha256})).await?;
        tx.commit().await?;
        Ok(summary(&row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn waiver_requires_explicit_loss_and_termination_attestations() {
        let mut request = WaiverRequest {
            id: "waiver-fixture".into(),
            reason: "Unrecoverable synthetic receipt".into(),
            incident_reference: "incident-fixture".into(),
            request_terminated: true,
            platform_absorbs_loss: true,
        };
        assert!(validate_waiver(&request).is_ok());
        request.platform_absorbs_loss = false;
        assert!(validate_waiver(&request).is_err());
        request.platform_absorbs_loss = true;
        request.request_terminated = false;
        assert!(validate_waiver(&request).is_err());
    }
    #[test]
    fn receipt_is_required_and_bounded() {
        let mut request = EvidenceRequest {
            id: "case-1".into(),
            source: "runtime".into(),
            source_request_id: "req-1".into(),
            document: "receipt".into(),
            explanation: "final provider usage".into(),
            usage: SettleRequest {
                input_tokens: 0,
                output_tokens: 0,
                latency_ms: 0,
                endpoint_id: "endpoint".into(),
                region: "local".into(),
                status: "cancelled".into(),
            },
        };
        assert!(validate_evidence(&request).is_ok());
        request.document = " ".into();
        assert!(validate_evidence(&request).is_err());
        request.document = "x".repeat(65_537);
        assert!(validate_evidence(&request).is_err());
        request.document = "receipt".into();
        request.usage.status = "unknown".into();
        assert!(validate_evidence(&request).is_err());
    }
}
