//! A participant is recorded before an accepting snapshot can leave the server.
use crate::{
    billing::{conflict, invalid},
    clusters::now,
    error::{ServiceError, ServiceResult},
    managed_pools::{decode, encode},
    repository::Repository,
};
use base64::Engine;
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, TransactionTrait,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use xscope_domain::{
    GatewaySnapshot,
    catalog::ServingEndpoint,
    traffic::{LEASE_SECONDS, PoolControl, TrafficReport, TrafficSnapshot},
};
use xscope_entities::{
    gateway_pool_view as view, gateway_session as session, managed_pool as pool,
    member_cluster as cluster,
};

impl Repository {
    pub async fn route_acks(&self, project: &str, model: &str) -> ServiceResult<serde_json::Value> {
        let policy = xscope_entities::route_policy::Entity::find_by_id((
            project.to_owned(),
            model.to_owned(),
        ))
        .one(&self.db)
        .await?
        .ok_or(ServiceError::NotFound)?;
        let rows = session::Entity::find()
            .order_by_asc(session::Column::Id)
            .all(&self.db)
            .await?;
        let sessions: Vec<_> = rows.into_iter().filter_map(|s| {
            let relevant = s.delivered_routes.as_array().is_some_and(|a| a.iter().any(|r| r["project_id"]==project && r["model"]==model));
            if !relevant { return None; }
            let acknowledged = s.closed || s.acknowledged_routes.as_array().is_some_and(|a| a.iter().any(|r| r["project_id"]==project && r["model"]==model && r["revision"].as_i64().is_some_and(|v| v>=policy.revision)));
            Some(json!({"session_id":s.id,"acknowledged":acknowledged,"retired":s.closed,"delivered_at":s.delivered_at}))
        }).collect();
        let complete = !sessions.is_empty() && sessions.iter().all(|s| s["acknowledged"] == true);
        Ok(
            json!({"revision":policy.revision,"known_sessions_only":true,"all_known_acknowledged":complete,"sessions":sessions}),
        )
    }
    pub async fn traffic_snapshot(
        &self,
        id: &str,
        identity: Option<xscope_domain::traffic::GatewayIdentity>,
    ) -> ServiceResult<GatewaySnapshot> {
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(invalid("invalid gateway incarnation"));
        }
        let mut snapshot = self.gateway_snapshot().await?;
        if let Some(identity) = &identity {
            crate::gateway_proofs::validate_identity(&self.db, identity).await?;
        }
        let tx = self.db.begin().await?;
        // Drain and snapshot grant serialize on these rows, including a lost GET reply.
        let pools = pool::Entity::find()
            .order_by_asc(pool::Column::Id)
            .lock_shared()
            .all(&tx)
            .await?;
        let time = now(&tx).await?;
        session::Entity::insert(session::ActiveModel {
            gateway_cluster_id: Set(identity.as_ref().map(|i| i.cluster_id.clone())),
            proof_complete: Set(false),
            identity: Set(identity.as_ref().map(encode).transpose()?),
            runtime: Set(None),
            proof: Set(None),
            proof_nonce: Set(None),
            proof_until: Set(None),
            id: Set(id.into()),
            sequence: Set(0),
            closed: Set(false),
            nonce: Set(String::new()),
            reported: Set(None),
            delivered_routes: Set(json!([])),
            acknowledged_routes: Set(json!([])),
            delivered_at: Set(time),
        })
        .on_conflict(
            OnConflict::column(session::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .try_insert()
        .exec(&tx)
        .await?;
        let row = session::Entity::find_by_id(id)
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if row.closed {
            return Err(conflict("gateway incarnation was permanently retired"));
        }
        if row.identity != identity.as_ref().map(encode).transpose()? {
            return Err(conflict("gateway incarnation identity changed"));
        }
        let attributed = row.identity.is_some() && row.runtime.is_some();
        let attribution_required =
            std::env::var("XSCOPE_REQUIRE_GATEWAY_IDENTITY").as_deref() == Ok("true");
        let identity_ready = attributed || (!attribution_required && row.identity.is_none());
        let sequence = row
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("gateway sequence exhausted"))?;
        let nonce = uuid::Uuid::now_v7().to_string();
        let mut active: session::ActiveModel = row.into();
        active.sequence = Set(sequence);
        active.nonce = Set(nonce.clone());
        active.reported = Set(None);
        active.delivered_routes = Set(json!(
            snapshot
                .route_policies
                .iter()
                .map(|p| json!({"project_id":p.project_id,"model":p.model,"revision":p.revision}))
                .collect::<Vec<_>>()
        ));
        active.delivered_at = Set(time);
        active.update(&tx).await?;
        let mut entropy = [0u8; 32];
        getrandom::fill(&mut entropy).map_err(|_| invalid("traffic entropy unavailable"))?;
        let admission_token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(entropy);
        xscope_entities::traffic_grant::Entity::delete_many()
            .filter(xscope_entities::traffic_grant::Column::SessionId.eq(id))
            .filter(
                xscope_entities::traffic_grant::Column::ExpiresAt
                    .lt(time - chrono::Duration::minutes(2)),
            )
            .exec(&tx)
            .await?;
        let clusters = cluster::Entity::find().all(&tx).await?;
        let mut controls = Vec::new();
        for pool in pools {
            let identity_valid = clusters.iter().any(|c| {
                c.id == pool.cluster_id
                    && c.revoked_at.is_none()
                    && c.expires_at > time
                    && c.acknowledged_version
                        >= pool
                            .scale_operation
                            .as_ref()
                            .filter(|op| op["up"] == true)
                            .and_then(|op| op["previous_version"].as_i64())
                            .unwrap_or(pool.desired_version)
            });
            let accepting = pool.state == "active"
                && identity_ready
                && identity_valid
                && pool.ready_until.is_some_and(|until| until > time);
            let previous = view::Entity::find_by_id((pool.id.clone(), id.to_owned()))
                .one(&tx)
                .await?;
            if let Some(previous) = previous {
                let mut active: view::ActiveModel = previous.into();
                active.delivered_generation = Set(pool.generation);
                active.update(&tx).await?;
            } else if accepting {
                view::ActiveModel {
                    pool_id: Set(pool.id.clone()),
                    session_id: Set(id.into()),
                    delivered_generation: Set(pool.generation),
                    acknowledged_generation: Set(0),
                    active_requests: Set(0),
                    retired: Set(false),
                    reported_at: Set(None),
                }
                .insert(&tx)
                .await?;
            }
            let endpoint: ServingEndpoint = decode(pool.endpoint)?;
            snapshot.catalog.endpoints.push(endpoint);
            controls.push(PoolControl {
                id: pool.id,
                generation: pool.generation,
                accepting,
            });
        }
        let pools: std::collections::BTreeMap<_, _> = controls
            .iter()
            .filter(|p| p.accepting)
            .map(|p| (p.id.clone(), p.generation))
            .collect();
        xscope_entities::traffic_grant::ActiveModel {
            token_hash: Set(format!("{:x}", Sha256::digest(admission_token.as_bytes()))),
            session_id: Set(id.into()),
            expires_at: Set(time + chrono::Duration::seconds(LEASE_SECONDS as i64)),
            pools: Set(encode(&pools)?),
        }
        .insert(&tx)
        .await?;
        tx.commit().await?;
        snapshot.traffic = Some(TrafficSnapshot {
            admission_token,
            session_id: id.into(),
            sequence,
            nonce,
            lease_seconds: LEASE_SECONDS,
            pools: controls,
        });
        Ok(snapshot)
    }

