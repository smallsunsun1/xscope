//! Readiness and request-drain are independent proofs. Expiry never means zero.
use crate::{
    billing::{conflict, invalid},
    clusters::{audit, authenticated, name, now},
    error::{ServiceError, ServiceResult},
    repository::Repository,
};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, TransactionTrait,
};
use serde_json::{Value, json};
use xscope_domain::{
    catalog::ServingEndpoint,
    cluster::DesiredDeployment,
    traffic::{ObservationTask, PoolBinding, PoolObservation},
};
use xscope_entities::{
    cluster_revision as revision, gateway_pool_view as view, managed_pool as pool,
    member_cluster as cluster,
};

pub(crate) fn decode<T: serde::de::DeserializeOwned>(value: Value) -> ServiceResult<T> {
    serde_json::from_value(value).map_err(|_| invalid("invalid managed pool contract"))
}
pub(crate) fn encode<T: serde::Serialize>(value: &T) -> ServiceResult<Value> {
    serde_json::to_value(value).map_err(|_| invalid("invalid managed pool encoding"))
}

pub(crate) async fn registry_lock(tx: &DatabaseTransaction) -> ServiceResult<()> {
    xscope_entities::traffic_registry_guard::Entity::find_by_id(1)
        .lock_exclusive()
        .one(tx)
        .await?
        .ok_or_else(|| conflict("traffic registry guard missing"))?;
    Ok(())
}

impl Repository {
    pub async fn bind_pool(
        &self,
        request: PoolBinding,
        actor: &str,
        legacy: &[xscope_domain::RoutePool],
    ) -> ServiceResult<Value> {
        if !name(&request.cluster_id)
            || !name(&request.deployment)
            || !name(&request.serving_service)
        {
            return Err(invalid(
                "invalid cluster, deployment or serving Service name",
            ));
        }
        let tx = self.db.begin().await?;
        registry_lock(&tx).await?;
        let cluster = cluster::Entity::find_by_id(&request.cluster_id)
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let time = now(&tx).await?;
        if cluster.revoked_at.is_some()
            || cluster.expires_at <= time
            || request.expected_desired_version != cluster.desired_version
        {
            return Err(conflict("cluster credential or desired version changed"));
        }
        let desired = revision::Entity::find_by_id((cluster.id.clone(), cluster.desired_version))
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let deployments: Vec<DesiredDeployment> = decode(desired.payload)?;
        let spec = deployments
            .into_iter()
            .find(|d| d.name == request.deployment)
            .and_then(|d| d.spec)
            .ok_or(ServiceError::NotFound)?;
        if (!spec["autoscaling"].is_null() && spec["autoscaling"]["managed"] != true)
            || spec["runtime"]["protocol"] != "openai"
            || spec["serving"]["endpointPickerService"] != request.serving_service
        {
            return Err(invalid(
                "managed drain currently requires manual replicas and a pool-specific combined Envoy/EPP entry",
            ));
        }
        let model = spec["model"]["id"]
            .as_str()
            .ok_or_else(|| invalid("model ID missing"))?;
        crate::catalog::active_model(&tx, model).await?;
        let id = format!("managed-{}", uuid::Uuid::now_v7());
        let endpoint = ServingEndpoint {
            id: id.clone(),
            model: model.into(),
            revision: spec["model"]["revision"]
                .as_str()
                .ok_or_else(|| invalid("model revision missing"))?
                .into(),
            address: request.address.clone(),
            tls: request.tls,
            server_name: request.server_name.clone(),
        };
        endpoint.validate().map_err(ServiceError::Invalid)?;
        if legacy.iter().any(|p| {
            p.id == request.deployment
                || (p.model == endpoint.model && p.revision == endpoint.revision)
        }) {
            return Err(conflict(
                "managed pool must not reuse a legacy model revision or entry",
            ));
        }
        for old in xscope_entities::serving_endpoint::Entity::find()
            .all(&tx)
            .await?
        {
            let old: ServingEndpoint = decode(old.definition)?;
            if old.address == endpoint.address
                || (old.model == endpoint.model && old.revision == endpoint.revision)
            {
                return Err(conflict(
                    "serving entry is already exposed through the legacy catalog",
                ));
            }
        }
        for old in pool::Entity::find().all(&tx).await? {
            let old: ServingEndpoint = decode(old.endpoint)?;
            if old.address == endpoint.address {
                return Err(conflict("managed entries must be dedicated to one pool"));
            }
        }
        pool::ActiveModel {
            scale_operation: Set(None),
            id: Set(id.clone()),
            cluster_id: Set(cluster.id),
            deployment: Set(request.deployment.clone()),
            generation: Set(1),
            state: Set("pending".into()),
            binding: Set(encode(&request)?),
            endpoint: Set(encode(&endpoint)?),
            expected_spec: Set(spec),
            desired_version: Set(request.expected_desired_version),
            deployment_uid: Set(None),
            ready_until: Set(None),
            observation_nonce: Set(None),
            observation_until: Set(None),
            observation_code: Set("not_ready".into()),
            observed_at: Set(None),
            updated_at: Set(time),
        }
        .insert(&tx)
        .await
        .map_err(crate::repository::conflict_or_database)?;
        audit(
            &tx,
            actor,
            "pool.bound",
            &id,
            json!({"generation":1,"state":"pending"}),
        )
        .await?;
        tx.commit().await?;
        Ok(json!({"id":id,"generation":1,"state":"pending","registered":false}))
    }

