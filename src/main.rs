#[macro_use]
extern crate tracing;

use clap::Parser;
use cloudflare_dns_operator::{
    context,
    dns::cloudflare::CloudflareApi,
    dns_check,
    reconcile::{
        self,
        ReconcileError,
    },
    resources,
    services,
};
use context::Context;
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
use services::is_suitable_service;
use std::{
    net::SocketAddr,
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

    #[clap(
        long,
        env = "NAMESERVER_FOR_DNS_CHECK",
        help = "Nameserver and port to use for DNS checks",
        default_value = "1.1.1.1:53"
    )]
    nameserver: SocketAddr,
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
            let cloudflare_api = CloudflareApi::new(args.cloudflare_api_token);
            let zones = cloudflare_api.list_zones().await?;
            dbg!(zones);
        }
    }

    Ok(())
}

async fn run_controller(
    ArgsController {
        cloudflare_api_token,
        dns_checks,
        nameserver,
    }: ArgsController,
) -> Result<(), ReconcileError> {
    let client = kube::Client::try_default().await?;

    let dns_resources = Api::<resources::CloudflareDNSRecord>::all(client.clone());

    let (dns_check_tx, dns_check_rx) = mpsc::channel(64);

    let cloudflare_api = CloudflareApi::new(cloudflare_api_token);

    let context = Arc::new(Context {
        client: client.clone(),
        cloudflare_api,
        do_dns_check: dns_checks.is_some(),
        dns_check_tx,
        dns_lookup_success: Default::default(),
    });

    let dns_change = dns_check::start_dns_check(context.clone(), dns_check_rx, dns_checks, nameserver);

    info!("Starting controller");

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
    ctx: Arc<Context>,
) -> Result<Action, finalizer::Error<ReconcileError>> {
    let ns = resource.meta().namespace.as_deref().unwrap_or("default");
    let api: Api<resources::CloudflareDNSRecord> = Api::namespaced(ctx.client.clone(), ns);

    finalizer(&api, "dns.cloudflare.com/delete-dns-record", resource, |event| async {
        let result = match event {
            Event::Apply(server) => reconcile::apply(server, ctx.clone()).await,
            Event::Cleanup(server) => reconcile::cleanup(server, ctx.clone()).await,
        };

        if let Err(err) = result {
            match err {
                reconcile::ReconcileError::Kube(kube::Error::Api(err)) if err.code == 409 => {
                    warn!("Conflict when reconciling object: {err}");
                }
                reconcile::ReconcileError::Kube(kube::Error::Api(err)) if err.code == 404 => {
                    warn!("Object not found when reconciling object: {err}");
                }
                reconcile::ReconcileError::Deletion(err) => {
                    error!("Failed to delete object: {err:?}");
                }
                err => {
                    return Err(err);
                }
            }
        }

        Ok(Action::requeue(Duration::from_secs(5 * 60)))
    })
    .await
}

fn error_policy(
    _object: Arc<resources::CloudflareDNSRecord>,
    err: &finalizer::Error<ReconcileError>,
    _ctx: Arc<Context>,
) -> Action {
    error!("Error reconciling: {:?}", err);
    Action::requeue(Duration::from_secs(15))
}
