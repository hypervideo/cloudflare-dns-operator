#[macro_use]
extern crate tracing;

mod dns;
mod reconcile;
mod resources;
mod services;
mod state;

use clap::Parser;
use eyre::Result;
use futures::StreamExt as _;
use k8s_openapi::api::core::v1::{
    Secret,
    Service,
};
use kube::{
    runtime::{
        controller::Action,
        finalizer,
        finalizer::Event,
        watcher,
        Controller,
    },
    Api,
    Client,
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

#[derive(Parser)]
enum Args {
    Crds,
    Controller(ArgsController),
}

#[derive(Parser)]
struct ArgsController {}

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

async fn run_controller(ArgsController {}: ArgsController) -> Result<()> {
    // Load the kubeconfig file.
    let config = kube::Config::from_kubeconfig(&kube::config::KubeConfigOptions::default()).await?;
    let client = Client::try_from(config)?;
    let owned = Api::<resources::CloudflareDNSRecord>::all(client.clone());
    let secrets = Api::<Secret>::all(client.clone());

    info!("Starting controller");

    Controller::new(owned, Default::default())
        // watch load balancers to adjust dns <-> public ip
        .watches(
            Api::<Service>::all(client.clone()),
            watcher::Config::default(),
            is_suitable_service,
        )
        .owns(secrets, Default::default())
        .shutdown_on_signal()
        .run(
            reconcile,
            error_policy,
            Arc::new(ControllerState {
                client,
                latest_dns_specs: Default::default(),
            }),
        )
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
