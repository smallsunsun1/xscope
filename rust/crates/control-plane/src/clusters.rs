use crate::{
    billing::{conflict, invalid},
    error::{ServiceError, ServiceResult},
    repository::Repository,
};
use base64::Engine;
use chrono::{DateTime, FixedOffset, Utc};
use sea_orm::sea_query::{Alias, Func, Query};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ConnectionTrait, DatabaseTransaction, EntityTrait,
    QueryOrder, QuerySelect, TransactionTrait,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use xscope_domain::cluster::{ClusterDelivery, ClusterReport, DesiredState};
use xscope_entities::{cluster_revision as revision, member_cluster as cluster, operation_audit};

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(crate) fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
pub(crate) async fn now(tx: &DatabaseTransaction) -> ServiceResult<DateTime<FixedOffset>> {
    let q = Query::select()
        .expr_as(Func::cust(Alias::new("clock_timestamp")), "now")
        .to_owned();
    Ok(tx
        .query_one(&q)
        .await?
        .ok_or_else(|| invalid("database clock unavailable"))?
        .try_get("", "now")?)
}
pub(crate) async fn audit(
    tx: &DatabaseTransaction,
    actor: &str,
    action: &str,
    id: &str,
    payload: Value,
) -> ServiceResult<()> {
    operation_audit::ActiveModel {
        id: Set(uuid::Uuid::now_v7().to_string()),
        actor_id: Set(actor.into()),
        action: Set(action.into()),
        resource_id: Set(id.into()),
        payload: Set(payload),
        created_at: Set(now(tx).await?),
    }
    .insert(tx)
    .await?;
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterCluster {
    pub id: String,
    pub namespace: String,
    pub credential_days: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RotateCredential {
    pub expected_epoch: i64,
    pub credential_days: i64,
}
fn credential(days: i64) -> ServiceResult<(String, String)> {
    if !(1..=90).contains(&days) {
        return Err(invalid("cluster credentials expire in 1..90 days"));
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| invalid("credential entropy unavailable"))?;
    let value = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    Ok((hash(value.as_bytes()), value))
}
fn view(row: cluster::Model) -> Value {
    let online = row
        .heartbeat_at
        .is_some_and(|time| Utc::now().signed_duration_since(time).num_seconds() < 90);
    json!({"id":row.id,"namespace":row.namespace,"credential_epoch":row.credential_epoch,"expires_at":row.expires_at,
        "revoked_at":row.revoked_at,"desired_version":row.desired_version,"acknowledged_version":row.acknowledged_version,
        "heartbeat_at":row.heartbeat_at,"online":online,"last_error":row.last_error,"ready_not_implied":true})
}
pub(crate) async fn authenticated(
    tx: &DatabaseTransaction,
    id: &str,
    token: &str,
) -> ServiceResult<cluster::Model> {
    let row = cluster::Entity::find_by_id(id)
        .lock_exclusive()
        .one(tx)
        .await?
        .ok_or(ServiceError::Unauthorized)?;
    if row.revoked_at.is_some()
        || row.expires_at <= now(tx).await?
        || !bool::from(
            row.credential_hash
                .as_bytes()
                .ct_eq(hash(token.as_bytes()).as_bytes()),
        )
    {
        return Err(ServiceError::Unauthorized);
    }
    Ok(row)
}
impl Repository {
    pub async fn register_cluster(
        &self,
        request: RegisterCluster,
        actor: &str,
    ) -> ServiceResult<Value> {
        if !name(&request.id) || !name(&request.namespace) {
            return Err(invalid("invalid cluster identity or namespace"));
        }
        let (digest, token) = credential(request.credential_days)?;
        let tx = self.db.begin().await?;
        if cluster::Entity::find_by_id(&request.id)
            .one(&tx)
            .await?
            .is_some()
        {
            return Err(conflict("cluster identity already exists"));
        }
        let time = now(&tx).await?;
        let row = cluster::ActiveModel {
            id: Set(request.id.clone()),
            namespace: Set(request.namespace),
            credential_hash: Set(digest),
            credential_epoch: Set(1),
            expires_at: Set(time + chrono::Duration::days(request.credential_days)),
            revoked_at: Set(None),
            desired_version: Set(0),
            acknowledged_version: Set(0),
            delivery_version: Set(0),
            delivery_lease: Set(None),
            lease_until: Set(None),
            heartbeat_at: Set(None),
            last_error: Set(None),
            created_at: Set(time),
            updated_at: Set(time),
        }
        .insert(&tx)
        .await?;
        audit(
            &tx,
            actor,
            "cluster.register",
            &request.id,
            json!({"credential_epoch":1}),
        )
        .await?;
        tx.commit().await?;
        Ok(json!({"cluster":view(row),"credential":token,"display_once":true}))
    }
    pub async fn list_clusters(&self) -> ServiceResult<Value> {
        let rows = cluster::Entity::find()
            .order_by_asc(cluster::Column::Id)
            .limit(1000)
            .all(&self.db)
            .await?;
        Ok(json!({"data":rows.into_iter().map(view).collect::<Vec<_>>(),"limit":1000}))
    }
    pub async fn rotate_cluster_credential(
        &self,
        id: &str,
        request: RotateCredential,
        actor: &str,
    ) -> ServiceResult<Value> {
        let (digest, token) = credential(request.credential_days)?;
        let tx = self.db.begin().await?;
        let row = cluster::Entity::find_by_id(id)
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if row.credential_epoch != request.expected_epoch {
            return Err(conflict("cluster credential version changed"));
        }
        let epoch = row
            .credential_epoch
            .checked_add(1)
            .ok_or_else(|| invalid("credential epoch exhausted"))?;
        let time = now(&tx).await?;
        let mut active: cluster::ActiveModel = row.into();
        active.credential_hash = Set(digest);
        active.credential_epoch = Set(epoch);
        active.expires_at = Set(time + chrono::Duration::days(request.credential_days));
        active.revoked_at = Set(None);
        active.delivery_lease = Set(None);
        active.lease_until = Set(None);
        active.updated_at = Set(time);
        let row = active.update(&tx).await?;
        audit(
            &tx,
            actor,
            "cluster.credential_rotated",
            id,
            json!({"credential_epoch":epoch}),
        )
        .await?;
        tx.commit().await?;
        Ok(json!({"cluster":view(row),"credential":token,"display_once":true}))
    }
    pub async fn revoke_cluster(&self, id: &str, actor: &str) -> ServiceResult<Value> {
        let tx = self.db.begin().await?;
        let row = cluster::Entity::find_by_id(id)
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if row.revoked_at.is_none() {
            let mut active: cluster::ActiveModel = row.into();
            active.revoked_at = Set(Some(now(&tx).await?));
            active.delivery_lease = Set(None);
            active.lease_until = Set(None);
            active.update(&tx).await?;
            audit(
                &tx,
                actor,
                "cluster.revoked",
                id,
                json!({"resources_deleted":false}),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(json!({"revoked":true,"resources_deleted":false}))
    }
    pub async fn put_cluster_desired(
        &self,
        id: &str,
        mut request: DesiredState,
        actor: &str,
    ) -> ServiceResult<Value> {
        if request.expected_version < 0 || request.deployments.len() > 100 {
            return Err(invalid("invalid desired-state version or size"));
        }
        let mut names = std::collections::HashSet::new();
        for item in &request.deployments {
            if !name(&item.name)
                || !names.insert(&item.name)
                || item
                    .expected_uid
                    .as_ref()
                    .is_some_and(|uid| uid.is_empty() || uid.len() > 128)
                || (item.spec.is_none() && item.expected_uid.is_some())
                || match &item.spec {
                    Some(spec) => !spec.is_object() || item.delete_uid.is_some(),
                    None => item
                        .delete_uid
                        .as_deref()
                        .is_none_or(|uid| uid.is_empty() || uid.len() > 128),
                }
            {
                return Err(invalid(
                    "desired deployments require unique names, specs, or explicit UID-bound deletion",
                ));
            }
        }
        let payload = serde_json::to_value(&request.deployments)
            .map_err(|_| invalid("invalid desired state"))?;
        let bytes = serde_json::to_vec(&payload).map_err(|_| invalid("invalid desired state"))?;
        if bytes.len() > 256 * 1024 {
            return Err(invalid("desired state exceeds 256 KiB"));
        }
        let tx = self.db.begin().await?;
        let row = cluster::Entity::find_by_id(id)
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if row.revoked_at.is_some() {
            return Err(conflict("cluster is revoked"));
        }
        if row.desired_version != request.expected_version {
            return Err(conflict("desired state version changed; reload"));
        }
        let version = row
            .desired_version
            .checked_add(1)
            .ok_or_else(|| invalid("desired version exhausted"))?;
        crate::managed_pools::guard_desired(&tx, id, version, &mut request.deployments).await?;
        let payload = serde_json::to_value(&request.deployments)
            .map_err(|_| invalid("invalid desired state"))?;
        let bytes = serde_json::to_vec(&payload).map_err(|_| invalid("invalid desired state"))?;
        if bytes.len() > 256 * 1024 {
            return Err(invalid("desired state exceeds 256 KiB"));
        }
        let digest = hash(&bytes);
        revision::ActiveModel {
            cluster_id: Set(id.into()),
            version: Set(version),
            payload: Set(payload),
            sha256: Set(digest.clone()),
            created_by: Set(actor.into()),
            created_at: Set(now(&tx).await?),
        }
        .insert(&tx)
        .await?;
        let mut active: cluster::ActiveModel = row.into();
        active.desired_version = Set(version);
        // An already delivered batch finishes or expires before a newer delivery.
        active.updated_at = Set(now(&tx).await?);
        active.update(&tx).await?;
        audit(
            &tx,
            actor,
            "cluster.desired_updated",
            id,
            json!({"version":version,"sha256":digest}),
        )
        .await?;
        tx.commit().await?;
        Ok(json!({"version":version,"sha256":digest,"applied":false}))
    }
    pub async fn poll_cluster(&self, id: &str, token: &str) -> ServiceResult<Value> {
        let tx = self.db.begin().await?;
        let row = authenticated(&tx, id, token).await?;
        let time = now(&tx).await?;
        if row.lease_until.is_some_and(|until| until > time) {
            tx.commit().await?;
            return Ok(json!({"delivery":null,"retry_after_seconds":5}));
        }
        let delivery = if row.desired_version > row.acknowledged_version {
            let revision = revision::Entity::find_by_id((id.to_owned(), row.desired_version))
                .one(&tx)
                .await?
                .ok_or_else(|| conflict("desired revision missing"))?;
            Some(ClusterDelivery {
                cluster_id: id.into(),
                namespace: row.namespace.clone(),
                version: revision.version,
                sha256: revision.sha256,
                lease: uuid::Uuid::now_v7().to_string(),
                lease_seconds: 60,
                deployments: serde_json::from_value(revision.payload)
                    .map_err(|_| invalid("stored desired state invalid"))?,
            })
        } else {
            None
        };
        let mut active: cluster::ActiveModel = row.into();
        active.heartbeat_at = Set(Some(time));
        if let Some(delivery) = &delivery {
            active.delivery_version = Set(delivery.version);
            active.delivery_lease = Set(Some(delivery.lease.clone()));
            active.lease_until = Set(Some(time + chrono::Duration::seconds(60)));
        }
        active.update(&tx).await?;
        tx.commit().await?;
        Ok(json!({"delivery":delivery,"retry_after_seconds":15}))
    }
    pub async fn report_cluster(
        &self,
        id: &str,
        token: &str,
        report: ClusterReport,
    ) -> ServiceResult<Value> {
        if !["applied", "rejected"].contains(&report.outcome.as_str())
            || report.error_code.as_deref().is_some_and(|code| {
                ![
                    "invalid_spec",
                    "ownership_conflict",
                    "identity_mismatch",
                    "kubernetes_unavailable",
                    "apply_timeout",
                ]
                .contains(&code)
            })
            || (report.outcome == "applied" && report.error_code.is_some())
            || (report.outcome == "rejected" && report.error_code.is_none())
        {
            return Err(invalid("invalid cluster report"));
        }
        let tx = self.db.begin().await?;
        let row = authenticated(&tx, id, token).await?;
        let revision = revision::Entity::find_by_id((id.to_owned(), report.version))
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if revision.sha256 != report.sha256 {
            return Err(conflict("report digest differs from desired state"));
        }
        if row.acknowledged_version == report.version && report.outcome == "applied" {
            tx.commit().await?;
            return Ok(json!({"duplicate":true,"acknowledged_version":report.version}));
        }
        let time = now(&tx).await?;
        if row.delivery_version != report.version
            || row.delivery_lease.as_deref() != Some(&report.lease)
            || row.lease_until.is_none_or(|until| until <= time)
            || report.version < row.acknowledged_version
        {
            return Err(conflict("stale or expired cluster delivery"));
        }
        let mut active: cluster::ActiveModel = row.into();
        if report.outcome == "applied" {
            active.acknowledged_version = Set(report.version);
        }
        active.delivery_lease = Set(None);
        active.lease_until = Set(None);
        active.heartbeat_at = Set(Some(time));
        active.last_error = Set(report.error_code.clone());
        active.updated_at = Set(time);
        active.update(&tx).await?;
        audit(
            &tx,
            &format!("cluster:{id}"),
            if report.outcome == "applied" {
                "cluster.ack"
            } else {
                "cluster.nack"
            },
            id,
            json!({"version":report.version,"sha256":report.sha256,"error_code":report.error_code}),
        )
        .await?;
        tx.commit().await?;
        xscope_telemetry::background_event(
            "cluster_delivery",
            if report.outcome == "applied" {
                "ack"
            } else {
                "nack"
            },
        );
        Ok(json!({"accepted":true,"model_readiness_not_implied":true}))
    }
}
