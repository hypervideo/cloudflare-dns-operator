use super::ControllerState;
use crate::{
    dns::cloudflare::{
        self,
        Zone,
    },
    resources::{
        CloudflareDNSRecord,
        StringOrService,
        ZoneNameOrId,
    },
    services::public_ip_from_service,
};
use eyre::{
    Context as _,
    OptionExt as _,
    Result,
};
use kube::{
    api::{
        ObjectMeta,
        Patch,
        PatchParams,
    },
    Api,
};
use serde::{
    Deserialize,
    Serialize,
};
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize, Clone)]
struct AnnotationContent {
    record_id: String,
    api_token: String,
    zone_id: String,
}

#[instrument(level = "debug", skip_all)]
pub async fn update(resource: Arc<CloudflareDNSRecord>, ctx: Arc<ControllerState>) -> Result<()> {
    let client = &ctx.client;
    let ns = resource.metadata.namespace.as_deref().unwrap_or("default");
    let name = resource.metadata.name.as_deref().ok_or_eyre("missing name")?;
    info!("reconcile request: CloudflareDNSRecord {ns}/{name}");

    let content = match &resource.spec.content {
        StringOrService::Value(value) => value.clone(),
        StringOrService::Service(selector) => {
            let ns = selector.namespace.as_deref().unwrap_or(ns);
            let name = selector.name.as_str();
            let record_type = resource.spec.type_;
            let Some(ip) = public_ip_from_service(client, name, ns, record_type).await? else {
                error!("no public ip found for service {ns}/{name}");
                return Ok(());
            };
            ip.to_string()
        }
    };

    let zone = match &resource.spec.zone {
        ZoneNameOrId::Name(it) => {
            let Some(name) = it.lookup(client, ns).await? else {
                error!("unable to resolve {it:?} for CloudflareDNSRecord {ns}/{name}");
                return Ok(());
            };
            Zone::name(name)
        }
        ZoneNameOrId::Id(it) => {
            let Some(id) = it.lookup(client, ns).await? else {
                error!("unable to resolve {it:?} for CloudflareDNSRecord {ns}/{name}");
                return Ok(());
            };
            Zone::id(id)
        }
    };

    let Some(zone) = zone.resolve(&ctx.cloudflare_api_token).await? else {
        error!("unable to resolve zone for CloudflareDNSRecord {ns}/{name}");
        return Ok(());
    };
    let Zone::Identifier(zone_id) = zone.clone() else {
        unreachable!();
    };

    debug!("updating dns record for CloudflareDNSRecord {ns}/{name}");

    let record = cloudflare::update_dns_record_and_wait(cloudflare::CreateRecordArgs {
        api_token: ctx.cloudflare_api_token.clone(),
        zone,
        name: resource.spec.name.clone(),
        record_type: resource.spec.type_.unwrap_or_default(),
        content,
        comment: resource.spec.comment.clone(),
        ttl: resource.spec.ttl,
    })
    .await?;

    // -=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-

    // We are storing the details about how we created the record in a secret. At deletion time, the configmap / secrets
    // we got the zone_id and api_token from might be gone already.
    let mut annotations = resource.metadata.annotations.as_ref().cloned().unwrap_or_default();
    annotations.insert("dns.cloudflare.com/record_id".to_string(), record.id.clone());
    annotations.insert("dns.cloudflare.com/zone_id".to_string(), zone_id.clone());

    let patched = CloudflareDNSRecord {
        metadata: ObjectMeta {
            annotations: Some(annotations),
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        spec: resource.spec.clone(),
    };
    Api::<CloudflareDNSRecord>::namespaced(client.clone(), ns)
        .patch(name, &PatchParams::apply("dns.cloudflare.com"), &Patch::Apply(&patched))
        .await
        .context("unable to patch CloudflareDNSRecord with record details")?;

    Ok(())
}

/// This functions runs before the resource is deleted. It'll try to delete the DNS record from Cloudflare.
#[instrument(level = "debug", skip_all)]
pub async fn cleanup(resource: Arc<CloudflareDNSRecord>, ctx: Arc<ControllerState>) -> Result<()> {
    let ns = resource.metadata.namespace.as_deref().unwrap_or("default");
    let name = resource.metadata.name.as_deref().ok_or_eyre("missing name")?;

    info!("delete request: CloudflareDNSRecord {ns}/{name}");

    let Some(annotations) = resource.metadata.annotations.as_ref() else {
        error!("missing annotations for CloudflareDNSRecord {ns}/{name}");
        return Ok(());
    };

    let Some(record_id) = annotations.get("dns.cloudflare.com/record_id") else {
        error!("missing record_id in annotations for CloudflareDNSRecord {ns}/{name}");
        return Ok(());
    };

    let Some(zone_id) = annotations.get("dns.cloudflare.com/zone_id") else {
        error!("missing zone_id in annotations for CloudflareDNSRecord {ns}/{name}");
        return Ok(());
    };

    if let Err(err) = cloudflare::delete_dns_record(zone_id, record_id, &ctx.cloudflare_api_token).await {
        error!("Unable to delete dns record for cloudflare: {err}");
    }

    Ok(())
}
