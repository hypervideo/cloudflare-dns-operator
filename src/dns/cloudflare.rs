use super::util;
use crate::resources::RecordType;
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
    list_zone_cache: Arc<Mutex<Option<(DateTime<Utc>, Vec<AccountInfo>)>>>,
    list_dns_records_cache: Arc<Mutex<HashMap<String, (DateTime<Utc>, Vec<DnsRecordInfo>)>>>,
}

impl CloudflareApi {
    pub fn new(api_token: String) -> Self {
        Self {
            api_token,
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

        let url = "https://api.cloudflare.com/client/v4/zones";
        let zones = cloudflare_api_request::<Vec<AccountInfo>, ()>(url, None, Method::GET, &self.api_token).await?;
        *cache = Some((Utc::now(), zones.clone()));

        Ok(zones)
    }

    /// List DNS records in a cloudflare zone.
    pub async fn list_dns_records(&self, zone_identifier: impl AsRef<str>) -> Result<Vec<DnsRecordInfo>> {
        const CACHE_DURATION: Duration = Duration::minutes(1);

        let zone_identifier = zone_identifier.as_ref();
        let mut cache = self.list_dns_records_cache.lock().await;

        if let Some((time, records)) = cache.get(zone_identifier) {
            if Utc::now() - *time < CACHE_DURATION {
                return Ok(records.clone());
            }
        }

        let url = format!("https://api.cloudflare.com/client/v4/zones/{zone_identifier}/dns_records");
        let records =
            cloudflare_api_request::<Vec<DnsRecordInfo>, ()>(&url, None, Method::GET, &self.api_token).await?;
        cache.insert(zone_identifier.to_string(), (Utc::now(), records.clone()));

        info!("Found the following records:");
        for record in &records {
            info!(%record);
        }

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
        let url = format!("https://api.cloudflare.com/client/v4/zones/{zone_identifier}/dns_records");
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
    pub async fn update_dns_record_and_wait(&self, args: CreateRecordArgs) -> Result<DnsRecordInfo, eyre::Error> {
        let Some(zone_id) = args.zone.clone().lookup_id(self).await? else {
            bail!("zone not found");
        };

        let domain = args.name.clone();
        let dns_records = self.list_dns_records(&zone_id).await?;

        if let Some(existing) = dns_records.into_iter().find(|record| record.name == domain) {
            if existing.content == args.content {
                info!("DNS record for {domain:?} already exists with {:?}", args.content);
                return Ok(existing);
            }

            warn!(
                "Found existing DNS record for web domain {domain:?} with ip {:?}. Deleting.",
                existing.content
            );
            self.delete_dns_record(&zone_id, &existing.id)
                .await
                .context("Failed to delete existing DNS record")?;
        }

        info!("Creating new DNS record for {domain:?} with {:?}", args.content);
        let record = self.create_dns_record(args).await?;
        debug!("Registered record for {domain:?} with {:?}", record.content);

        self.invalidate_dns_record_cache(zone_id).await;

        Ok(record)
    }

    /// Delete a DNS record by its (domain) name using the cloudflare API
    #[allow(dead_code)]
    pub async fn delete_dns_record_by_name(
        &self,
        name: impl AsRef<str>,
        zone_identifier: impl AsRef<str>,
    ) -> Result<(), eyre::Error> {
        let name = name.as_ref();
        let zone_identifier = zone_identifier.as_ref();

        info!(?name, "deleting dns record by name");
        let record = self
            .list_dns_records(&zone_identifier)
            .await?
            .into_iter()
            .find(|it| it.name == name);
        let Some(record) = record else {
            bail!("no record found with name: {name}");
        };

        self.delete_dns_record(zone_identifier, record.id).await?;

        Ok(())
    }

    /// Delete a DNS record by its id using the cloudflare API.
    pub async fn delete_dns_record(&self, zone_identifier: impl AsRef<str>, id: impl AsRef<str>) -> Result<()> {
        let zone_identifier = zone_identifier.as_ref();
        let id = id.as_ref();
        let url = format!("https://api.cloudflare.com/client/v4/zones/{zone_identifier}/dns_records/{id}");

        cloudflare_api_request::<Value, ()>(&url, None, Method::DELETE, &self.api_token).await?;

        self.invalidate_dns_record_cache(zone_identifier).await;

        Ok(())
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
    let req = reqwest::Client::new()
        .request(method, url)
        .bearer_auth(api_token.as_ref())
        .header("Content-Type", "application/json");

    let req = if let Some(body) = body { req.json(&body) } else { req };

    let res = req.send().await?;

    if !res.status().is_success() {
        bail!(
            "cloudflare api error: status={:?}, body={:?}",
            res.status(),
            res.text().await?
        );
    }

    #[cfg(debug_assertions)]
    let body: ApiResult<_> = {
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

    Ok(body.result)
}
