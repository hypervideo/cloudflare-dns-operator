use super::util;
use crate::resources::{
    normalize_dns_name,
    RecordType,
};
use chrono::{
    prelude::*,
    Duration,
};
use eyre::{
    bail,
    Context as _,
    Result,
};
use reqwest::Method;
use serde::{
    de::DeserializeOwned,
    Deserialize,
    Serialize,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::Arc,
};
use tokio::sync::Mutex;

// curl 'https://api.cloudflare.com/client/v4/accounts/{account_id}/pages/projects/{project_name}/deployments' --header 'Authorization: Bearer <API_TOKEN>'
// c8bba8ee5e5c7b5f8b20bc4d5ca0de58

/// How long one cloudflare request may take, from connecting until the response body is read. The caches in
/// [`CloudflareApi`] stay locked during their request, so a request that never finishes would block every reconcile.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

const DEFAULT_BASE_URL: &str = "https://api.cloudflare.com/client/v4";

/// Non-success HTTP response returned by the Cloudflare API.
#[derive(thiserror::Error, Debug)]
#[error("cloudflare api error: status={status:?}, body={body:?}")]
pub struct CloudflareApiError {
    pub status: reqwest::StatusCode,
    pub body: String,
}

pub fn is_cloudflare_rate_limit(err: &eyre::Report) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<CloudflareApiError>()
            .is_some_and(|api_err| api_err.status == reqwest::StatusCode::TOO_MANY_REQUESTS)
    })
}

fn is_cloudflare_not_found(err: &eyre::Report) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<CloudflareApiError>()
            .is_some_and(|api_err| api_err.status == reqwest::StatusCode::NOT_FOUND)
    })
}

/// Wraps the cloudflare api response.
#[derive(Debug, Serialize, Deserialize)]
struct ApiResult<T> {
    errors: Value,
    messages: Value,
    result: T,
    result_info: Option<ApiResultInfo>,
    success: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct ApiResultInfo {
    count: usize,
    page: usize,
    per_page: usize,
    total_count: usize,
    total_pages: usize,
}

// -=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-
// account

/// A cloudflare account that represents a zone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountInfo {
    account: Account,
    id: String,
    name: String,
    activated_on: DateTime<Utc>,
    created_on: DateTime<Utc>,
    modified_on: Option<DateTime<Utc>>,
    development_mode: i64,
    meta: Value,
    name_servers: Vec<String>,
    original_dnshost: Option<Value>,
    original_name_servers: Option<Value>,
    original_registrar: Option<Value>,
    owner: Owner,
    paused: bool,
    permissions: Vec<String>,
    plan: Plan,
    status: String,
    tenant: Value,
    tenant_unit: Value,
    r#type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    id: String,
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Owner {
    email: Option<String>,
    id: Option<String>,
    r#type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    can_subscribe: bool,
    currency: String,
    externally_managed: bool,
    frequency: String,
    id: String,
    is_subscribed: bool,
    legacy_discount: bool,
    legacy_id: String,
    name: String,
    price: i64,
}

// -=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-

/// A cloudflare dns record.
///
/// See https://developers.cloudflare.com/api/operations/zones-get?schema_url=https%3A%2F%2Fraw.githubusercontent.com%2Fcloudflare%2Fapi-schemas%2Fmain%2Fopenapi.yaml
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsRecordInfo {
    pub comment: Option<String>,
    pub content: String,
    pub created_on: DateTime<Utc>,
    pub id: String,
    pub meta: DnsRecordMeta,
    pub modified_on: DateTime<Utc>,
    pub name: String,
    pub proxiable: bool,
    pub proxied: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    pub ttl: i64,
    #[serde(rename = "type")]
    pub record_type: String,
}

impl std::fmt::Display for DnsRecordInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{name} {record_type} {content} ttl={ttl}",
            name = self.name,
            record_type = self.record_type,
            content = self.content,
            ttl = self.ttl
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsRecordMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_added: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_by_apps: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_by_argo_tunnel: Option<bool>,
}

/// Request payload for creating a new dns record.
///
/// See https://developers.cloudflare.com/api/operations/dns-records-for-a-zone-create-dns-record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsRecordModification {
    /// <= 32 characters
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub record_type: RecordType,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxied: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

