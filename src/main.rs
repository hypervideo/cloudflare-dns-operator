#[macro_use]
extern crate tracing;

mod dns;
mod dns_check;
mod reconcile;
mod resources;
mod services;
mod state;

use clap::Parser;
use dns_check::start_dns_check;
use eyre::Result;
use futures::StreamExt as _;
use k8s_openapi::api::core::v1::Service;
use kube::{
    runtime::{
        controller::Action,
        finalizer,
        finalizer::Event,
        watcher,
        Controller,
    },
    Api,
    CustomResourceExt as _,
    Resource as _,
};
use reconcile::{
    cleanup,
    update,
};
use services::is_suitable_service;
use state::ControllerState;
use std::{
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(version, about)]
enum Args {
    Crds,
    Controller(ArgsController),
    ListZones(ArgsController),
}

#[derive(Parser)]
struct ArgsController {
    #[clap(long, env = "CLOUDFLARE_API_TOKEN", help = "Cloudflare API token")]
    cloudflare_api_token: String,

    #[clap(
        long = "dns-check",
        env = "CHECK_DNS_RESOLUTION",
        help = "Do active DNS checks by querying 1.1.1.1? If not set, DNS check is disabled",
        value_parser = humantime::parse_duration
    )]
    dns_checks: Option<Duration>,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install().expect("color_eyre init");
    tracing_subscriber::fmt::init();

    match Args::parse() {
        Args::Crds => {
            let yaml = serde_yaml::to_string(&resources::CloudflareDNSRecord::crd()).expect("Failed to serialize CRD");
            print!("{yaml}")
        }
        Args::Controller(args) => {
            run_controller(args).await?;
        }
        Args::ListZones(args) => {
            let zones = dns::cloudflare::list_zones(&args.cloudflare_api_token).await?;
            dbg!(zones);
        }
    }

    Ok(())
}

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("Failed to create CRD: {0}")]
    Crd(#[from] kube::Error),
    #[error("Unexpected error: {0}")]
    Unexpected(#[from] eyre::Error),
}

async fn run_controller(
    ArgsController {
        cloudflare_api_token,
        dns_checks,
    }: ArgsController,
) -> Result<()> {
    // Load the kubeconfig file.
    let client = kube::Client::try_default().await?;

    let dns_resources = Api::<resources::CloudflareDNSRecord>::all(client.clone());

    info!("Starting controller");

    let (dns_check_tx, dns_check_rx) = mpsc::channel(64);

    let context = Arc::new(ControllerState {
        client: client.clone(),
        cloudflare_api_token,
        do_dns_check: dns_checks.is_some(),
        dns_check_tx,
        dns_lookup_success: Default::default(),
    });

    let dns_change = start_dns_check(context.clone(), dns_check_rx, dns_checks);

    Controller::new(dns_resources, Default::default())
        // watch load balancers / external ip services to adjust dns <-> public ip
        .watches(
            Api::<Service>::all(client),
            watcher::Config::default(),
            is_suitable_service,
        )
        .reconcile_on(dns_change)
        .shutdown_on_signal()
        .run(reconcile, error_policy, context)
        .for_each(|msg| async move { info!("Reconciled: {:?}", msg) })
        .await;

    info!("Controller stopped");

    Ok(())
}

async fn reconcile(
    resource: Arc<resources::CloudflareDNSRecord>,
    ctx: Arc<ControllerState>,
) -> Result<Action, finalizer::Error<Error>> {
    let ns = resource.meta().namespace.as_deref().unwrap_or("default");
    let api: Api<resources::CloudflareDNSRecord> = Api::namespaced(ctx.client.clone(), ns);

    finalizer(&api, "dns.cloudflare.com/delete-dns-record", resource, |event| async {
        match event {
            Event::Apply(server) => update(server, ctx.clone())
                .await
                .expect("Failed to update hyper deployment"),
            Event::Cleanup(server) => cleanup(server, ctx.clone())
                .await
                .expect("Failed to delete hyper deployment"),
        }

        Ok(Action::requeue(Duration::from_secs(60)))
    })
    .await
}

fn error_policy(
    _object: Arc<resources::CloudflareDNSRecord>,
    err: &finalizer::Error<Error>,
    _ctx: Arc<ControllerState>,
) -> Action {
    error!("Error reconciling: {:?}", err);
    Action::requeue(Duration::from_secs(15))
}
