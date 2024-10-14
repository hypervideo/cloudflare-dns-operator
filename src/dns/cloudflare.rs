use super::util;
use crate::resources::RecordType;
use chrono::prelude::*;
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
#[derive(Debug, Serialize, Deserialize)]
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

#[derive(Debug, Serialize, Deserialize)]
pub struct Account {
    id: String,
    name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Owner {
    email: Option<String>,
    id: Option<String>,
    r#type: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
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
#[derive(Debug, Serialize, Deserialize)]
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
    pub zone_id: String,
    pub zone_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
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
#[derive(Debug, Serialize, Deserialize)]
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

    pub async fn resolve(self, api_token: &str) -> Result<Option<Self>> {
        self.lookup_id(api_token).await.map(|id| id.map(Zone::Identifier))
    }

    pub async fn lookup_id(self, api_token: &str) -> Result<Option<String>> {
        match self {
            Zone::Identifier(id) => Ok(Some(id)),
            Zone::Name(name) => {
                debug!(?name, "looking up zone by name");
                let accounts = list_zones(api_token).await?;
                Ok(accounts.into_iter().find(|it| it.name == name).map(|it| it.id))
            }
        }
    }
}

/// Arguments for [`create_dns_record`].
pub struct CreateRecordArgs {
    pub api_token: String,
    pub zone: Zone,
    pub name: String,
    pub record_type: RecordType,
    pub content: String,
    pub comment: Option<String>,
    pub ttl: Option<i64>,
}

/// List all cloudflare accounts which represent zones.
pub async fn list_zones(api_token: &str) -> Result<Vec<AccountInfo>, eyre::Error> {
    let url = "https://api.cloudflare.com/client/v4/zones";
    request::<Vec<AccountInfo>, ()>(url, None, Method::GET, api_token).await
}

/// Create a new cloudflare dns record
pub async fn create_dns_record(args: CreateRecordArgs) -> Result<DnsRecordInfo, eyre::Error> {
    let CreateRecordArgs {
        api_token,
        zone,
        name,
        record_type,
        content,
        comment,
        ttl,
    } = args;

    let zone_identifier = zone
        .lookup_id(&api_token)
        .await?
        .ok_or_else(|| eyre::eyre!("zone not found"))?;

    let url = format!("https://api.cloudflare.com/client/v4/zones/{zone_identifier}/dns_records");
    let id = util::id();

    info!(?id, ?name, r#type = ?record_type, "creating dns record");

    request::<DnsRecordInfo, _>(
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
        api_token,
    )
    .await
}

/// Updates a cloudflare dns record... currently deletes and recreates... Will wait for the dns record to propagate,
/// i.e. a dns lookup resolves to the correct ip.
// TODO: we should use the proper patch api.
pub async fn update_dns_record_and_wait(args: CreateRecordArgs) -> Result<DnsRecordInfo, eyre::Error> {
    let Some(zone_id) = args.zone.clone().lookup_id(&args.api_token).await? else {
        bail!("zone not found");
    };
    let api_token = args.api_token.clone();
    let domain = args.name.clone();

    let dns_records = list_dns_records(&zone_id, &api_token).await?;
    if let Some(existing) = dns_records.into_iter().find(|record| record.name == domain) {
        if existing.content == args.content {
            info!("DNS record for {domain:?} already exists with {:?}", args.content);
            return Ok(existing);
        }

        warn!(
            "Found existing DNS record for web domain {domain:?} with ip {:?}. Deleting.",
            existing.content
        );
        delete_dns_record(&zone_id, &existing.id, &api_token)
            .await
            .context("Failed to delete existing DNS record")?;
    }

    info!("Creating new DNS record for {domain:?} with {:?}", args.content);

    let record = create_dns_record(args).await?;

    debug!("Registered record for {domain:?} with {:?}", record.content);

    Ok(record)
}

/// Delete a DNS record by its (domain) name using the cloudflare API
#[allow(dead_code)]
pub async fn delete_dns_record_by_name(
    name: impl AsRef<str>,
    zone_identifier: impl AsRef<str>,
    api_token: impl AsRef<str>,
) -> Result<(), eyre::Error> {
    let name = name.as_ref();
    let zone_identifier = zone_identifier.as_ref();

    info!(?name, "deleting dns record by name");

    let record = list_dns_records(&zone_identifier, api_token.as_ref())
        .await?
        .into_iter()
        .find(|it| it.name == name);

    let Some(record) = record else {
        bail!("no record found with name: {name}");
    };

    delete_dns_record(zone_identifier, record.id, api_token).await?;

    Ok(())
}

/// Delete a DNS record by its id using the cloudflare API.
pub async fn delete_dns_record(
    zone_identifier: impl AsRef<str>,
    id: impl AsRef<str>,
    api_token: impl AsRef<str>,
) -> Result<()> {
    let zone_identifier = zone_identifier.as_ref();
    let id = id.as_ref();
    let url = format!("https://api.cloudflare.com/client/v4/zones/{zone_identifier}/dns_records/{id}");

    request::<Value, ()>(&url, None, Method::DELETE, api_token).await?;

    Ok(())
}

/// List DNS records in a cloudflare zone.
pub async fn list_dns_records(
    zone_identifier: impl AsRef<str>,
    api_token: impl AsRef<str>,
) -> Result<Vec<DnsRecordInfo>> {
    let zone_identifier = zone_identifier.as_ref();
    let url = format!("https://api.cloudflare.com/client/v4/zones/{zone_identifier}/dns_records");
    request::<Vec<DnsRecordInfo>, ()>(&url, None, Method::GET, api_token).await
}

async fn request<R, B>(url: &str, body: Option<B>, method: Method, api_token: impl AsRef<str>) -> Result<R>
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
