use crate::resources::{
    CloudflareDNSRecord,
    RecordType,
};
use eyre::Result;
use k8s_openapi::api::core::v1::Service;
use kube::runtime::reflector::ObjectRef;
use std::net::IpAddr;

pub fn is_suitable_service(svc: Service) -> Option<ObjectRef<CloudflareDNSRecord>> {
    let spec = svc.spec.as_ref()?;
    if spec.type_.as_deref() == Some("LoadBalancer") || spec.external_ips.as_ref().map_or(false, |ips| !ips.is_empty())
    {
        let name = svc.metadata.name.as_deref()?;
        let ns = svc.metadata.namespace.as_deref()?;
        Some(ObjectRef::new(name).within(ns))
    } else {
        None
    }
}

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
