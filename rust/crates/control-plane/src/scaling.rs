//! Durable KEDA actuation. The scaler never writes a Runtime Deployment.
use crate::{billing::{invalid, conflict}, clusters::{audit,now}, error::ServiceResult, managed_pools::{decode,encode}};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter};
use serde::{Deserialize,Serialize};
use serde_json::json;
use sha2::{Digest,Sha256};
use xscope_entities::{managed_pool as pool,member_cluster as cluster,cluster_revision,gateway_pool_view as view};
use xscope_domain::{cluster::DesiredDeployment,traffic::ScaleEvidence};

#[derive(Clone,Deserialize,Serialize)]
pub struct Operation { pub phase:String, pub target:i32, pub from:i32, pub previous_version:i64, pub up:bool }

pub async fn reconcile(tx:&DatabaseTransaction, cluster:&cluster::Model, row:&pool::Model, evidence:Option<ScaleEvidence>, applied:i32, ready:bool)->ServiceResult<bool> {
    if row.expected_spec["autoscaling"]["managed"]!=true { return Ok(false); }
    let operation:Option<Operation>=row.scale_operation.clone().map(decode).transpose()?;
    if let Some(op)=&operation && op.phase=="applying" {
        if ready && applied==op.target && cluster.acknowledged_version>=row.desired_version {
            let mut active:pool::ActiveModel=row.clone().into(); active.scale_operation=Set(None);
            if !op.up { active.state=Set("active".into()); active.generation=Set(row.generation.checked_add(1).ok_or_else(||invalid("pool generation exhausted"))?); }
            active.update(tx).await?;
            audit(tx,"keda","pool.scale_completed",&row.id,json!({"replicas":op.target})).await?;
        }
        return Ok(false);
    }
    let Some(evidence)=evidence else {return Ok(false);};
    let minimum=row.expected_spec["autoscaling"]["minReplicas"].as_i64().unwrap_or(1);
    let maximum=row.expected_spec["autoscaling"]["maxReplicas"].as_i64().unwrap_or(0);
    if evidence.uid.is_empty() || evidence.uid.len()>128 || evidence.resource_version.is_empty() || evidence.resource_version.len()>128
        || i64::from(evidence.replicas)<minimum || i64::from(evidence.replicas)>maximum { return Err(invalid("invalid KEDA recommendation evidence")); }
    let current=row.expected_spec["replicas"].as_i64().and_then(|v|i32::try_from(v).ok()).ok_or_else(||invalid("stored replica count invalid"))?;
    if let Some(op)=operation {
        if row.state!="draining" || op.phase!="draining" { return Err(conflict("scaling operation state changed")); }
        if evidence.replicas>=current && ready {
            let mut active:pool::ActiveModel=row.clone().into(); active.state=Set("active".into()); active.scale_operation=Set(None);
            active.generation=Set(row.generation.checked_add(1).ok_or_else(||invalid("pool generation exhausted"))?);
            active.update(tx).await?;
            audit(tx,"keda","pool.scale_cancelled",&row.id,json!({"reason":"demand_recovered"})).await?;
            return Ok(false);
        }
        let participants=view::Entity::find().filter(view::Column::PoolId.eq(&row.id)).all(tx).await?;
        if participants.iter().any(|v| v.active_requests!=0 || (!v.retired && v.acknowledged_generation<row.generation)) {return Ok(false);}
        if !ready {return Ok(false);}
        return apply(tx,cluster,row,current,evidence.replicas,false).await;
    }
    if row.state!="active" || !ready || evidence.replicas==current {return Ok(false);}
    if evidence.replicas>current {return apply(tx,cluster,row,current,evidence.replicas,true).await;}
    let op=Operation{phase:"draining".into(),target:evidence.replicas,from:current,previous_version:row.desired_version,up:false};
    let mut active:pool::ActiveModel=row.clone().into();
    active.scale_operation=Set(Some(encode(&op)?)); active.state=Set("draining".into());
    active.generation=Set(row.generation.checked_add(1).ok_or_else(||invalid("pool generation exhausted"))?);
    active.update(tx).await?;
    audit(tx,"keda","pool.scale_draining",&row.id,json!({"from":current,"target":evidence.replicas,"recommendation_uid":evidence.uid})).await?;
    Ok(false)
}

async fn apply(tx:&DatabaseTransaction, cluster:&cluster::Model, row:&pool::Model, from:i32,target:i32,up:bool)->ServiceResult<bool> {
    if cluster.desired_version!=cluster.acknowledged_version {return Ok(false);}
    let uid=row.deployment_uid.clone().ok_or_else(||conflict("deployment UID evidence missing"))?;
    let previous=cluster_revision::Entity::find_by_id((cluster.id.clone(),cluster.desired_version)).one(tx).await?.ok_or_else(||conflict("desired revision missing"))?;
    let mut deployments:Vec<DesiredDeployment>=decode(previous.payload)?;
    let mut spec=row.expected_spec.clone(); spec["replicas"]=json!(target);
    let item=DesiredDeployment{name:row.deployment.clone(),spec:Some(spec.clone()),expected_uid:Some(uid),delete_uid:None};
    if let Some(existing)=deployments.iter_mut().find(|d|d.name==row.deployment) {*existing=item;} else {deployments.push(item);}
    let payload=encode(&deployments)?;
    let bytes=serde_json::to_vec(&payload).map_err(|_|invalid("invalid scale payload"))?;
    if bytes.len()>256*1024 {return Err(invalid("scale payload too large"));}
    let version=cluster.desired_version.checked_add(1).ok_or_else(||invalid("desired version exhausted"))?;
    let time=now(tx).await?;
    cluster_revision::ActiveModel{cluster_id:Set(cluster.id.clone()),version:Set(version),payload:Set(payload),sha256:Set(format!("{:x}",Sha256::digest(&bytes))),created_by:Set("keda".into()),created_at:Set(time)}.insert(tx).await?;
    let mut cluster:cluster::ActiveModel=cluster.clone().into();cluster.desired_version=Set(version);cluster.updated_at=Set(time);cluster.update(tx).await?;
    let op=Operation{phase:"applying".into(),target,from,previous_version:row.desired_version,up};
    let mut active:pool::ActiveModel=row.clone().into();active.expected_spec=Set(spec);active.desired_version=Set(version);
    active.scale_operation=Set(Some(encode(&op)?));active.observation_nonce=Set(None);active.observation_until=Set(None);
    if !up {active.state=Set("drained".into());active.ready_until=Set(None);active.generation=Set(row.generation.checked_add(1).ok_or_else(||invalid("pool generation exhausted"))?);}
    active.updated_at=Set(time);active.update(tx).await?;
    audit(tx,"keda","pool.scale_applied",&row.id,json!({"replicas":target,"desired_version":version,"drain_required":!up})).await?;
    Ok(true)
}
