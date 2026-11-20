use crate::resources::{
    CloudflareDNSRecord,
    RecordType,
    StringOrService,
};
use eyre::Result;
use k8s_openapi::api::core::v1::Service;
use kube::runtime::reflector::ObjectRef;
use std::{
    net::IpAddr,
    sync::Arc,
};

/// The records whose content comes from `svc`. They need a reconcile when the service changes, such as when it gets a
/// new public IP.
pub fn records_using_service(
    svc: &Service,
    records: &[Arc<CloudflareDNSRecord>],
) -> Vec<ObjectRef<CloudflareDNSRecord>> {
    records
        .iter()
        .filter(|record| match &record.spec.content {
            StringOrService::Service(selector) => {
                // A selector without a namespace selects a service in the namespace of the record.
                let ns = selector.namespace.as_ref().or(record.metadata.namespace.as_ref());
                Some(&selector.name) == svc.metadata.name.as_ref() && ns == svc.metadata.namespace.as_ref()
            }
            StringOrService::Value(_) => false,
        })
        .map(|record| ObjectRef::from_obj(record.as_ref()))
        .collect()
}

/// Extracts the public IP address from a [`Service`] resource.
pub async fn public_ip_from_service(
    client: &kube::Client,
    name: &str,
    ns: &str,
    record_type: Option<RecordType>,
) -> Result<Option<IpAddr>> {
    let svc = kube::api::Api::<Service>::namespaced(client.clone(), ns)
        .get(name)
        .await?;

    let Some(spec) = svc.spec.as_ref() else {
        warn!("Service {ns}/{name} has no spec");
        return Ok(None);
    };

    if spec.type_.as_deref() == Some("LoadBalancer") {
        let Some(ips) = svc.status.as_ref().and_then(|s| {
            s.load_balancer
                .as_ref()
                .and_then(|lb| lb.ingress.as_ref())
                .map(|ingress| {
                    ingress
                        .iter()
                        .filter_map(|i| {
                            let ip = i.ip.as_deref()?;
                            ip.parse::<IpAddr>().ok()
                        })
                        .collect::<Vec<_>>()
                })
        }) else {
            return Err(eyre::eyre!("no load balancer ip found"));
        };

        return Ok(select_ip(ips, record_type, name, ns));
    }

    if let Some(ips) = spec.external_ips.as_ref().map(|ips| {
        ips.iter()
            .filter_map(|ip| ip.parse::<IpAddr>().ok())
            .collect::<Vec<_>>()
    }) {
        return Ok(select_ip(ips, record_type, name, ns));
    };

    warn!("Service {ns}/{name} is not a LoadBalancer and has no external IPs");
    Ok(None)
}

/// Selects the most suitable IP address from a list of IPs, given a [hint](RecordType) about the expected record type.
fn select_ip(ips: Vec<IpAddr>, record_type: Option<RecordType>, name: &str, ns: &str) -> Option<IpAddr> {
    match (&ips[..], record_type) {
        ([], None) => {
            warn!("Service {ns}/{name} has no lb/external ip");
            None
        }

        // Single ipv4
        ([ip @ IpAddr::V4(_)], Some(RecordType::A)) => Some(*ip),
        ([ip @ IpAddr::V4(_)], Some(RecordType::AAAA)) => {
            warn!("Expected ipv6 address, but found ipv4 address");
            Some(*ip)
        }

        // Single ipv6
        ([ip @ IpAddr::V6(_)], Some(RecordType::A)) => {
            warn!("Expected ipv4 address, but found ipv4 address");
            Some(*ip)
        }
        ([ip @ IpAddr::V6(_)], Some(RecordType::AAAA)) => Some(*ip),

        // Single ip, no hint
        ([ip], _) => Some(*ip),

        // multiple ips with hint
        (ips, Some(RecordType::A)) => Some(ips.iter().find(|ip| ip.is_ipv4()).copied().unwrap_or(ips[0])),
        (ips, Some(RecordType::AAAA)) => Some(ips.iter().find(|ip| ip.is_ipv6()).copied().unwrap_or(ips[0])),

        // No uselful hint
        (ips, _) => {
            warn!("Service {ns}/{name} has multiple load balancer ips, using the first ipv4 one or the first one if none are ipv4");
            Some(ips.iter().find(|ip| ip.is_ipv4()).copied().unwrap_or(ips[0]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::api::ObjectMeta;
    use serde_json::json;

    fn record(ns: &str, name: &str, content: serde_json::Value) -> Arc<CloudflareDNSRecord> {
        let spec = json!({ "name": "example.com", "content": content, "zone": { "name": { "value": "example.com" } } });
        let mut record = CloudflareDNSRecord::new(name, serde_json::from_value(spec).unwrap());
        record.metadata.namespace = Some(ns.to_string());
        Arc::new(record)
    }

    #[test]
    fn service_change_reconciles_the_records_that_use_the_service() {
        // regression: the watch reconciled the record named like the service, so
        // records that use traefik/traefik never reacted to it (2026-11-20)
        let records = [
            record(
                "app",
                "web",
                json!({ "service": { "name": "traefik", "namespace": "traefik" } }),
            ),
            record("traefik", "dashboard", json!({ "service": { "name": "traefik" } })),
            // named like the service, but uses the service app/traefik
            record("app", "traefik", json!({ "service": { "name": "traefik" } })),
            record("traefik", "api", json!({ "service": { "name": "api" } })),
            record("traefik", "static", json!({ "value": "1.2.3.4" })),
        ];
        let traefik = Service {
            metadata: ObjectMeta {
                name: Some("traefik".to_string()),
                namespace: Some("traefik".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            records_using_service(&traefik, &records),
            [
                ObjectRef::new("web").within("app"),
                ObjectRef::new("dashboard").within("traefik")
            ]
        );
    }
}
