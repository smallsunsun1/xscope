//! KEDA owns only desired recommendations; it cannot delete runtime replicas.
use crate::{Error, api::ModelDeployment, controller::ensure_owner};
use kube::{Api, Client, CustomResource, ResourceExt, api::{PostParams, Patch, PatchParams}};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize)]
#[kube(group="platform.xscope.io",version="v1alpha1",kind="ModelScale",plural="modelscales",namespaced,status="ModelScaleStatus",schema="disabled")]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct ModelScaleSpec { pub model_uid: String, pub replicas: i32 }
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all="camelCase")]
pub struct ModelScaleStatus { pub replicas: i32, pub selector: String }
pub fn enabled(m: &ModelDeployment) -> bool { m.spec.autoscaling.as_ref().is_some_and(|s|s.managed) }
pub fn target_kind(m: &ModelDeployment) -> &'static str { if enabled(m) { "ModelScale" } else { "ModelDeployment" } }

pub async fn sync(client: Client, model: &ModelDeployment, replicas: i32, selector: &str) -> Result<(),Error> {
    let namespace=model.namespace().ok_or_else(||Error::Invalid("namespace missing".into()))?;
    let api: Api<ModelScale>=Api::namespaced(client,&namespace);
    let old=match api.get_opt(&model.name_any()).await {
        Ok(v)=>v,
        Err(kube::Error::Api(e)) if e.code==404 && !enabled(model)=>return Ok(()),
        Err(e)=>return Err(e.into()),
    };
    if !enabled(model) { crate::resources::sync(&api,model,old,None).await?; return Ok(()); }
    let row=if let Some(row)=old {
        ensure_owner(&row,model)?;
        if Some(&row.spec.model_uid)!=model.metadata.uid.as_ref() { return Err(Error::OwnershipConflict); }
        row
    } else {
        let mut row=ModelScale::new(&model.name_any(),ModelScaleSpec{model_uid:model.uid().ok_or(Error::OwnershipConflict)?,replicas:model.spec.replicas});
        row.metadata.namespace=Some(namespace);
        row.metadata.owner_references=Some(vec![model.controller_owner_ref(&()).ok_or(Error::OwnershipConflict)?]);
        api.create(&PostParams::default(),&row).await?
    };
    let status=ModelScaleStatus{replicas,selector:selector.into()};
    if row.status.as_ref()!=Some(&status) {
        api.patch_status(&model.name_any(),&PatchParams::default(),&Patch::Merge(json!({"metadata":{"resourceVersion":row.metadata.resource_version},"status":status}))).await?;
    }
    Ok(())
}

pub async fn observe(client:Client, model:&ModelDeployment, applied_replicas:i32)->Result<Option<xscope_domain::traffic::ScaleEvidence>,Error> {
    if !enabled(model) { return Ok(None); }
    let ns=model.namespace().ok_or_else(||Error::Invalid("namespace missing".into()))?;
    let recommendations:Api<ModelScale>=Api::namespaced(client.clone(),&ns);
    let row=recommendations.get(&model.name_any()).await?;
    ensure_owner(&row,model)?;
    if Some(&row.spec.model_uid)!=model.metadata.uid.as_ref() { return Err(Error::OwnershipConflict); }
    let scalers:Api<crate::autoscaling::ScaledObject>=Api::namespaced(client.clone(),&ns);
    let scaled=scalers.get(&model.name_any()).await?;
    ensure_owner(&scaled,model)?;
    let hpas:Api<crate::resources::Hpa>=Api::namespaced(client,&ns);
    let hpa=hpas.get(&format!("keda-hpa-{}",model.name_any())).await?;
    if !crate::autoscaling::keda_owns_hpa(&hpa,&scaled) || !crate::autoscaling::hpa_targets_model(&hpa,model)
        || !scaled.status.as_ref().is_some_and(|s|s.conditions.iter().any(|c|c.type_=="Ready" && c.status=="True"))
        || !hpa.status.as_ref().is_some_and(|s|s.desired_replicas==row.spec.replicas && s.conditions.as_ref().is_some_and(|cs|
            ["ScalingActive","AbleToScale"].iter().all(|name|cs.iter().any(|c|c.type_==*name && c.status=="True")))) { return Ok(None); }
    Ok(Some(xscope_domain::traffic::ScaleEvidence{uid:row.uid().ok_or(Error::OwnershipConflict)?, resource_version:row.resource_version().ok_or(Error::OwnershipConflict)?, replicas:row.spec.replicas, applied_replicas}))
}