/// A cloudflare zone. Either the zone name (such as "example.com") or the cloudflare id of it.
#[derive(Clone, Debug)]
pub enum Zone {
    Identifier(String),
    Name(String),
}

impl Zone {
    pub fn id(id: impl ToString) -> Self {
        Zone::Identifier(id.to_string())
    }

    pub fn name(name: impl ToString) -> Self {
        Zone::Name(name.to_string())
    }

    pub async fn resolve(self, api: &CloudflareApi) -> Result<Option<Self>> {
        self.lookup_id(api).await.map(|id| id.map(Zone::Identifier))
    }

    pub async fn lookup_id(self, api: &CloudflareApi) -> Result<Option<String>> {
        match self {
            Zone::Identifier(id) => Ok(Some(id)),
            Zone::Name(name) => {
                debug!(?name, "looking up zone by name");
                let accounts = api.list_zones().await?;
                Ok(accounts.into_iter().find(|it| it.name == name).map(|it| it.id))
            }
        }
    }
}

/// Result of [`CloudflareApi::update_dns_record_and_wait`].
#[derive(Debug, Clone)]
pub struct UpdatedDnsRecord {
    pub record: DnsRecordInfo,
    /// Cloudflare record IDs that were deleted while replacing mismatched records of the same name and type.
    pub deleted_ids: Vec<String>,
}

/// Arguments for [`create_dns_record`].
#[derive(Debug)]
pub struct CreateRecordArgs {
    pub zone: Zone,
    pub name: String,
    pub record_type: RecordType,
    pub content: String,
    pub comment: Option<String>,
    pub ttl: Option<i64>,
}

#[allow(clippy::type_complexity)]
#[derive(Clone, Debug)]
pub struct CloudflareApi {
    api_token: String,
    base_url: String,
    list_zone_cache: Arc<Mutex<Option<(DateTime<Utc>, Vec<AccountInfo>)>>>,
    list_dns_records_cache: Arc<Mutex<HashMap<String, (DateTime<Utc>, Vec<DnsRecordInfo>)>>>,
}

