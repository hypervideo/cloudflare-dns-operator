use crate::dns_check::DnsCheckSender;
use std::collections::HashMap;
use tokio::sync::Mutex;

/// Holds state shared by the controller and other processes such as the DNS watcher.
pub struct Context {
    pub client: kube::Client,
    pub cloudflare_api_token: String,
    pub do_dns_check: bool,
    pub dns_check_tx: DnsCheckSender,
    /// Maps CloudflareDNSRecord `{ns}:{name}` keys to DNS lookup results.
    pub dns_lookup_success: Mutex<HashMap<String, bool>>,
}