    pub async fn managed_pool_status(&self, id: &str) -> ServiceResult<Value> {
        let row = pool::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let participants = view::Entity::find()
            .filter(view::Column::PoolId.eq(id))
            .order_by_asc(view::Column::SessionId)
            .all(&self.db)
            .await?;
        let blocked = participants
            .iter()
            .filter(|v| {
                !v.retired && (v.acknowledged_generation < row.generation || v.active_requests != 0)
            })
            .count();
        Ok(
            json!({"id":row.id,"cluster_id":row.cluster_id,"deployment":row.deployment,"generation":row.generation,
            "state":row.state,"observed_at":row.observed_at,"observation_code":row.observation_code,
            "ready_until":row.ready_until,"deployment_uid":row.deployment_uid,"desired_version":row.desired_version,
            "participants":participants.iter().map(|v| json!({"session_id":v.session_id,"acknowledged_generation":v.acknowledged_generation,
                "active_requests":v.active_requests,"reported_at":v.reported_at,"retired":v.retired})).collect::<Vec<_>>(),
            "blocked_participants":blocked,"can_finish_drain":row.state=="draining" && blocked==0}),
        )
    }

    pub async fn pool_transition(
        &self,
        id: &str,
        expected: i64,
        action: &str,
        actor: &str,
    ) -> ServiceResult<Value> {
        let tx = self.db.begin().await?;
        let row = pool::Entity::find_by_id(id)
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if row.generation != expected {
            return Err(conflict("pool generation changed"));
        }
        if row.scale_operation.is_some() { return Err(conflict("automatic scaling operation is in progress")); }
        let time = now(&tx).await?;
        let state = match action {
            "drain" if matches!(row.state.as_str(), "active" | "pending") => "draining",
            "finish-drain" if row.state == "draining" => {
                let views = view::Entity::find()
                    .filter(view::Column::PoolId.eq(id))
                    .all(&tx)
                    .await?;
                if views.iter().any(|v| {
                    !v.retired
                        && (v.acknowledged_generation < row.generation || v.active_requests != 0)
                }) {
                    return Err(conflict(
                        "gateway ACK or in-flight completion is missing; offline is not drained",
                    ));
                }
                "drained"
            }
            "activate"
                if row.state == "drained" && row.ready_until.is_some_and(|until| until > time) =>
            {
                "active"
            }
            _ => {
                return Err(conflict(
                    "invalid pool state or missing fresh readiness proof",
                ));
            }
        };
        // Completion is the same closed generation. Reopening creates a new one.
        let generation = if action == "finish-drain" {
            row.generation
        } else {
            row.generation
                .checked_add(1)
                .ok_or_else(|| invalid("pool generation exhausted"))?
        };
        let mut active: pool::ActiveModel = row.into();
        active.generation = Set(generation);
        active.state = Set(state.into());
        active.updated_at = Set(time);
        active.observation_nonce = Set(None);
        active.observation_until = Set(None);
        active.update(&tx).await?;
        audit(
            &tx,
            actor,
            &format!("pool.{action}"),
            id,
            json!({"generation":generation,"state":state}),
        )
        .await?;
        tx.commit().await?;
        Ok(json!({"id":id,"generation":generation,"state":state}))
    }

