//! A participant is recorded before an accepting snapshot can leave the server.
use crate::{
    billing::{conflict, invalid},
    clusters::now,
    error::{ServiceError, ServiceResult},
    managed_pools::{decode, encode},
    repository::Repository,
};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, TransactionTrait,
};
use serde_json::json;
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
    pub async fn traffic_snapshot(&self, id: &str) -> ServiceResult<GatewaySnapshot> {
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(invalid("invalid gateway incarnation"));
        }
        let mut snapshot = self.gateway_snapshot().await?;
        let tx = self.db.begin().await?;
        // Drain and snapshot grant serialize on these rows, including a lost GET reply.
        let pools = pool::Entity::find()
            .order_by_asc(pool::Column::Id)
            .lock_shared()
            .all(&tx)
            .await?;
        let time = now(&tx).await?;
        session::Entity::insert(session::ActiveModel {
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
        let clusters = cluster::Entity::find().all(&tx).await?;
        let mut controls = Vec::new();
        for pool in pools {
            let identity_valid = clusters.iter().any(|c| {
                c.id == pool.cluster_id
                    && c.revoked_at.is_none()
                    && c.expires_at > time
                    && c.acknowledged_version >= pool.scale_operation.as_ref().filter(|op|op["up"]==true).and_then(|op|op["previous_version"].as_i64()).unwrap_or(pool.desired_version)
            });
            let accepting = pool.state == "active"
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
        tx.commit().await?;
        snapshot.traffic = Some(TrafficSnapshot {
            session_id: id.into(),
            sequence,
            nonce,
            lease_seconds: LEASE_SECONDS,
            pools: controls,
        });
        Ok(snapshot)
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
