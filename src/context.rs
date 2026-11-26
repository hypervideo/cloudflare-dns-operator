use crate::{
    dns::cloudflare::CloudflareApi,
    dns_check::DnsCheckSender,
    resources::CloudflareDNSRecord,
};
use kube::runtime::reflector::Store;
use std::{
    collections::HashMap,
    sync::OnceLock,
};
use tokio::sync::Mutex;

/// Holds state shared by the controller and other processes such as the DNS watcher.
pub struct Context {
    pub client: kube::Client,
    pub cloudflare_api: CloudflareApi,
    pub do_dns_check: bool,
    pub dns_check_tx: DnsCheckSender,
    /// Maps CloudflareDNSRecord `{ns}:{name}` keys to DNS lookup results.
    pub dns_lookup_success: Mutex<HashMap<String, bool>>,
    /// Controller reflector store used as a fast pre-check for sibling records in `apply`.
    pub records_store: OnceLock<Store<CloudflareDNSRecord>>,
}