impl CloudflareApi {
    pub fn new(api_token: String) -> Self {
        Self {
            api_token,
            base_url: DEFAULT_BASE_URL.to_string(),
            list_zone_cache: Default::default(),
            list_dns_records_cache: Default::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_base_url(api_token: String, base_url: impl Into<String>) -> Self {
        Self {
            api_token,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            list_zone_cache: Default::default(),
            list_dns_records_cache: Default::default(),
        }
    }

    async fn invalidate_dns_record_cache(&self, zone_identifier: impl AsRef<str>) {
        self.list_dns_records_cache
            .lock()
            .await
            .remove(zone_identifier.as_ref());
    }

    /// List all cloudflare accounts which represent zones.
    pub async fn list_zones(&self) -> Result<Vec<AccountInfo>, eyre::Error> {
        const CACHE_DURATION: Duration = Duration::minutes(5);

        let mut cache = self.list_zone_cache.lock().await;
        if let Some((time, zones)) = cache.as_ref() {
            if Utc::now() - *time < CACHE_DURATION {
                return Ok(zones.clone());
            }
        }

        let url = format!("{}/zones", self.base_url);
        let zones = cloudflare_api_request::<Vec<AccountInfo>, ()>(&url, None, Method::GET, &self.api_token).await?;
        *cache = Some((Utc::now(), zones.clone()));

        Ok(zones)
    }

    /// List DNS records in a cloudflare zone.
    pub async fn list_dns_records(&self, zone_identifier: impl AsRef<str>) -> Result<Vec<DnsRecordInfo>> {
        const CACHE_DURATION: Duration = Duration::minutes(1);

        let zone_identifier = zone_identifier.as_ref();
        // Keep the cache locked during the request. Concurrent reconciles then wait for this listing and read it from
        // the cache instead of each listing the zone, which would exceed cloudflare's rate limit.
        let mut cache = self.list_dns_records_cache.lock().await;

        if let Some((time, records)) = cache.get(zone_identifier) {
            if Utc::now() - *time < CACHE_DURATION {
                return Ok(records.clone());
            }
        }

        let url = format!("{}/zones/{zone_identifier}/dns_records", self.base_url);
        let records = cloudflare_api_get_all::<DnsRecordInfo, ()>(&url, None, Method::GET, &self.api_token).await?;
        cache.insert(zone_identifier.to_string(), (Utc::now(), records.clone()));

        debug!(records = %records.len(), "listed dns records");

        Ok(records)
    }

    /// Create a new cloudflare dns record
    pub async fn create_dns_record(&self, args: CreateRecordArgs) -> Result<DnsRecordInfo, eyre::Error> {
        let CreateRecordArgs {
            zone,
            name,
            record_type,
            content,
            comment,
            ttl,
        } = args;

        let zone_identifier = zone
            .lookup_id(self)
            .await?
            .ok_or_else(|| eyre::eyre!("zone not found"))?;
        let url = format!("{}/zones/{zone_identifier}/dns_records", self.base_url);
        let id = util::id();

        info!(?id, ?name, r#type = ?record_type, "creating dns record");
        let result = cloudflare_api_request::<DnsRecordInfo, _>(
            &url,
            Some(DnsRecordModification {
                id,
                name,
                record_type,
                content,
                ttl,
                proxied: None,
                comment,
                tags: None,
            }),
            Method::POST,
            &self.api_token,
        )
        .await;

        self.invalidate_dns_record_cache(zone_identifier).await;

        result
    }

    /// Updates a cloudflare dns record... currently deletes and recreates... Will wait for the dns record to propagate,
    /// i.e. a dns lookup resolves to the correct ip.
    // TODO: we should use the proper patch api.
    #[instrument(level = "debug", skip(self))]
    pub async fn update_dns_record_and_wait(&self, args: CreateRecordArgs) -> Result<UpdatedDnsRecord, eyre::Error> {
        let Some(zone_id) = args.zone.clone().lookup_id(self).await? else {
            bail!("zone not found");
        };

        debug!("updating dns record");

        let domain = normalize_dns_name(&args.name);
        let record_type = args.record_type;
        let dns_records = self.list_dns_records(&zone_id).await?;

        let mut existing = None;
        let mut deleted_ids = Vec::new();
        for record in dns_records.into_iter().filter(|record| {
            normalize_dns_name(&record.name) == domain && record.record_type.eq_ignore_ascii_case(record_type.as_str())
        }) {
            if record.content == args.content {
                existing = Some(record);
                info!(
                    "DNS record for {domain:?} ({record_type:?}) already exists with {:?}",
                    args.content
                );
                continue;
            }

            warn!(
                "Found existing DNS record for web domain {domain:?} ({record_type:?}) with ip {:?}. Deleting.",
                record.content
            );
            self.delete_dns_record(&zone_id, &record.id)
                .await
                .context("Failed to delete existing DNS record")?;
            deleted_ids.push(record.id);
        }

        if let Some(existing) = existing {
            return Ok(UpdatedDnsRecord {
                record: existing,
                deleted_ids,
            });
        }

        info!(
            "Creating new DNS record for {domain:?} ({record_type:?}) with {:?}",
            args.content
        );
        let record = self.create_dns_record(args).await?;
        debug!("Registered record for {domain:?} with {:?}", record.content);

        self.invalidate_dns_record_cache(zone_id).await;

        Ok(UpdatedDnsRecord { record, deleted_ids })
    }

    /// Delete a DNS record by its (domain) name using the cloudflare API
    #[allow(dead_code)]
    pub async fn delete_dns_records_by_name(
        &self,
        name: impl AsRef<str>,
        zone_identifier: impl AsRef<str>,
    ) -> Result<(), eyre::Error> {
        let name = normalize_dns_name(name.as_ref());
        let zone_identifier = zone_identifier.as_ref();

        info!(?name, "deleting dns record by name");
        let records = self
            .list_dns_records(&zone_identifier)
            .await?
            .into_iter()
            .filter(|it| normalize_dns_name(&it.name) == name);

        for record in records {
            self.delete_dns_record(zone_identifier, record.id).await?;
        }

        Ok(())
    }

    /// Delete a DNS record by its id using the cloudflare API.
    /// Treats 404 Not Found as already deleted so retries are idempotent.
    pub async fn delete_dns_record(&self, zone_identifier: impl AsRef<str>, id: impl AsRef<str>) -> Result<()> {
        let zone_identifier = zone_identifier.as_ref();
        let id = id.as_ref();
        let url = format!("{}/zones/{zone_identifier}/dns_records/{id}", self.base_url);

        match cloudflare_api_request::<Value, ()>(&url, None, Method::DELETE, &self.api_token).await {
            Ok(_) => {}
            Err(err) if is_cloudflare_not_found(&err) => {
                warn!(
                    zone_identifier,
                    id, "Cloudflare DNS record already gone (404), treating delete as done"
                );
            }
            Err(err) => return Err(err),
        }

        self.invalidate_dns_record_cache(zone_identifier).await;

        Ok(())
    }
}

pub async fn cloudflare_api_get_all<T, B>(
    url: &str,
    body: Option<B>,
    method: Method,
    api_token: impl AsRef<str>,
) -> Result<Vec<T>>
where
    B: Serialize + Clone,
    T: DeserializeOwned,
{
    let api_token = api_token.as_ref();
    let mut results = Vec::new();
    let mut current_page = 1usize;

    loop {
        let query = format!("page={current_page}");
        let url = if url.contains('?') {
            format!("{url}&{query}")
        } else {
            format!("{url}?{query}")
        };

        let res = cloudflare_api_request_inner::<Vec<T>, _>(&url, body.clone(), method.clone(), api_token).await?;

        let page_len = res.result.len();
        results.extend(res.result);

        if !has_more_pages(res.result_info.as_ref(), page_len, current_page) {
            break;
        }

        current_page += 1;
    }

    Ok(results)
}

/// Whether to fetch the page after `current_page`. Records can be deleted
/// while we paginate, shrinking `total_pages` below `current_page`; cloudflare
/// answers out-of-range pages with an empty success, so a `==` comparison
/// against `total_pages` never terminates. Only continue while the current
/// page was non-empty and more pages are reported ahead.
fn has_more_pages(info: Option<&ApiResultInfo>, page_len: usize, current_page: usize) -> bool {
    let Some(info) = info else {
        return false;
    };
    page_len > 0 && current_page < info.total_pages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(total_pages: usize) -> ApiResultInfo {
        ApiResultInfo {
            count: 0,
            page: 0,
            per_page: 100,
            total_count: 0,
            total_pages,
        }
    }

    #[test]
    fn pagination_stops_on_last_page() {
        assert!(has_more_pages(Some(&info(2)), 100, 1));
        assert!(!has_more_pages(Some(&info(2)), 88, 2));
        assert!(!has_more_pages(Some(&info(1)), 100, 1));
    }

    #[test]
    fn pagination_stops_when_total_pages_shrinks_below_current_page() {
        // regression: records deleted mid-listing shrank total_pages below
        // current_page and the operator paginated forever (2026-08-14)
        assert!(!has_more_pages(Some(&info(1)), 0, 3));
        assert!(!has_more_pages(Some(&info(1)), 0, 213_898));
    }

    #[test]
    fn pagination_stops_without_result_info_or_results() {
        assert!(!has_more_pages(None, 100, 1));
        assert!(!has_more_pages(Some(&info(5)), 0, 2));
    }

    #[tokio::test(start_paused = true)]
    async fn request_to_unresponsive_server_times_out() {
        // regression: a request that never got an answer held the dns record
        // cache lock forever and stopped all reconciles (2026-11-20)
        // The kernel accepts the connection, but nobody ever answers.
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", server.local_addr().unwrap());

        let request = cloudflare_api_request::<Value, ()>(&url, None, Method::GET, "token");
        let result = tokio::time::timeout(std::time::Duration::from_secs(600), request)
            .await
            .expect("the request never gave up");

        let err = result.unwrap_err();
        assert!(err.downcast_ref::<reqwest::Error>().unwrap().is_timeout(), "{err:?}");
    }

    #[tokio::test]
    async fn update_dns_record_matches_record_type_and_normalized_name() {
        // regression: update_dns_record_and_wait filtered only by exact `record.name == domain`, so reconciling an A
        // record deleted TXT/AAAA records with the same name and missed uppercase or trailing-dot names
        use std::{
            io::{
                Read as _,
                Write as _,
            },
            sync::{
                atomic::{
                    AtomicBool,
                    Ordering,
                },
                Mutex as StdMutex,
            },
        };

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();

        let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let reqs = requests.clone();
        let stop_flag = stop.clone();

        let list_body = serde_json::json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [
                {
                    "id": "rec-txt",
                    "name": "example.com",
                    "type": "TXT",
                    "content": "v=spf1 -all",
                    "ttl": 300,
                    "proxiable": false,
                    "proxied": false,
                    "created_on": "2026-01-01T00:00:00Z",
                    "modified_on": "2026-01-01T00:00:00Z",
                    "meta": {},
                    "tags": []
                },
                {
                    "id": "rec-a",
                    "name": "EXAMPLE.COM",
                    "type": "A",
                    "content": "1.2.3.4",
                    "ttl": 300,
                    "proxiable": true,
                    "proxied": false,
                    "created_on": "2026-01-01T00:00:00Z",
                    "modified_on": "2026-01-01T00:00:00Z",
                    "meta": {},
                    "tags": []
                }
            ]
        })
        .to_string();

        let created_body = serde_json::json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": {
                "id": "rec-new",
                "name": "example.com",
                "type": "A",
                "content": "1.2.3.4",
                "ttl": 300,
                "proxiable": true,
                "proxied": false,
                "created_on": "2026-01-01T00:00:00Z",
                "modified_on": "2026-01-01T00:00:00Z",
                "meta": {},
                "tags": []
            }
        })
        .to_string();

        let handle = std::thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                            .unwrap();
                        let mut buf = [0u8; 4096];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let req_text = String::from_utf8_lossy(&buf[..n]);
                        let first_line = req_text.lines().next().unwrap_or("");
                        let mut parts = first_line.split_whitespace();
                        let method = parts.next().unwrap_or("");
                        let path = parts.next().unwrap_or("");
                        reqs.lock().unwrap().push(format!("{method} {path}"));
                        let body = match method {
                            "GET" => list_body.as_str(),
                            "POST" => created_body.as_str(),
                            _ => r#"{"success":true,"errors":[],"messages":[],"result":{"id":"deleted"}}"#,
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(resp.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let api = CloudflareApi::with_base_url("token".to_string(), base_url);
        let result = api
            .update_dns_record_and_wait(CreateRecordArgs {
                zone: Zone::id("zone-1"),
                name: "example.com.".to_string(),
                record_type: RecordType::A,
                content: "1.2.3.4".to_string(),
                comment: None,
                ttl: None,
            })
            .await;

        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();

        let seen = requests.lock().unwrap().clone();
        let updated = result.expect("update_dns_record_and_wait failed");
        assert_eq!(
            updated.record.id, "rec-a",
            "should reuse existing A record; requests: {seen:?}"
        );
        assert!(updated.deleted_ids.is_empty());
        assert!(
            !seen.iter().any(|r| r.starts_with("DELETE ")),
            "must not delete TXT record when reconciling A record: {seen:?}"
        );
    }
}

