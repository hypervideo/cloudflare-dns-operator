use k8s_openapi::api::core::v1::{
    ConfigMap,
    Secret,
};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{
    Deserialize,
    Serialize,
};

/// https://developers.cloudflare.com/dns/manage-dns-records/reference/dns-record-types/#dns-record-types
#[allow(clippy::upper_case_acronyms)]
#[derive(Default, Debug, Serialize, Deserialize, Clone, Copy, JsonSchema)]
pub enum RecordType {
    #[default]
    #[serde(rename = "A")]
    A,
    #[serde(rename = "AAAA")]
    AAAA,
    #[serde(rename = "CNAME")]
    CNAME,
    #[serde(rename = "MX")]
    MX,
    #[serde(rename = "TXT")]
    TXT,
    #[serde(rename = "SRV")]
    SRV,
    #[serde(rename = "LOC")]
    LOC,
    #[serde(rename = "SPF")]
    SPF,
    #[serde(rename = "NS")]
    NS,
}

impl std::str::FromStr for RecordType {
    type Err = eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "A" => Ok(RecordType::A),
            "AAAA" => Ok(RecordType::AAAA),
            "CNAME" => Ok(RecordType::CNAME),
            "MX" => Ok(RecordType::MX),
            "TXT" => Ok(RecordType::TXT),
            "SRV" => Ok(RecordType::SRV),
            "LOC" => Ok(RecordType::LOC),
            "SPF" => Ok(RecordType::SPF),
            "NS" => Ok(RecordType::NS),
            s => Err(eyre::eyre!("Invalid RecordType: {s:?}")),
        }
    }
}

// Define the spec of our custom resource
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "dns.cloudflare.com",
    version = "v1alpha1",
    kind = "CloudflareDNSRecord",
    namespaced
)]
pub struct CloudflareDNSRecordSpec {
    /// The name of the record (e.g example.com)
    pub name: String,
    /// The type of the record (e.g A, CNAME, MX, TXT, SRV, LOC, SPF, NS). Defaults to A.
    #[serde(rename = "type")]
    pub type_: Option<RecordType>,
    /// The content of the record
    pub content: StringOrService,
    /// TTL in seconds
    pub ttl: Option<i64>,
    /// Whether the record is proxied by Cloudflare
    pub proxied: Option<bool>,
    /// Arbitrary comment
    pub comment: Option<String>,
    /// Tags to apply to the record
    pub tags: Option<Vec<String>>,
    /// The cloudflare zone ID to create the record in
    pub zone: ZoneNameOrId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum ZoneNameOrId {
    #[serde(rename = "name")]
    Name(ValueOrReference),
    #[serde(rename = "id")]
    Id(ValueOrReference),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum StringOrService {
    #[serde(rename = "value")]
    Value(String),
    #[serde(rename = "service")]
    Service(ServiceSelector),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceSelector {
    /// Service name
    pub name: String,
    /// Namespace, default is the same namespace as the referent.
    pub namespace: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum ValueOrReference {
    #[serde(rename = "value")]
    Value(String),
    #[serde(rename = "from")]
    Reference(Reference),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum Reference {
    #[serde(rename = "configMap")]
    ConfigMap(k8s_openapi::api::core::v1::ConfigMapKeySelector),

    #[serde(rename = "secret")]
    Secret(k8s_openapi::api::core::v1::SecretKeySelector),
}

impl ValueOrReference {
    pub async fn lookup(&self, client: &kube::Client, ns: &str) -> eyre::Result<Option<String>> {
        match self {
            ValueOrReference::Value(value) => Ok(Some(value.clone())),
            ValueOrReference::Reference(reference) => reference.lookup(client, ns).await,
        }
    }
}

impl Reference {
    pub async fn lookup(&self, client: &kube::Client, ns: &str) -> eyre::Result<Option<String>> {
        match self {
            Reference::ConfigMap(selector) => {
                trace!(name = %selector.name, %ns, key = %selector.key, "configmap reference lookup");
                let config_map = kube::api::Api::<ConfigMap>::namespaced(client.clone(), ns)
                    .get(&selector.name)
                    .await?;
                let value = config_map.data.and_then(|data| data.get(&selector.key).cloned());
                trace!(value = ?value, "configmap reference lookup result");
                Ok(value)
            }
            Reference::Secret(selector) => {
                trace!(name = %selector.name, %ns, key = %selector.key, "secret reference lookup");
                let secret = kube::api::Api::<Secret>::namespaced(client.clone(), ns)
                    .get(&selector.name)
                    .await?;
                let result = secret
                    .string_data
                    .and_then(|data| data.get(&selector.key).cloned())
                    .or_else(|| {
                        secret.data.and_then(|data| {
                            data.get(&selector.key).and_then(|bytes| {
                                use base64::prelude::*;
                                if let Ok(decoded) = String::from_utf8(bytes.0.clone()) {
                                    trace!("secret reference lookup result string");
                                    return Some(decoded);
                                }
                                if let Some(decoded) = BASE64_STANDARD.decode(&bytes.0).ok().and_then(|decoded| String::from_utf8(decoded).ok()) {
                                    return Some(decoded);
                                };
                                error!(name = %selector.name, %ns, "unable to decode secret reference value as utf8 or base64");
                                None
                            })
                        })
                    });
                Ok(result)
            }
        }
    }
}
