use crate::dns_check::DnsCheckSender;
use std::collections::HashMap;
use tokio::sync::Mutex;

pub struct ControllerState {
    pub client: kube::Client,
    pub cloudflare_api_token: String,
    pub do_dns_check: bool,
    pub dns_check_tx: DnsCheckSender,
    pub dns_lookup_success: Mutex<HashMap<String, bool>>,
}
