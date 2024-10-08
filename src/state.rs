use std::{
    collections::HashMap,
    sync::Arc,
};
use tokio::sync::Mutex;

pub struct ControllerState {
    pub client: kube::Client,
    pub latest_dns_specs: Arc<Mutex<HashMap<String, LatestDnsSpec>>>,
}

// TODO: We use this to cache the zone_id, api_token fetched from the server spec (e.g. by resolving a secret ref).
// Since the secret/configmap these values come from aren't owned by the server resource, they could be deleted before
// the server finalizer runs. Since we need the secrets for that, we "cache" them here. This fails if the controller is
// restarted and does not have a chance to lookup the values before deletion.
#[derive(Debug, Clone)]
pub enum LatestDnsSpec {
    Cloudflare { zone_id: String, api_token: String },
}
