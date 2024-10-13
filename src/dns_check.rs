use crate::{
    context::Context,
    dns::lookup as dns_lookup,
    resources::CloudflareDNSRecord,
};
use futures::Stream;
use kube::{
    api::ListParams,
    runtime::reflector::{
        Lookup,
        ObjectRef,
    },
    Api,
};
use std::{
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;

pub type DnsCheckSender = mpsc::Sender<DnsCheckRequest>;
pub type DnsCheckReceiver = mpsc::Receiver<DnsCheckRequest>;

pub enum DnsCheckRequest {
    CheckSingleRecord { name: String, namespace: String },
}

pub fn start_dns_check(
    ctx: Arc<Context>,
    mut dns_check_receiver: DnsCheckReceiver,
    check_interval: Option<Duration>,
) -> impl Stream<Item = ObjectRef<CloudflareDNSRecord>> + Send + 'static {
    async_stream::stream! {
        let Some(check_interval) = check_interval else {
            return;
        };

        let mut timer = tokio::time::interval(check_interval);
        let client = ctx.client.clone();

        loop {
            let resources = tokio::select! {
                _ = timer.tick() => {
                    let api = Api::<CloudflareDNSRecord>::all(client.clone());
                    match api.list(&ListParams::default()).await {
                        Ok(resources) => resources.into_iter().collect(),
                        Err(err) => {
                            error!("Failed to list CloudflareDNSRecord resources: {:?}", err);
                            continue;
                        }
                    }
                },

                Some(request) = dns_check_receiver.recv() => {
                    match request {
                        DnsCheckRequest::CheckSingleRecord { name, namespace } => {
                            trace!("Request to check single DNS record {}/{}", namespace, name);
                            let api = Api::<CloudflareDNSRecord>::namespaced(client.clone(), &namespace);
                            match api.get(&name).await {
                                Ok(resource) => vec![resource],
                                Err(err) => {
                                    error!("Failed to get CloudflareDNSRecord {}/{}: {}", namespace, name, err);
                                    continue;
                                }
                            }
                        },
                    }
                }
            };

            debug!("Checking DNS {} CloudflareDNSRecord resources", resources.len());

            for resource in resources {
                let Some(name) = resource.metadata.name.clone() else {
                    error!("Resource has no name: {:?}", resource);
                    continue;
                };
                let Some(ns) = resource.metadata.namespace.clone() else {
                    error!("Resource has no namespace: {:?}", resource);
                    continue;
                };

                let key = format!("{ns}:{name}");

                if resource.status.clone().is_none() {
                    // Status should be set on first reconcile
                    warn!("Resource {key:?} has not yet a status");
                    continue;
                };

                let qname = &resource.spec.name;

                let Some(content) = resource.spec.lookup_content(&ctx.client, &ns).await.ok().flatten() else {
                    error!("unable to resolve content for CloudflareDNSRecord {key:?}");
                    continue;
                };

                let ty = resource.spec.type_.unwrap_or_default();

                let dns_record_data = match dns_lookup::resolve(qname, ty).await {
                    Ok(Some(it)) => it,
                    Ok(None) => {
                        error!("Unable to resolve unsupported DNS record type: {ty:?} for {key:?}");
                        continue;
                    }
                    Err(err) => {
                        error!("Failed to resolve DNS record: {err:?} for {key:?}");
                        continue;
                    }
                };

                let matches = dns_record_data.contains(&content);

                trace!(?key, ?dns_record_data, ?content, "Matches DNS record?");
                let mut dns_lookup_success = ctx.dns_lookup_success.lock().await;
                let matched_before = dns_lookup_success.get(&key).cloned().unwrap_or(false);
                let changed = matched_before != matches;
                trace!(?key, ?matches, matched_before, changed, "DNS record matches");
                dns_lookup_success.insert(key, matches);

                if changed {
                    yield resource.to_object_ref(());
                }
            }
        }
    }
}