pub async fn cloudflare_api_request<R, B>(
    url: &str,
    body: Option<B>,
    method: Method,
    api_token: impl AsRef<str>,
) -> Result<R>
where
    B: Serialize,
    R: DeserializeOwned,
{
    let body = cloudflare_api_request_inner(url, body, method, api_token).await?;
    Ok(body.result)
}

async fn cloudflare_api_request_inner<R, B>(
    url: &str,
    body: Option<B>,
    method: Method,
    api_token: impl AsRef<str>,
) -> Result<ApiResult<R>>
where
    B: Serialize,
    R: DeserializeOwned,
{
    debug!("[{method}] {url} cloudflare API request");

    let req = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()?
        .request(method, url)
        .bearer_auth(api_token.as_ref())
        .header("Content-Type", "application/json");

    let req = if let Some(body) = body { req.json(&body) } else { req };

    let res = req.send().await?;

    if !res.status().is_success() {
        let status = res.status();
        let body = res
            .text()
            .await
            .unwrap_or_else(|err| format!("<failed to read body: {err}>"));
        return Err(CloudflareApiError { status, body }.into());
    }

    #[cfg(debug_assertions)]
    let body: ApiResult<R> = {
        let body: Value = res.json().await?;
        match serde_json::from_value(body.clone()) {
            Err(err) => bail!(
                "failed to parse api response: {err:?}: {}",
                serde_json::to_string_pretty(&body).expect("pretty json")
            ),
            Ok(it) => it,
        }
    };

    #[cfg(not(debug_assertions))]
    let body: ApiResult<_> = res.json().await?;

    if !body.success {
        bail!("cloudflare api error: success=false, errors={}", body.errors);
    }

    Ok(body)
}
