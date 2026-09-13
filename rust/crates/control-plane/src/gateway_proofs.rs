//! Container termination fences future traffic. Unknown provider work is
//! cleared separately, using post-fence Envoy in-flight evidence.
use crate::{
    billing::{conflict, invalid},
    clusters::{audit, authenticated, name, now},
    error::{ServiceError, ServiceResult},
    managed_pools::{decode, encode},
    repository::Repository,
};
use sea_orm::TransactionTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseTransaction,
    EntityTrait, QueryFilter, QueryOrder, QuerySelect,
};
use serde_json::{Value, json};
use xscope_domain::traffic::{GatewayIdentity, GatewayProof, GatewayProofTask, GatewayRuntime};
use xscope_entities::{
    gateway_pool_view as view, gateway_session as session, managed_pool, member_cluster as cluster,
};

pub async fn validate_identity(
    db: &impl ConnectionTrait,
    identity: &GatewayIdentity,
) -> ServiceResult<()> {
    if !name(&identity.cluster_id)
        || !name(&identity.namespace)
        || !name(&identity.pod)
        || identity.pod_uid.is_empty()
        || identity.pod_uid.len() > 128
    {
        return Err(invalid("invalid gateway pod identity"));
    }
    let row = cluster::Entity::find_by_id(&identity.cluster_id)
        .one(db)
        .await?
        .ok_or(ServiceError::Forbidden)?;
    if row.namespace != identity.namespace
        || row.revoked_at.is_some()
        || row.expires_at <= chrono::Utc::now().fixed_offset()
    {
        return Err(ServiceError::Forbidden);
    }
    Ok(())
}

impl Repository {
    pub async fn gateway_proof_tasks(&self, id: &str, token: &str) -> ServiceResult<Value> {
        let tx = self.db.begin().await?;
        authenticated(&tx, id, token).await?;
        let rows = session::Entity::find()
            .filter(session::Column::GatewayClusterId.eq(id))
            .filter(session::Column::ProofComplete.eq(false))
            .order_by_asc(session::Column::DeliveredAt)
            .limit(100)
            .all(&tx)
            .await?;
        let open = session::Entity::find()
            .filter(session::Column::GatewayClusterId.eq(id))
            .filter(session::Column::Closed.eq(false))
            .all(&tx)
            .await?;
        let time = now(&tx).await?;
        let mut tasks = Vec::new();
        for row in rows {
            let identity: GatewayIdentity = decode(
                row.identity
                    .clone()
                    .ok_or_else(|| invalid("gateway identity missing"))?,
            )?;
            let runtime: Option<GatewayRuntime> = row.runtime.clone().map(decode).transpose()?;
            let nonce = uuid::Uuid::now_v7().to_string();
            let release_finalizer = row.closed
                && !open.iter().any(|s| {
                    s.identity
                        .as_ref()
                        .is_some_and(|i| i["pod_uid"] == identity.pod_uid)
                });
            tasks.push(GatewayProofTask {
                session_id: row.id.clone(),
                nonce: nonce.clone(),
                identity,
                runtime,
                closed: row.closed,
                release_finalizer,
            });
            let mut active: session::ActiveModel = row.into();
            active.proof_nonce = Set(Some(nonce));
            active.proof_until = Set(Some(time + chrono::Duration::seconds(60)));
            active.update(&tx).await?;
        }
        tx.commit().await?;
        Ok(json!({"tasks":tasks}))
    }
    pub async fn gateway_proof(
        &self,
        id: &str,
        token: &str,
        proof: GatewayProof,
    ) -> ServiceResult<Value> {
        if !matches!(
            proof.outcome.as_str(),
            "bound" | "terminated" | "unknown" | "unbound" | "cleaned"
        ) {
            return Err(invalid("unknown termination proof outcome"));
        }
        let tx = self.db.begin().await?;
        authenticated(&tx, id, token).await?;
        let row = session::Entity::find_by_id(&proof.session_id)
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let time = now(&tx).await?;
        if row.gateway_cluster_id.as_deref() != Some(id) {
            return Err(ServiceError::Forbidden);
        }
        if row.proof_nonce.as_deref() != Some(&proof.nonce)
            || row.proof_until.is_none_or(|t| t <= time)
        {
            return Err(conflict("stale gateway proof"));
        }
        let mut active: session::ActiveModel = row.clone().into();
        active.proof_nonce = Set(None);
        active.proof_until = Set(None);
        match proof.outcome.as_str() {
            "bound" => {
                let runtime = proof
                    .runtime
                    .ok_or_else(|| invalid("container attestation missing"))?;
                if row.closed
                    || runtime.container_id.is_empty()
                    || runtime.container_id.len() > 256
                    || !name(&runtime.node)
                    || runtime.node_uid.is_empty()
                    || runtime.node_uid.len() > 128
                {
                    return Err(invalid("invalid container attestation"));
                }
                let value = encode(&runtime)?;
                if row.runtime.as_ref().is_some_and(|old| old != &value) {
                    return Err(conflict("container attestation is immutable"));
                }
                active.runtime = Set(Some(value));
            }
            "terminated" => {
                let value = proof.runtime.as_ref().map(encode).transpose()?;
                if row.runtime.is_none() || row.runtime != value {
                    return Err(conflict(
                        "termination must match the attested container and node",
                    ));
                }
                if !row.closed {
                    active.closed = Set(true);
                    active.proof = Set(Some(
                        json!({"kind":"container_terminated","observed_at":time,"runtime":value}),
                    ));
                    // A stopped Gateway cannot report provider usage. Retire it,
                    // but retain UNKNOWN in-flight work until the entry is idle.
                    view::Entity::update_many()
                        .col_expr(view::Column::Retired, Expr::value(true))
                        .col_expr(view::Column::ActiveRequests, Expr::value(-1))
                        .filter(view::Column::SessionId.eq(&row.id))
                        .exec(&tx)
                        .await?;
                    audit(
                        &tx,
                        &format!("cluster:{id}"),
                        "gateway.container_terminated",
                        &row.id,
                        json!({"provider_usage_not_inferred":true}),
                    )
                    .await?;
                }
            }
            "unbound" => {
                if row.runtime.is_some()
                    || !view::Entity::find()
                        .filter(view::Column::SessionId.eq(&row.id))
                        .all(&tx)
                        .await?
                        .is_empty()
                {
                    return Err(conflict(
                        "gateway has granted traffic; termination evidence required",
                    ));
                }
                active.closed = Set(true);
                active.proof_complete = Set(true);
            }
            "cleaned" => {
                if !row.closed {
                    return Err(conflict("gateway is not retired"));
                }
                active.proof_complete = Set(true);
            }
            _ => {}
        }
        active.update(&tx).await?;
        tx.commit().await?;
        Ok(json!({"accepted":true}))
    }
}

