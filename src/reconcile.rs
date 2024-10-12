use super::ControllerState;
use crate::{
    dns::cloudflare,
    resources::{
        CloudflareDNSRecord,
        StringOrService,
    },
    services::public_ip_from_service,
};
use eyre::{
    Context as _,
    OptionExt as _,
    Result,
};
use k8s_openapi::api::core::v1::Secret;
use kube::{
    api::{
        ObjectMeta,
        Patch,
        PatchParams,
    },
    Api,
    Resource,
};
use serde::{
    Deserialize,
    Serialize,
};
use std::{
    collections::BTreeMap,
    sync::Arc,
};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct AnnotationContent {
    record_id: String,
    api_token: String,
    zone_id: String,
}

const SECRET_FINALIZER: &str = "dns.cloudflare.com/delete-dns-record";

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

    let Some(api_token) = &resource.spec.api_token.lookup(client, ns).await? else {
        error!("api token not found for CloudflareDNSRecord {ns}/{name}");
        return Ok(());
    };

    let Some(zone_id) = &resource.spec.zone_id.lookup(client, ns).await? else {
        error!("zone id not found for CloudflareDNSRecord {ns}/{name}");
        return Ok(());
    };

    debug!("updating dns record for CloudflareDNSRecord {ns}/{name}");

    let record = cloudflare::update_dns_record_and_wait(cloudflare::CreateRecordArgs {
        api_token: api_token.clone(),
        zone_identifier: zone_id.clone(),
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

    let oref = resource.controller_owner_ref(&()).unwrap();
    let secret_name = format!("{name}-for-cloudflare-dns-record");
    let secret = Secret {
        metadata: ObjectMeta {
            owner_references: Some(vec![oref]),
            name: Some(secret_name.clone()),
            namespace: Some(ns.to_string()),
            finalizers: Some(vec![SECRET_FINALIZER.to_string()]),
            ..Default::default()
        },
        string_data: Some(BTreeMap::from_iter(vec![
            ("api_token".to_string(), api_token.clone()),
            ("record_id".to_string(), record.id.clone()),
            ("zone_id".to_string(), zone_id.clone()),
        ])),
        ..Default::default()
    };

    Api::<Secret>::namespaced(client.clone(), ns)
        .patch(
            &secret_name,
            &PatchParams::apply("dns.cloudflare.com"),
            &Patch::Apply(&secret),
        )
        .await
        .context("unable to create secret for storing cloudflare record details")?;

    Ok(())
}

/// This functions runs before the resource is deleted. It'll try to delete the DNS record from Cloudflare.
// #[instrument(level = "debug", skip_all)]
pub async fn cleanup(resource: Arc<CloudflareDNSRecord>, ctx: Arc<ControllerState>) -> Result<()> {
    let client = &ctx.client;

    let ns = resource.metadata.namespace.as_deref().unwrap_or("default");
    let name = resource.metadata.name.as_deref().ok_or_eyre("missing name")?;
    let secret_name = format!("{name}-for-cloudflare-dns-record");

    info!("delete request: CloudflareDNSRecord {ns}/{name}");

    let secret_api = Api::<Secret>::namespaced(client.clone(), ns);

    let mut failed = false;
    match secret_api.get(&secret_name).await {
        Ok(secret) => {
            let data = secret.data;
            if let (Some(api_token), Some(record_id), Some(zone_id)) = (
                data.as_ref()
                    .and_then(|data| data.get("api_token"))
                    .and_then(|data| String::from_utf8(data.0.clone()).ok()),
                data.as_ref()
                    .and_then(|data| data.get("record_id"))
                    .and_then(|data| String::from_utf8(data.0.clone()).ok()),
                data.as_ref()
                    .and_then(|data| data.get("zone_id"))
                    .and_then(|data| String::from_utf8(data.0.clone()).ok()),
            ) {
                if let Err(err) = cloudflare::delete_dns_record(&zone_id, &record_id, &api_token).await {
                    error!("Unable to delete dns record for cloudflare: {err}");
                    failed = true;
                }
            } else {
                error!("missing data in secret for cloudflare record details");
                failed = true;
            }

            // remove the finalizer
            Api::<Secret>::namespaced(client.clone(), ns)
                .patch(
                    &secret_name,
                    &PatchParams::apply("dns.cloudflare.com"),
                    &Patch::Apply(
                        &(Secret {
                            metadata: ObjectMeta {
                                name: Some(secret_name.clone()),
                                namespace: Some(ns.to_string()),
                                finalizers: secret.metadata.finalizers.as_ref().map(|finalizers| {
                                    finalizers
                                        .iter()
                                        .filter(|f| f.as_str() != SECRET_FINALIZER)
                                        .cloned()
                                        .collect()
                                }),
                                ..Default::default()
                            },
                            ..Default::default()
                        }),
                    ),
                )
                .await
                .context("unable to create secret for storing cloudflare record details")?;
        }
        Err(err) => {
            error!("Unable to lookup secret for cloudflare record details: {err}");
            failed = true;
        }
    };
    if failed {
        error!("This means we are unable to delete the dns record, please do so manually");
    }

    Ok(())
}