    pub async fn observation_tasks(&self, id: &str, token: &str) -> ServiceResult<Value> {
        let tx = self.db.begin().await?;
        let cluster = authenticated(&tx, id, token).await?;
        let time = now(&tx).await?;
        let rows = pool::Entity::find()
            .filter(pool::Column::ClusterId.eq(id))
            .filter(pool::Column::State.ne("retired"))
            .order_by_asc(pool::Column::UpdatedAt)
            .lock_exclusive()
            .limit(100)
            .all(&tx)
            .await?;
        let mut tasks = Vec::new();
        for row in rows {
            if cluster.acknowledged_version < row.desired_version {
                continue;
            }
            let binding: PoolBinding = decode(row.binding.clone())?;
            let nonce = uuid::Uuid::now_v7().to_string();
            tasks.push(ObservationTask {
                idle_after: crate::gateway_proofs::idle_after(&tx, &row.id).await?,
                pool_id: row.id.clone(),
                generation: row.generation,
                nonce: nonce.clone(),
                desired_version: row.desired_version,
                deployment: row.deployment.clone(),
                serving_service: binding.serving_service,
                expected_uid: row.deployment_uid.clone(),
                expected_spec: row.expected_spec.clone(),
            });
            let mut active: pool::ActiveModel = row.into();
            active.observation_nonce = Set(Some(nonce));
            active.observation_until = Set(Some(time + chrono::Duration::seconds(60)));
            active.update(&tx).await?;
        }
        tx.commit().await?;
        Ok(json!({"tasks":tasks}))
    }
    pub async fn observe_pool(
        &self,
        id: &str,
        token: &str,
        report: PoolObservation,
    ) -> ServiceResult<Value> {
        if !matches!(
            report.code.as_str(),
            "ok" | "not_ready" | "spec_drift" | "identity_mismatch" | "kubernetes_unavailable"
        ) || report.ready != (report.code == "ok")
            || report
                .deployment_uid
                .as_ref()
                .is_some_and(|uid| uid.is_empty() || uid.len() > 128)
            || (report.ready && report.deployment_uid.is_none())
        {
            return Err(invalid("invalid readiness observation"));
        }
        let tx = self.db.begin().await?;
        let cluster = authenticated(&tx, id, token).await?;
        let row = pool::Entity::find_by_id(&report.pool_id)
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let time = now(&tx).await?;
        if row.cluster_id != id {
            return Err(ServiceError::Forbidden);
        }
        if row.generation != report.generation
            || row.observation_nonce.as_deref() != Some(&report.nonce)
            || row.observation_until.is_none_or(|until| until <= time)
            || cluster.acknowledged_version < row.desired_version
        {
            return Err(conflict("stale observation generation or lease"));
        }
        if report.deployment_uid.is_some()
            && row
                .deployment_uid
                .as_ref()
                .is_some_and(|uid| Some(uid) != report.deployment_uid.as_ref())
        {
            return Err(conflict(
                "deployment identity was replaced; rebind explicitly",
            ));
        }
        let mut active: pool::ActiveModel = row.clone().into();
        active.observed_at = Set(Some(time));
        active.observation_code = Set(report.code);
        active.observation_nonce = Set(None);
        active.observation_until = Set(None);
        active.ready_until = Set(if report.ready {
            Some(time + chrono::Duration::seconds(45))
        } else {
            None
        });
        if report.deployment_uid.is_some() && row.deployment_uid.is_none() {
            active.deployment_uid = Set(report.deployment_uid);
        }
        if report.ready && row.state == "pending" {
            active.state = Set("active".into());
            active.generation = Set(row
                .generation
                .checked_add(1)
                .ok_or_else(|| invalid("pool generation exhausted"))?);
            audit(
                &tx,
                &format!("cluster:{id}"),
                "pool.registered",
                &row.id,
                json!({"generation":row.generation+1}),
            )
            .await?;
            let binding: PoolBinding = decode(row.binding.clone())?;
            if binding.make_default {
                use sea_orm::sea_query::{Expr, ExprTrait};
                use xscope_entities::model_catalog;
                let endpoint: ServingEndpoint = decode(row.endpoint.clone())?;
                model_catalog::Entity::update_many()
                    .col_expr(
                        model_catalog::Column::DefaultPool,
                        Expr::value(row.id.clone()),
                    )
                    .col_expr(
                        model_catalog::Column::Revision,
                        Expr::col(model_catalog::Column::Revision).add(1),
                    )
                    .col_expr(model_catalog::Column::UpdatedAt, Expr::value(time))
                    .filter(model_catalog::Column::Id.eq(endpoint.model))
                    .filter(model_catalog::Column::Enabled.eq(true))
                    .filter(model_catalog::Column::DefaultPool.is_null())
                    .exec(&tx)
                    .await?;
            }
        }
        // Readiness observations NEVER reopen a draining/drained pool.
        active.updated_at = Set(time);
        let updated=active.update(&tx).await?;
        if report.idle { crate::gateway_proofs::confirm_idle(&tx,&updated).await?; }
        let changed=crate::scaling::reconcile(&tx,&cluster,&updated, report.scaling, report.applied_replicas, report.ready).await?;
        tx.commit().await?;
        Ok(json!({"accepted":true,"replicas_changed":changed}))
    }
}

