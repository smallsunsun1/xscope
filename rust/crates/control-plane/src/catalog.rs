//! Catalog writes use optimistic revisions; price versions are immutable evidence.
use crate::{
    error::{ServiceError, ServiceResult},
    repository::{Repository, conflict_or_database},
};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, TransactionTrait,
};
use xscope_domain::{
    Model,
    catalog::{CatalogModel, CatalogSnapshot, PutModel, ServingEndpoint},
};
use xscope_entities::{
    model_catalog as catalog, model_price as price, serving_endpoint as endpoint,
};

fn decode(row: catalog::Model) -> ServiceResult<CatalogModel> {
    Ok(CatalogModel {
        revision: row.revision,
        enabled: row.enabled,
        default_pool: row.default_pool,
        model: serde_json::from_value(row.definition)
            .map_err(|_| ServiceError::Internal("invalid stored model definition".into()))?,
    })
}
fn json<T: serde::Serialize>(value: &T) -> ServiceResult<serde_json::Value> {
    serde_json::to_value(value)
        .map_err(|_| ServiceError::Internal("cannot encode catalog definition".into()))
}

pub(crate) async fn active_model(db: &impl ConnectionTrait, id: &str) -> ServiceResult<Model> {
    let row = catalog::Entity::find_by_id(id)
        .lock_shared()
        .one(db)
        .await?
        .filter(|r| r.enabled)
        .ok_or_else(|| ServiceError::Invalid("model is unknown or disabled".into()))?;
    Ok(decode(row)?.model)
}

