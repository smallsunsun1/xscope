//! Alertmanager is responsible for grouping/retry; PostgreSQL is the durable
//! in-console notification sink. Acknowledgement never resolves an alert.
use crate::{
    billing::invalid,
    clusters::{audit, now},
    error::{ServiceError, ServiceResult},
    repository::Repository,
};
use chrono::{DateTime, FixedOffset, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter,
    QueryOrder, QuerySelect, TransactionTrait,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use xscope_entities::ops_alert as alert;

#[derive(Deserialize)]
pub struct Webhook {
    pub alerts: Vec<Alert>,
    #[serde(default, rename = "truncatedAlerts")]
    pub truncated_alerts: u64,
}
#[derive(Deserialize)]
pub struct Alert {
    pub status: String,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    #[serde(rename = "startsAt")]
    pub starts_at: DateTime<FixedOffset>,
    #[serde(rename = "endsAt")]
    pub ends_at: DateTime<FixedOffset>,
    pub fingerprint: String,
}
impl Alert {
    fn validate(&self) -> ServiceResult<()> {
        if !matches!(self.status.as_str(), "firing" | "resolved")
            || self.fingerprint.len() != 16
            || !self.fingerprint.bytes().all(|v| v.is_ascii_hexdigit())
            || !self
                .labels
                .get("alertname")
                .is_some_and(|v| v.starts_with("XScope") && v.len() <= 128)
            || self.labels.len() > 32
            || self.annotations.len() > 16
            || self
                .labels
                .iter()
                .any(|(k, v)| k.len() > 128 || v.len() > 2048)
            || self
                .annotations
                .iter()
                .any(|(k, v)| k.len() > 128 || v.len() > 4096)
            || self.starts_at > Utc::now() + chrono::Duration::minutes(5)
            || (self.status == "resolved" && self.ends_at < self.starts_at)
        {
            return Err(invalid("invalid bounded XScope alert notification"));
        }
        Ok(())
    }
    fn id(&self) -> String {
        format!(
            "{:x}",
            Sha256::digest(format!(
                "{}:{}",
                self.fingerprint.to_lowercase(),
                self.starts_at.timestamp_millis()
            ))
        )
    }
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertQuery {
    pub after: Option<String>,
    pub state: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acknowledge {
    pub reason: String,
}

pub async fn backup_status(base: Option<&str>) -> ServiceResult<Value> {
    let unavailable = || ServiceError::Dependency("backup telemetry unavailable".into());
    let base = base.ok_or_else(unavailable)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| unavailable())?;
    let mut url = reqwest::Url::parse(&format!("{}/api/v1/query", base.trim_end_matches('/')))
        .map_err(|_| unavailable())?;
    url.query_pairs_mut().append_pair("query", r#"{__name__=~"kube_cronjob_status_last_successful_time|kube_cronjob_status_last_schedule_time|kube_cronjob_status_active|kube_cronjob_spec_suspend|kube_cronjob_created",namespace="xscope-system",cronjob="xscope-business-backup"}"#);
    let mut response = client.get(url).send().await.map_err(|_| unavailable())?;
    if !response.status().is_success() {
        return Err(unavailable());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
        if bytes.len() + chunk.len() > 65536 {
            return Err(unavailable());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
    if value["status"] != "success" || value["data"]["resultType"] != "vector" {
        return Err(unavailable());
    }
    let rows = value["data"]["result"].as_array().ok_or_else(unavailable)?;
    let mut result = json!({"last_successful_at":null,"last_scheduled_at":null,"active_jobs":null,"suspended":null,"created_at":null,"restore_verified_by_schedule":false,"offsite":false,"pitr":false});
    for (metric, field) in [
        (
            "kube_cronjob_status_last_successful_time",
            "last_successful_at",
        ),
        (
            "kube_cronjob_status_last_schedule_time",
            "last_scheduled_at",
        ),
        ("kube_cronjob_status_active", "active_jobs"),
        ("kube_cronjob_spec_suspend", "suspended"),
        ("kube_cronjob_created", "created_at"),
    ] {
        let values: Vec<_> = rows
            .iter()
            .filter(|r| r["metric"]["__name__"] == metric)
            .collect();
        if values.len() > 1 {
            return Err(unavailable());
        }
        if let Some(row) = values.first() {
            let number = row["value"][1]
                .as_str()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|n| n.is_finite() && *n >= 0.)
                .ok_or_else(unavailable)?;
            result[field] = json!(number);
        }
    }
    result["telemetry_present"] = json!(!result["created_at"].is_null());
    Ok(result)
}
impl Repository {
    pub async fn receive_alerts(&self, request: Webhook) -> ServiceResult<()> {
        if request.alerts.len() > 100 || request.truncated_alerts != 0 {
            return Err(invalid(
                "alert batch must contain at most 100 complete alerts",
            ));
        }
        for row in &request.alerts {
            row.validate()?;
        }
        // Deterministic ordering avoids deadlocks between overlapping batches.
        let mut rows: BTreeMap<String, &Alert> = BTreeMap::new();
        for row in &request.alerts {
            rows.entry(row.id())
                .and_modify(|prior| {
                    if row.status == "resolved" {
                        *prior = row;
                    }
                })
                .or_insert(row);
        }
        let tx = self.db.begin().await?;
        let time = now(&tx).await?;
        for (id, row) in rows {
            let model = alert::ActiveModel {
                id: Set(id.clone()),
                name: Set(row.labels["alertname"].clone()),
                severity: Set(row
                    .labels
                    .get("severity")
                    .filter(|v| matches!(v.as_str(), "critical" | "warning" | "info"))
                    .cloned()
                    .unwrap_or_else(|| "warning".into())),
                summary: Set(row.annotations.get("summary").cloned().unwrap_or_default()),
                state: Set(row.status.clone()),
                starts_at: Set(row.starts_at),
                ends_at: Set((row.status == "resolved").then_some(row.ends_at)),
                received_at: Set(time),
                acknowledged_by: Set(None),
                acknowledged_at: Set(None),
            };
            alert::Entity::insert(model)
                .on_conflict(
                    OnConflict::column(alert::Column::Id)
                        .do_nothing()
                        .to_owned(),
                )
                .try_insert()
                .exec(&tx)
                .await?;
            let old = alert::Entity::find_by_id(&id)
                .lock_exclusive()
                .one(&tx)
                .await?
                .ok_or(ServiceError::NotFound)?;
            // Late firing / duplicate delivery cannot undo resolution or ACK.
            if row.status == "resolved" && old.state != "resolved" {
                let mut active = old.into_active_model();
                active.state = Set("resolved".into());
                active.ends_at = Set(Some(row.ends_at));
                active.received_at = Set(time);
                active.update(&tx).await?;
            }
        }
        tx.commit().await?;
        xscope_telemetry::background_event("alert_delivery", "committed");
        Ok(())
    }
    pub async fn ops_alerts(&self, query: AlertQuery) -> ServiceResult<Value> {
        if query
            .state
            .as_ref()
            .is_some_and(|v| !matches!(v.as_str(), "firing" | "resolved"))
            || query
                .after
                .as_ref()
                .is_some_and(|v| v.len() != 64 || !v.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(invalid("invalid alert page"));
        }
        let mut select = alert::Entity::find();
        if let Some(state) = query.state {
            select = select.filter(alert::Column::State.eq(state));
        }
        if let Some(after) = query.after {
            select = select.filter(alert::Column::Id.gt(after));
        }
        let mut rows = select
            .order_by_asc(alert::Column::Id)
            .limit(51)
            .all(&self.db)
            .await?;
        let more = rows.len() > 50;
        rows.truncate(50);
        Ok(json!({"next":if more { rows.last().map(|r| r.id.clone()) } else { None },"data":rows}))
    }
    pub async fn acknowledge_alert(
        &self,
        id: &str,
        actor: &str,
        request: Acknowledge,
    ) -> ServiceResult<Value> {
        if request.reason.trim().is_empty() || request.reason.len() > 1024 {
            return Err(invalid("bounded acknowledgement reason required"));
        }
        let tx = self.db.begin().await?;
        let row = alert::Entity::find_by_id(id)
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if row.acknowledged_at.is_none() {
            let mut active = row.into_active_model();
            active.acknowledged_at = Set(Some(now(&tx).await?));
            active.acknowledged_by = Set(Some(actor.into()));
            active.update(&tx).await?;
            audit(
                &tx,
                actor,
                "alert.acknowledge",
                id,
                json!({"reason":request.reason}),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(json!({"acknowledged":true,"resolution_not_implied":true}))
    }
}
