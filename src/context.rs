use crate::{
    dns::cloudflare::CloudflareApi,
    dns_check::DnsCheckSender,
};
use std::collections::HashMap;
use tokio::sync::Mutex;

/// Holds state shared by the controller and other processes such as the DNS watcher.
pub struct Context {
    pub client: kube::Client,
    pub cloudflare_api: CloudflareApi,
    pub do_dns_check: bool,
    pub dns_check_tx: DnsCheckSender,
    /// Maps CloudflareDNSRecord `{ns}:{name}` keys to DNS lookup results.
    pub dns_lookup_success: Mutex<HashMap<String, bool>>,
    /// Serializes `apply` and `cleanup` in this process so reference counts and status writes do not race.
    // lessismore: one global lock + a full cluster list per apply is simple and fine at current scale (~300 records);
    // revisit with per-name locking if reconcile queue wait times grow or record counts reach the thousands.
    pub reconcile_lock: Mutex<()>,
}
