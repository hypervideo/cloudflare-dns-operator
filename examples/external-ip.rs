#[macro_use]
extern crate tracing;

use eyre::Result;
use k8s_openapi::api::core::v1::Service;
use kube::ResourceExt as _;
use std::net::IpAddr;

#[tokio::main]
async fn main() {
    color_eyre::install().expect("color_eyre init");
    tracing_subscriber::fmt::init();
    run().await.expect("run");
}

async fn run() -> Result<()> {
    let client = kube::Client::try_default().await?;
    let api = kube::Api::<Service>::all(client.clone());

    let services = api.list(&kube::api::ListParams::default()).await?;

    for svc in services {
        let Some(spec) = svc.spec.as_ref() else {
            warn!("Service has no spec");
            continue;
        };

        let name = svc.name_any();
        let ns = svc.metadata.namespace.as_deref().unwrap_or("default");

        if let Some(ips) = spec.external_ips.as_ref().map(|ips| {
            ips.iter()
                .filter_map(|ip| ip.parse::<IpAddr>().ok())
                .collect::<Vec<_>>()
        }) {
            println!("Service {ns}/{name} has external IPs: {:?}", ips);
        };
    }

    Ok(())
}