    pub async fn authorize_serving(&self, headers: &axum::http::HeaderMap) -> ServiceResult<()> {
        let get = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .ok_or(ServiceError::Forbidden)
        };
        let session_id = get("x-xscope-traffic-session")?;
        let token = get("x-xscope-traffic-token")?;
        let pool_id = get("x-xscope-managed-pool")?;
        if token.len() != 43 || uuid::Uuid::parse_str(session_id).is_err() {
            return Err(ServiceError::Forbidden);
        }
        let grant = xscope_entities::traffic_grant::Entity::find_by_id(format!(
            "{:x}",
            Sha256::digest(token.as_bytes())
        ))
        .one(&self.db)
        .await?
        .ok_or(ServiceError::Forbidden)?;
        if grant.session_id != session_id || grant.expires_at <= chrono::Utc::now().fixed_offset() {
            return Err(ServiceError::Forbidden);
        }
        let session = session::Entity::find_by_id(session_id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::Forbidden)?;
        if session.closed || (session.identity.is_some() && session.runtime.is_none()) {
            return Err(ServiceError::Forbidden);
        }
        let pool = pool::Entity::find_by_id(pool_id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::Forbidden)?;
        if grant.pools.get(pool_id).and_then(|v| v.as_i64()) != Some(pool.generation) {
            return Err(ServiceError::Forbidden);
        }
        if pool.state != "active"
            || pool
                .ready_until
                .is_none_or(|t| t <= chrono::Utc::now().fixed_offset())
        {
            return Err(ServiceError::Forbidden);
        }
        let identity = cluster::Entity::find_by_id(&pool.cluster_id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::Forbidden)?;
        if identity.revoked_at.is_some() || identity.expires_at <= chrono::Utc::now().fixed_offset()
        {
            return Err(ServiceError::Forbidden);
        }
        if std::env::var("XSCOPE_REQUIRE_GATEWAY_IDENTITY").as_deref() == Ok("true")
            && session.runtime.is_none()
        {
            return Err(ServiceError::Forbidden);
        }
        Ok(())
    }
    pub async fn traffic_report(&self, report: TrafficReport) -> ServiceResult<serde_json::Value> {
        if uuid::Uuid::parse_str(&report.session_id).is_err()
            || report.sequence <= 0
            || report.active.len() > 10000
            || report
                .active
                .values()
                .any(|n| *n > 1_000_000 || (report.closed && *n != 0))
        {
            return Err(invalid("invalid gateway ACK"));
        }
        let tx = self.db.begin().await?;
        let row = session::Entity::find_by_id(&report.session_id)
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if row.sequence != report.sequence || row.nonce != report.nonce {
            return Err(conflict("stale gateway ACK"));
        }
        let payload = encode(&report)?;
        if row.closed {
            if report.closed && row.reported.as_ref() == Some(&payload) {
                tx.commit().await?;
                return Ok(json!({"duplicate":true}));
            }
            return Err(conflict("retired gateway cannot publish a late ACK"));
        }
        if let Some(previous) = &row.reported {
            if previous != &payload {
                return Err(conflict("ACK sequence is bound to a different report"));
            }
            tx.commit().await?;
            return Ok(json!({"duplicate":true}));
        }
        let views = view::Entity::find()
            .filter(view::Column::SessionId.eq(&report.session_id))
            .all(&tx)
            .await?;
        let time = now(&tx).await?;
        for view in views {
            let count = report.active.get(&view.pool_id).ok_or_else(|| {
                invalid("ACK must include every exposed pool, including zero counts")
            })?;
            let mut active: view::ActiveModel = view.clone().into();
            active.acknowledged_generation = Set(view.delivered_generation);
            active.retired = Set(report.closed);
            active.active_requests = Set(*count as i64);
            active.reported_at = Set(Some(time));
            active.update(&tx).await?;
        }
        let mut active: session::ActiveModel = row.clone().into();
        active.acknowledged_routes = Set(row.delivered_routes);
        active.closed = Set(report.closed);
        active.reported = Set(Some(payload));
        active.update(&tx).await?;
        tx.commit().await?;
        Ok(json!({"accepted":true}))
    }
}