impl Repository {
    /// Idempotent bootstrap never re-enables or overwrites an edited model.
    pub async fn bootstrap_catalog(&self) -> ServiceResult<()> {
        let model = Model::default();
        let tx = self.db.begin().await?;
        let now = chrono::Utc::now().fixed_offset();
        catalog::Entity::insert(catalog::ActiveModel {
            id: Set(model.id.clone()),
            revision: Set(1),
            enabled: Set(true),
            default_pool: Set(Some("demo-pool".into())),
            definition: Set(json(&model)?),
            updated_at: Set(now),
        })
        .on_conflict(
            OnConflict::column(catalog::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .try_insert()
        .exec(&tx)
        .await?;
        price::Entity::insert(price::ActiveModel {
            model_id: Set(model.id.clone()),
            version: Set(model.price_version.clone()),
            definition: Set(json(&model)?),
            created_at: Set(now),
        })
        .on_conflict(
            OnConflict::columns([price::Column::ModelId, price::Column::Version])
                .do_nothing()
                .to_owned(),
        )
        .try_insert()
        .exec(&tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn catalog(&self) -> ServiceResult<Vec<CatalogModel>> {
        catalog::Entity::find()
            .order_by_asc(catalog::Column::Id)
            .all(&self.db)
            .await?
            .into_iter()
            .map(decode)
            .collect()
    }
    pub async fn models(&self) -> ServiceResult<Vec<Model>> {
        Ok(self
            .catalog()
            .await?
            .into_iter()
            .filter(|r| r.enabled)
            .map(|r| r.model)
            .collect())
    }
    pub async fn model(&self, id: &str) -> ServiceResult<Model> {
        active_model(&self.db, id).await
    }
    pub async fn historical_price(&self, id: &str, version: &str) -> ServiceResult<Model> {
        let row = price::Entity::find_by_id((id.to_owned(), version.to_owned()))
            .one(&self.db)
            .await?
            .ok_or_else(|| ServiceError::Invalid("unknown model price version".into()))?;
        serde_json::from_value(row.definition)
            .map_err(|_| ServiceError::Internal("invalid stored price".into()))
    }
    pub async fn put_model(&self, id: &str, request: PutModel) -> ServiceResult<CatalogModel> {
        request.model.validate().map_err(ServiceError::Invalid)?;
        if id != request.model.id
            || !(0..i64::MAX).contains(&request.expected_revision)
            || request
                .default_pool
                .as_ref()
                .is_some_and(|id| !xscope_domain::catalog::identifier(id))
        {
            return Err(ServiceError::Invalid(
                "invalid model ID, pool or revision".into(),
            ));
        }
        let tx = self.db.begin().await?;
        let definition = json(&request.model)?;
        let revision = request.expected_revision + 1;
        let now = chrono::Utc::now().fixed_offset();
        if request.expected_revision == 0 {
            catalog::ActiveModel {
                id: Set(id.into()),
                revision: Set(revision),
                enabled: Set(request.enabled),
                default_pool: Set(request.default_pool.clone()),
                definition: Set(definition.clone()),
                updated_at: Set(now),
            }
            .insert(&tx)
            .await
            .map_err(conflict_or_database)?;
        } else {
            let result = catalog::Entity::update_many()
                .col_expr(catalog::Column::Revision, Expr::value(revision))
                .col_expr(catalog::Column::Enabled, Expr::value(request.enabled))
                .col_expr(
                    catalog::Column::DefaultPool,
                    Expr::value(request.default_pool.clone()),
                )
                .col_expr(catalog::Column::Definition, Expr::value(definition.clone()))
                .col_expr(catalog::Column::UpdatedAt, Expr::value(now))
                .filter(catalog::Column::Id.eq(id))
                .filter(catalog::Column::Revision.eq(request.expected_revision))
                .exec(&tx)
                .await?;
            if result.rows_affected != 1 {
                return Err(ServiceError::Conflict(
                    "model revision changed; reload before saving".into(),
                ));
            }
        }
        if let Some(previous) =
            price::Entity::find_by_id((id.to_owned(), request.model.price_version.clone()))
                .one(&tx)
                .await?
        {
            if previous.definition != definition {
                return Err(ServiceError::Conflict(
                    "price version is immutable; use a new version".into(),
                ));
            }
        } else {
            price::ActiveModel {
                model_id: Set(id.into()),
                version: Set(request.model.price_version.clone()),
                definition: Set(definition),
                created_at: Set(now),
            }
            .insert(&tx)
            .await
            .map_err(conflict_or_database)?;
        }
        tx.commit().await?;
        Ok(CatalogModel {
            revision,
            enabled: request.enabled,
            default_pool: request.default_pool,
            model: request.model,
        })
    }

    pub async fn serving_endpoints(&self) -> ServiceResult<Vec<ServingEndpoint>> {
        endpoint::Entity::find()
            .order_by_asc(endpoint::Column::Id)
            .all(&self.db)
            .await?
            .into_iter()
            .map(|r| {
                serde_json::from_value(r.definition)
                    .map_err(|_| ServiceError::Internal("invalid stored serving endpoint".into()))
            })
            .collect()
    }

    pub async fn serving_endpoint_versions(&self) -> ServiceResult<serde_json::Value> {
        let rows = endpoint::Entity::find()
            .order_by_asc(endpoint::Column::Id)
            .all(&self.db)
            .await?;
        Ok(
            serde_json::json!({"data": rows.into_iter().map(|row| serde_json::json!({
            "generation": row.generation, "endpoint": row.definition, "updated_at": row.updated_at
        })).collect::<Vec<_>>()}),
        )
    }
    pub async fn put_serving_endpoint(
        &self,
        id: &str,
        expected: i64,
        value: ServingEndpoint,
    ) -> ServiceResult<i64> {
        value.validate().map_err(ServiceError::Invalid)?;
        if value.id != id
            || xscope_domain::traffic::managed(id)
            || !(0..i64::MAX).contains(&expected)
        {
            return Err(ServiceError::Invalid(
                "invalid endpoint ID or generation".into(),
            ));
        }
        let tx = self.db.begin().await?;
        crate::managed_pools::registry_lock(&tx).await?;
        active_model(&tx, &value.model).await?;
        for row in xscope_entities::managed_pool::Entity::find()
            .all(&tx)
            .await?
        {
            let managed: ServingEndpoint = crate::managed_pools::decode(row.endpoint)?;
            if value.address == managed.address
                || (value.model == managed.model && value.revision == managed.revision)
            {
                return Err(ServiceError::Conflict(
                    "managed serving entries cannot be aliased through legacy routing".into(),
                ));
            }
        }
        let definition = json(&value)?;
        let now = chrono::Utc::now().fixed_offset();
        if expected == 0 {
            endpoint::ActiveModel {
                id: Set(id.into()),
                generation: Set(1),
                definition: Set(definition),
                updated_at: Set(now),
            }
            .insert(&tx)
            .await
            .map_err(conflict_or_database)?;
        } else {
            // Pool identity is immutable. A new model revision gets a new pool ID.
            let previous = endpoint::Entity::find_by_id(id)
                .lock_exclusive()
                .one(&tx)
                .await?
                .ok_or(ServiceError::NotFound)?;
            let old: ServingEndpoint = serde_json::from_value(previous.definition)
                .map_err(|_| ServiceError::Internal("invalid stored serving endpoint".into()))?;
            if previous.generation != expected
                || old.model != value.model
                || old.revision != value.revision
            {
                return Err(ServiceError::Conflict(
                    "endpoint generation or immutable model revision changed".into(),
                ));
            }
            endpoint::Entity::update_many()
                .col_expr(endpoint::Column::Generation, Expr::value(expected + 1))
                .col_expr(endpoint::Column::Definition, Expr::value(definition))
                .col_expr(endpoint::Column::UpdatedAt, Expr::value(now))
                .filter(endpoint::Column::Id.eq(id))
                .exec(&tx)
                .await?;
        }
        tx.commit().await?;
        Ok(expected + 1)
    }
    pub async fn catalog_snapshot(&self) -> ServiceResult<CatalogSnapshot> {
        Ok(CatalogSnapshot {
            models: self.catalog().await?,
            endpoints: self.serving_endpoints().await?,
        })
    }
    pub async fn route_pools(
        &self,
        bootstrap: &[xscope_domain::RoutePool],
    ) -> ServiceResult<Vec<xscope_domain::RoutePool>> {
        let mut pools: std::collections::BTreeMap<_, _> = bootstrap
            .iter()
            .cloned()
            .map(|p| (p.id.clone(), p))
            .collect();
        for endpoint in self.serving_endpoints().await? {
            pools.insert(
                endpoint.id.clone(),
                xscope_domain::RoutePool {
                    id: endpoint.id,
                    model: endpoint.model,
                    revision: endpoint.revision,
                },
            );
        }
        for row in xscope_entities::managed_pool::Entity::find()
            .filter(xscope_entities::managed_pool::Column::State.ne("retired"))
            .all(&self.db)
            .await?
        {
            let entry: ServingEndpoint = crate::managed_pools::decode(row.endpoint)?;
            pools.insert(
                entry.id.clone(),
                xscope_domain::RoutePool {
                    id: entry.id,
                    model: entry.model,
                    revision: entry.revision,
                },
            );
        }
        Ok(pools.into_values().collect())
    }
}