/// Caller holds the cluster lock. Managed revisions are immutable; replicas
/// may change only after every exposed Gateway incarnation has proven drain.
pub(crate) async fn guard_desired(
    tx: &DatabaseTransaction,
    id: &str,
    version: i64,
    deployments: &mut [DesiredDeployment],
) -> ServiceResult<()> {
    let rows = pool::Entity::find()
        .filter(pool::Column::ClusterId.eq(id))
        .order_by_asc(pool::Column::Id)
        .lock_exclusive()
        .all(tx)
        .await?;
    for row in rows {
        let Some(item) = deployments.iter_mut().find(|d| d.name == row.deployment) else {
            continue;
        };
        if row.state == "retired" {
            if item.spec.is_none() && item.delete_uid == row.deployment_uid {
                continue;
            }
            return Err(conflict(
                "retired pool cannot be resurrected; use a new deployment identity",
            ));
        }
        if item.spec.as_ref() == Some(&row.expected_spec) {
            item.expected_uid = row.deployment_uid.clone();
            continue;
        }
        if row.state != "drained" {
            return Err(conflict(
                "managed deployment must finish pool drain before mutation",
            ));
        }
        if row.scale_operation.is_some() { return Err(conflict("automatic scaling owns the pending replica change")); }
        let uid = row
            .deployment_uid
            .clone()
            .ok_or_else(|| conflict("deployment identity proof missing"))?;
        if let Some(spec) = &item.spec {
            let mut compare = spec.clone();
            compare["replicas"] = row.expected_spec["replicas"].clone();
            if compare != row.expected_spec
                || !spec["replicas"]
                    .as_i64()
                    .is_some_and(|n| (0..=10000).contains(&n))
            {
                return Err(invalid(
                    "managed pool model/runtime identity is immutable; create a new deployment for a new revision",
                ));
            }
            item.expected_uid = Some(uid);
        } else if item.delete_uid.as_deref() != Some(&uid) {
            return Err(conflict("delete UID differs from observed deployment"));
        }
        let mut active: pool::ActiveModel = row.clone().into();
        active.generation = Set(row
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("pool generation exhausted"))?);
        active.desired_version = Set(version);
        active.ready_until = Set(None);
        active.observation_nonce = Set(None);
        active.observation_until = Set(None);
        if let Some(spec) = &item.spec {
            active.expected_spec = Set(spec.clone());
        } else {
            active.state = Set("retired".into());
        }
        active.updated_at = Set(now(tx).await?);
        active.update(tx).await?;
    }
    Ok(())
}
