//! Manual release operations are CAS writes, not proof of global Gateway ACK.
use crate::{
    error::{ServiceError, ServiceResult},
    repository::Repository,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use serde::Deserialize;
use serde_json::{Value, json};
use xscope_domain::{
    Project, PutRoutePolicy, RoutePolicy, RoutePolicySpec, RoutePool, routing::ReleaseAction,
};
use xscope_entities::{route_policy, route_revision};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryQuery {
    pub before: Option<i64>,
    pub limit: Option<u64>,
}

impl Repository {
    pub async fn route_history(
        &self,
        project: &str,
        model: &str,
        query: HistoryQuery,
    ) -> ServiceResult<Value> {
        let limit = query.limit.unwrap_or(20);
        if !(1..=100).contains(&limit) || query.before.is_some_and(|v| v <= 0) {
            return Err(ServiceError::Invalid("invalid release history page".into()));
        }
        let mut select = route_revision::Entity::find()
            .filter(route_revision::Column::ProjectId.eq(project))
            .filter(route_revision::Column::Model.eq(model));
        if let Some(before) = query.before {
            select = select.filter(route_revision::Column::Revision.lt(before));
        }
        let rows = select
            .order_by_desc(route_revision::Column::Revision)
            .limit(limit + 1)
            .all(&self.db)
            .await?;
        let more = rows.len() as u64 > limit;
        let rows: Vec<_> = rows.into_iter().take(limit as usize).collect();
        Ok(
            json!({"next_before": if more { rows.last().map(|r| r.revision) } else { None }, "data": rows}),
        )
    }
    pub async fn release_action(
        &self,
        project: &Project,
        model: &str,
        action: ReleaseAction,
        pools: &[RoutePool],
        actor: &str,
    ) -> ServiceResult<RoutePolicy> {
        let expected = action.expected_revision();
        let current = route_policy::Entity::find_by_id((project.id.clone(), model.to_owned()))
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if expected <= 0 || expected != current.revision {
            return Err(ServiceError::Conflict(
                "route policy revision changed".into(),
            ));
        }
        let spec: RoutePolicySpec = serde_json::from_value(current.spec)
            .map_err(|_| ServiceError::Internal("invalid stored route policy".into()))?;
        let next = match &action {
            ReleaseAction::Pause { .. } => spec.pause(),
            ReleaseAction::Promote { .. } => spec.promote().map_err(ServiceError::Invalid)?,
            ReleaseAction::Rollback {
                target_revision, ..
            } => {
                if *target_revision <= 0 || *target_revision >= expected {
                    return Err(ServiceError::Invalid(
                        "rollback requires an earlier recorded revision".into(),
                    ));
                }
                let target = route_revision::Entity::find_by_id((
                    project.id.clone(),
                    model.to_owned(),
                    *target_revision,
                ))
                .one(&self.db)
                .await?
                .ok_or(ServiceError::NotFound)?;
                serde_json::from_value(target.spec)
                    .map_err(|_| ServiceError::Internal("invalid stored route revision".into()))?
            }
        };
        // This CAS also fences a concurrent raw PUT. History and route commit together.
        self.put_route_policy(
            project,
            model,
            PutRoutePolicy {
                expected_revision: expected,
                spec: next,
            },
            pools,
            actor,
            action.name(),
        )
        .await
    }
}
