use super::conditions::{
    error_condition,
    success_condition,
};
use crate::{
    context::Context,
    dns::cloudflare::{
        self,
        Zone,
    },
    dns_check::DnsCheckRequest,
    resources::{
        CloudflareDNSRecord,
        CloudflareDNSRecordStatus,
        ZoneNameOrId,
    },
};
use eyre::{
    Context as _,
    OptionExt as _,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::{
    api::{
        ObjectMeta,
        Patch,
        PatchParams,
    },
    runtime::wait,
    Api,
};
use serde::{
    Deserialize,
    Serialize,
};
use std::sync::Arc;

#[derive(thiserror::Error, Debug)]
pub enum ReconcileError {
    #[error(transparent)]
    Kube(#[from] kube::Error),

    #[error(transparent)]
    Deletion(#[from] wait::delete::Error),

    #[error(transparent)]
    Other(#[from] eyre::Error),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct AnnotationContent {
    record_id: String,
    api_token: String,
    zone_id: String,
}

pub async fn apply(resource: Arc<CloudflareDNSRecord>, ctx: Arc<Context>) -> Result<(), ReconcileError> {
    let client = &ctx.client;
    let ns = resource.metadata.namespace.as_deref().unwrap_or("default");
    let name = resource.metadata.name.as_deref().ok_or_eyre("missing name")?;
    let is_new = resource.status.is_none();
    let gen = resource.metadata.generation;

    info!("processing reconcile request");

    // If a record exists with a different name, we need to delete it first.
    let domain_or_record_text = resource.spec.name.as_str();
    let api = Api::<CloudflareDNSRecord>::namespaced(client.clone(), ns);
    if let Some(existing) = api.get_opt(name).await? {
        if existing.spec.name != domain_or_record_text {
            warn!(
                "conflict: CloudflareDNSRecord {ns}/{name} already exists with a different name, deleting old record"
            );
            wait::delete::delete_and_finalize(api, name, &Default::default()).await?;
        }
    }

    let Some(content) = resource.spec.lookup_content(client, ns).await? else {
        let msg = format!("unable to resolve content for CloudflareDNSRecord {ns}/{name}");
        error!("{msg}");
        update_conditions(
            &resource,
            &ctx,
            vec![error_condition(&resource, "missing content", msg, gen)],
        )
        .await?;
        return Ok(());
    };

    let zone = match &resource.spec.zone {
        ZoneNameOrId::Name(it) => it.lookup(client, ns).await?.map(Zone::name),
        ZoneNameOrId::Id(it) => it.lookup(client, ns).await?.map(Zone::id),
    };

    let Some(zone) = zone else {
        let msg = format!(
            "unable to resolve {:?} for CloudflareDNSRecord {ns}/{name}",
            resource.spec.zone
        );
        error!("{msg}");
        update_conditions(
            &resource,
            &ctx,
            vec![error_condition(&resource, "missing zone", msg, gen)],
        )
        .await?;
        return Ok(());
    };

    let Some(zone) = zone.resolve(&ctx.cloudflare_api).await? else {
        let msg = format!("unable to resolve zone for CloudflareDNSRecord {ns}/{name}");
        error!("{msg}");
        update_conditions(
            &resource,
            &ctx,
            vec![error_condition(&resource, "missing zone", msg, gen)],
        )
        .await?;
        return Ok(());
    };
    let Zone::Identifier(zone_id) = zone.clone() else {
        unreachable!();
    };

    debug!("updating dns record for CloudflareDNSRecord {ns}/{name}");

    let record = ctx
        .cloudflare_api
        .update_dns_record_and_wait(cloudflare::CreateRecordArgs {
            zone,
            name: domain_or_record_text.to_string(),
            record_type: resource.spec.ty.unwrap_or_default(),
            content,
            comment: resource.spec.comment.clone(),
            ttl: resource.spec.ttl,
        })
        .await?;

    // -=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-

    let status_key = format!("{ns}:{name}");

    let pending = if ctx.do_dns_check {
        !ctx.dns_lookup_success
            .lock()
            .await
            .get(&status_key)
            .cloned()
            .unwrap_or_default()
    } else {
        false
    };
    let condition = if !pending {
        success_condition(&resource, gen)
    } else {
        let msg = "The DNS record has not propagated yet. This is expected to take some time.".to_string();
        error_condition(&resource, "pending", msg, gen)
    };

    let patched = CloudflareDNSRecord {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        spec: resource.spec.clone(),
        status: Some(CloudflareDNSRecordStatus {
            // We are storing the details about how we created the record in the
            // status. At deletion time, the configmap / secrets we got the
            // zone_id from might be gone already.
            record_id: record.id,
            zone_id,
            pending,
            conditions: Some(vec![condition]),
        }),
    };

    if is_new && ctx.do_dns_check {
        let _ = ctx
            .dns_check_tx
            .send(DnsCheckRequest::CheckSingleRecord {
                name: name.to_string(),
                namespace: ns.to_string(),
            })
            .await;
    }

    Api::<CloudflareDNSRecord>::namespaced(client.clone(), ns)
        .patch(name, &PatchParams::apply("dns.cloudflare.com"), &Patch::Apply(&patched))
        .await
        .context("unable to patch CloudflareDNSRecord with record details")?;

    Api::<CloudflareDNSRecord>::namespaced(client.clone(), ns)
        .patch_status(name, &PatchParams::apply("dns.cloudflare.com"), &Patch::Apply(&patched))
        .await
        .context("unable to patch CloudflareDNSRecord with record details")?;

    Ok(())
}

/// This functions runs before the resource is deleted. It'll try to delete the DNS record from Cloudflare.
#[instrument(level = "debug", skip_all)]
pub async fn cleanup(resource: Arc<CloudflareDNSRecord>, ctx: Arc<Context>) -> Result<(), ReconcileError> {
    let ns = resource.metadata.namespace.as_deref().unwrap_or("default");
    let name = resource.metadata.name.as_deref().ok_or_eyre("missing name")?;

    info!("delete request: CloudflareDNSRecord {ns}/{name}");

    let Some(status) = resource.status.as_ref() else {
        error!("missing status for CloudflareDNSRecord {ns}/{name}");
        return Ok(());
    };

    if let Err(err) = ctx
        .cloudflare_api
        .delete_dns_record(&status.zone_id, &status.record_id)
        .await
    {
        error!("Unable to delete dns record for cloudflare: {err}");
    }

    Ok(())
}

pub async fn update_conditions(
    resource: &CloudflareDNSRecord,
    ctx: &Context,
    conditions: Vec<Condition>,
) -> Result<(), ReconcileError> {
    let name = resource.metadata.name.as_deref().ok_or_eyre("missing name")?;
    let ns = resource.metadata.namespace.as_deref().unwrap_or("default");
    let status = resource.status.clone().unwrap_or_default();

    let patched = CloudflareDNSRecord {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        spec: resource.spec.clone(),
        status: Some(CloudflareDNSRecordStatus {
            conditions: Some(conditions),
            ..status
        }),
    };

    Api::<CloudflareDNSRecord>::namespaced(ctx.client.clone(), ns)
        .patch_status(name, &PatchParams::apply("dns.cloudflare.com"), &Patch::Apply(&patched))
        .await
        .context("unable to patch CloudflareDNSRecord with record details")?;

    Ok(())
}