pub async fn idle_after(tx: &DatabaseTransaction, pool: &str) -> ServiceResult<Option<String>> {
    let views = view::Entity::find()
        .filter(view::Column::PoolId.eq(pool))
        .filter(view::Column::ActiveRequests.lt(0))
        .all(tx)
        .await?;
    let mut after = None;
    for v in views {
        let row = session::Entity::find_by_id(&v.session_id)
            .one(tx)
            .await?
            .ok_or_else(|| invalid("gateway evidence missing"))?;
        let time = row
            .proof
            .as_ref()
            .and_then(|p| p["observed_at"].as_str())
            .ok_or_else(|| invalid("gateway termination time missing"))?;
        let time = chrono::DateTime::parse_from_rfc3339(time)
            .map_err(|_| invalid("invalid termination time"))?;
        after = Some(
            after.map_or(time, |old: chrono::DateTime<chrono::FixedOffset>| {
                old.max(time)
            }),
        );
    }
    Ok(after.map(|t| t.to_rfc3339()))
}
pub async fn confirm_idle(
    tx: &DatabaseTransaction,
    pool: &managed_pool::Model,
) -> ServiceResult<()> {
    if pool.state != "draining" {
        return Err(conflict("provider idle evidence requires a closed pool"));
    }
    let binding: xscope_domain::traffic::PoolBinding = decode(pool.binding.clone())?;
    if binding.traffic_protocol != 2 {
        return Err(conflict(
            "orphan drain requires a v2 authorization-fenced entry",
        ));
    }
    if idle_after(tx, &pool.id).await?.is_none() {
        return Ok(());
    }
    view::Entity::update_many()
        .col_expr(view::Column::ActiveRequests, Expr::value(0))
        .filter(view::Column::PoolId.eq(&pool.id))
        .filter(view::Column::Retired.eq(true))
        .filter(view::Column::ActiveRequests.lt(0))
        .exec(tx)
        .await?;
    audit(
        tx,
        "member-agent",
        "pool.orphans_drained",
        &pool.id,
        json!({"financial_holds_unchanged":true}),
    )
    .await?;
    Ok(())
}
