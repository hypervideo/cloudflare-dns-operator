use super::conditions::{
    error_condition,
    success_condition,
};
use crate::{
    context::Context,
    dns::cloudflare::{
        self,
        Zone,
    },
    dns_check::DnsCheckRequest,
    resources::{
        normalize_dns_name,
        CloudflareDNSRecord,
        CloudflareDNSRecordStatus,
        RecordType,
        ZoneNameOrId,
    },
};
use eyre::OptionExt as _;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::{
    api::{
        ListParams,
        ObjectMeta,
        Patch,
        PatchParams,
    },
    Api,
};
use serde::{
    Deserialize,
    Serialize,
};
use std::sync::Arc;

#[derive(thiserror::Error, Debug)]
pub enum ReconcileError {
    #[error(transparent)]
    Kube(#[from] kube::Error),

    #[error(transparent)]
    Other(#[from] eyre::Error),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct AnnotationContent {
    record_id: String,
    api_token: String,
    zone_id: String,
}

pub async fn apply(resource: Arc<CloudflareDNSRecord>, ctx: Arc<Context>) -> Result<(), ReconcileError> {
    let _reconcile_guard = ctx.reconcile_lock.lock().await;

    let client = &ctx.client;
    let ns = resource.metadata.namespace.as_deref().unwrap_or("default");
    let name = resource.metadata.name.as_deref().ok_or_eyre("missing name")?;

    info!("processing reconcile request");

    // Read the live object so rename cleanup sees the latest status and a deleted or replaced object does not
    // trigger Cloudflare writes.
    let api = Api::<CloudflareDNSRecord>::namespaced(client.clone(), ns);
    let Some(live) = api.get_opt(name).await? else {
        debug!("CloudflareDNSRecord {ns}/{name} already deleted, skipping apply");
        return Ok(());
    };
    if live.metadata.uid != resource.metadata.uid || live.metadata.deletion_timestamp.is_some() {
        debug!("CloudflareDNSRecord {ns}/{name} is deleting or was replaced, skipping apply");
        return Ok(());
    }

    let gen = live.metadata.generation;
    let old_status = live.status.clone();
    let needs_initial_dns_check = old_status.as_ref().is_none_or(|status| status.record_id.is_empty());
    let domain_or_record_text = live.spec.name.as_str();
    let record_type = live.spec.ty.unwrap_or_default();

    let Some(content) = live.spec.lookup_content(client, ns).await? else {
        let msg = format!("unable to resolve content for CloudflareDNSRecord {ns}/{name}");
        error!("{msg}");
        update_conditions(&live, &ctx, vec![error_condition(&live, "missing content", msg, gen)]).await?;
        return Ok(());
    };

    let Some(zone_id) = resolve_zone_id(&live.spec.zone, client, ns, &ctx.cloudflare_api).await? else {
        let msg = format!(
            "unable to resolve {:?} for CloudflareDNSRecord {ns}/{name}",
            live.spec.zone
        );
        error!("{msg}");
        update_conditions(&live, &ctx, vec![error_condition(&live, "missing zone", msg, gen)]).await?;
        return Ok(());
    };

    let all_records = Api::<CloudflareDNSRecord>::all(client.clone())
        .list(&ListParams::default())
        .await?
        .items;
    let cf_records = ctx.cloudflare_api.list_dns_records(&zone_id).await?;

    let norm_name = normalize_dns_name(domain_or_record_text);
    if let Some(conflict) = find_conflicting_peer(
        &live,
        &zone_id,
        &norm_name,
        record_type,
        &content,
        &all_records,
        &cf_records,
        &ctx,
    )
    .await?
    {
        let msg = format!(
            "Cloudflare record {norm_name} ({record_type:?}) in zone {zone_id} conflicts with {}",
            conflict.winner_label
        );
        warn!("{msg}");

        // A refused record normally releases any previous Cloudflare record it held and clears its status IDs.
        // Exception: if its old record is the exact live Cloudflare record the winner matches and no other peer
        // holds that reference in status yet, keep the status reference until the winner adopts it (prevents
        // deleting the winner's record and flipping the tie-break back, and prevents a leak if the winner is
        // deleted before reconciling).
        if let Some(old) = old_status
            .as_ref()
            .filter(|status| !status.zone_id.is_empty() && !status.record_id.is_empty())
        {
            let old_cf = (old.zone_id == zone_id)
                .then(|| cf_records.iter().find(|record| record.id == old.record_id))
                .flatten();
            let peer_already_owns_old =
                has_referencing_peer(&live, &old.zone_id, &old.record_id, &all_records, old_cf, &ctx).await;
            let winner_relies_on_old = old_cf.is_some_and(|cf| {
                normalize_dns_name(&cf.name) == norm_name
                    && cf.record_type.eq_ignore_ascii_case(conflict.winner_type.as_str())
                    && cf.content == conflict.winner_content
            });
            if winner_relies_on_old && !peer_already_owns_old {
                update_conditions(&live, &ctx, vec![error_condition(&live, "NameConflict", msg, gen)]).await?;
                return Ok(());
            }
            if !peer_already_owns_old {
                ctx.cloudflare_api
                    .delete_dns_record(&old.zone_id, &old.record_id)
                    .await?;
            }
        }
        let status = CloudflareDNSRecordStatus {
            record_id: String::new(),
            zone_id: String::new(),
            pending: false,
            conditions: Some(vec![error_condition(&live, "NameConflict", msg, gen)]),
        };
        write_status(&live, &ctx, status).await?;
        return Ok(());
    }

    // Delete-before-create on rename (zone, normalized name, or record type changed):
    // - Allows changing between A/AAAA and CNAME at the same name (Cloudflare rejects creating the new type first).
    // - Avoids leaking a newly created record if deleting the old record fails before status is updated.
    if let Some(old) = old_status
        .as_ref()
        .filter(|status| !status.zone_id.is_empty() && !status.record_id.is_empty())
    {
        let old_cf = (old.zone_id == zone_id)
            .then(|| cf_records.iter().find(|record| record.id == old.record_id))
            .flatten();
        let moved_record = if old.zone_id != zone_id {
            true
        } else {
            old_cf.is_some_and(|cf| {
                normalize_dns_name(&cf.name) != norm_name || !cf.record_type.eq_ignore_ascii_case(record_type.as_str())
            })
        };
        if moved_record {
            delete_cloudflare_record_if_unreferenced(&ctx, &live, &old.zone_id, &old.record_id, &all_records, old_cf)
                .await?;
        }
    }

    debug!("updating dns record for CloudflareDNSRecord {ns}/{name}");

    let record = ctx
        .cloudflare_api
        .update_dns_record_and_wait(cloudflare::CreateRecordArgs {
            zone: Zone::Identifier(zone_id.clone()),
            name: domain_or_record_text.to_string(),
            record_type,
            content,
            comment: live.spec.comment.clone(),
            ttl: live.spec.ttl,
        })
        .await?;

    // -=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-

    let status_key = format!("{ns}:{name}");

    let pending = if ctx.do_dns_check {
        !ctx.dns_lookup_success
            .lock()
            .await
            .get(&status_key)
            .cloned()
            .unwrap_or_default()
    } else {
        false
    };
    let condition = if !pending {
        success_condition(&live, gen)
    } else {
        let msg = "The DNS record has not propagated yet. This is expected to take some time.".to_string();
        error_condition(&live, "pending", msg, gen)
    };

    let status = CloudflareDNSRecordStatus {
        // We are storing the details about how we created the record in the
        // status. At deletion time, the configmap / secrets we got the
        // zone_id from might be gone already.
        record_id: record.id,
        zone_id,
        pending,
        conditions: Some(vec![condition]),
    };

    if needs_initial_dns_check && ctx.do_dns_check {
        let _ = ctx
            .dns_check_tx
            .send(DnsCheckRequest::CheckSingleRecord {
                name: name.to_string(),
                namespace: ns.to_string(),
            })
            .await;
    }

    write_status(&live, &ctx, status).await
}

/// Runs before the resource is deleted. Deletes the DNS record from Cloudflare unless another non-deleting
/// `CloudflareDNSRecord` still references it, or returns an error so `kube::runtime::finalizer` keeps the finalizer
/// and retries.
#[instrument(level = "debug", skip_all)]
pub async fn cleanup(resource: Arc<CloudflareDNSRecord>, ctx: Arc<Context>) -> Result<(), ReconcileError> {
    let _reconcile_guard = ctx.reconcile_lock.lock().await;

    let ns = resource.metadata.namespace.as_deref().unwrap_or("default");
    let name = resource.metadata.name.as_deref().ok_or_eyre("missing name")?;

    info!("delete request: CloudflareDNSRecord {ns}/{name}");

    // Read the live object from the API server so we do not miss status written by apply before the cache updated.
    let api = Api::<CloudflareDNSRecord>::namespaced(ctx.client.clone(), ns);
    let Some(live) = api.get_opt(name).await? else {
        warn!("CloudflareDNSRecord {ns}/{name} already deleted, skipping cleanup");
        return Ok(());
    };

    // If the record was deleted and recreated with the same name while this cleanup was queued, do not touch the
    // replacement's Cloudflare record or let the finalizer helper strip the replacement's finalizer.
    if live.metadata.uid != resource.metadata.uid {
        return Err(eyre::eyre!(
            "CloudflareDNSRecord {ns}/{name} was replaced before cleanup (uid {:?} != {:?})",
            resource.metadata.uid,
            live.metadata.uid
        )
        .into());
    }

    let Some(status) = live
        .status
        .as_ref()
        .filter(|status| !status.zone_id.is_empty() && !status.record_id.is_empty())
    else {
        warn!("no Cloudflare record ID in live status for CloudflareDNSRecord {ns}/{name}, skipping Cloudflare delete");
        return Ok(());
    };

    let all_records = Api::<CloudflareDNSRecord>::all(ctx.client.clone())
        .list(&ListParams::default())
        .await?
        .items;
    delete_cloudflare_record_if_unreferenced(&ctx, &live, &status.zone_id, &status.record_id, &all_records, None)
        .await?;

    Ok(())
}

async fn resolve_zone_id(
    zone_spec: &ZoneNameOrId,
    client: &kube::Client,
    ns: &str,
    cloudflare_api: &cloudflare::CloudflareApi,
) -> Result<Option<String>, ReconcileError> {
    let zone = match zone_spec {
        ZoneNameOrId::Name(it) => it.lookup(client, ns).await?.map(Zone::name),
        ZoneNameOrId::Id(it) => it.lookup(client, ns).await?.map(Zone::id),
    };
    let Some(zone) = zone else {
        return Ok(None);
    };
    let Some(Zone::Identifier(zone_id)) = zone.resolve(cloudflare_api).await? else {
        return Ok(None);
    };
    Ok(Some(zone_id))
}

fn is_same_record(a: &CloudflareDNSRecord, b: &CloudflareDNSRecord) -> bool {
    match (&a.metadata.uid, &b.metadata.uid) {
        (Some(uid_a), Some(uid_b)) => uid_a == uid_b,
        _ => {
            a.metadata.namespace.as_deref().unwrap_or("default") == b.metadata.namespace.as_deref().unwrap_or("default")
                && a.metadata.name == b.metadata.name
        }
    }
}

struct NameConflict {
    winner_label: String,
    winner_type: RecordType,
    winner_content: String,
}

fn owned_cf_record<'a>(
    record: &CloudflareDNSRecord,
    zone_id: &str,
    name_cf_records: &[&'a cloudflare::DnsRecordInfo],
) -> Option<&'a cloudflare::DnsRecordInfo> {
    let status = record.status.as_ref()?;
    if status.zone_id != zone_id || status.record_id.is_empty() {
        return None;
    }
    name_cf_records.iter().copied().find(|cf| cf.id == status.record_id)
}

type ConflictRank<'a> = (
    bool,
    bool,
    bool,
    Option<chrono::DateTime<chrono::Utc>>,
    &'a str,
    &'a str,
    &'a str,
);

/// Deterministic total order for breaking conflicts at `(zone_id, norm_name)`.
/// Lower rank wins:
/// 1. Matches the record (type + content) that Cloudflare currently serves at this name.
/// 2. Holds `status.record_id` pointing to one of those live Cloudflare records at this name.
/// 3. Oldest `creationTimestamp`, then `namespace`, `name`, `uid`.
fn conflict_rank(record: &CloudflareDNSRecord, matches_cf_now: bool, owns_this_cf_record: bool) -> ConflictRank<'_> {
    let created = record.metadata.creation_timestamp.as_ref().map(|t| t.0);
    (
        !matches_cf_now,
        !owns_this_cf_record,
        created.is_none(),
        created,
        record.metadata.namespace.as_deref().unwrap_or("default"),
        record.metadata.name.as_deref().unwrap_or(""),
        record.metadata.uid.as_deref().unwrap_or(""),
    )
}

/// Evaluates all live (non-deleting) claimants for `(zone_id, norm_name)` across all record types in one
/// rank-ordered acceptance pass.
/// A claimant is accepted unless an already-accepted higher-ranked claimant has the same type with different
/// content, or a conflicting different type (such as `CNAME` vs `A`/`AAAA`).
/// Unresolved peers that own a live Cloudflare record at this name keep their rank using their live Cloudflare
/// type and content; unresolved peers that own nothing here are skipped so a broken peer cannot block the owner.
#[allow(clippy::too_many_arguments)]
async fn find_conflicting_peer(
    live: &CloudflareDNSRecord,
    zone_id: &str,
    norm_name: &str,
    record_type: RecordType,
    content: &str,
    all_records: &[CloudflareDNSRecord],
    cf_records: &[cloudflare::DnsRecordInfo],
    ctx: &Context,
) -> Result<Option<NameConflict>, ReconcileError> {
    struct Claimant<'a> {
        is_self: bool,
        label: String,
        record_type: RecordType,
        content: String,
        rank: ConflictRank<'a>,
    }

    fn claimants_conflict(a: &Claimant<'_>, b: &Claimant<'_>) -> bool {
        if a.record_type == b.record_type {
            a.content != b.content
        } else {
            a.record_type.conflicts_with(b.record_type)
        }
    }

    let name_cf_records: Vec<&cloudflare::DnsRecordInfo> = cf_records
        .iter()
        .filter(|cf| normalize_dns_name(&cf.name) == norm_name)
        .collect();

    let self_ns = live.metadata.namespace.as_deref().unwrap_or("default");
    let self_name = live.metadata.name.as_deref().unwrap_or("");
    let self_owns = owned_cf_record(live, zone_id, &name_cf_records).is_some();
    let self_matches_cf = name_cf_records
        .iter()
        .any(|cf| cf.record_type.eq_ignore_ascii_case(record_type.as_str()) && cf.content == content);

    let mut claimants = vec![Claimant {
        is_self: true,
        label: format!("{self_ns}/{self_name}"),
        record_type,
        content: content.to_string(),
        rank: conflict_rank(live, self_matches_cf, self_owns),
    }];

    for peer in all_records {
        if is_same_record(live, peer) || peer.metadata.deletion_timestamp.is_some() {
            continue;
        }
        if normalize_dns_name(&peer.spec.name) != norm_name {
            continue;
        }

        let peer_ns = peer.metadata.namespace.as_deref().unwrap_or("default");
        let peer_name = peer.metadata.name.as_deref().unwrap_or("");
        let peer_cf = owned_cf_record(peer, zone_id, &name_cf_records);

        match resolve_zone_id(&peer.spec.zone, &ctx.client, peer_ns, &ctx.cloudflare_api).await {
            Ok(Some(id)) if id != zone_id => continue,
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                if peer_cf.is_none() {
                    continue;
                }
            }
        }

        let (peer_type, peer_content) = match peer.spec.lookup_content(&ctx.client, peer_ns).await {
            Ok(Some(value)) => (peer.spec.ty.unwrap_or_default(), value),
            Ok(None) | Err(_) => {
                let Some(cf) = peer_cf else {
                    continue;
                };
                let cf_type = cf
                    .record_type
                    .parse::<RecordType>()
                    .unwrap_or_else(|_| peer.spec.ty.unwrap_or_default());
                (cf_type, cf.content.clone())
            }
        };

        let peer_matches_cf = name_cf_records
            .iter()
            .any(|cf| cf.record_type.eq_ignore_ascii_case(peer_type.as_str()) && cf.content == peer_content);

        claimants.push(Claimant {
            is_self: false,
            label: format!("{peer_ns}/{peer_name}"),
            record_type: peer_type,
            content: peer_content,
            rank: conflict_rank(peer, peer_matches_cf, peer_cf.is_some()),
        });
    }

    claimants.sort_by_key(|claimant| claimant.rank);

    let mut accepted: Vec<&Claimant<'_>> = Vec::new();
    for claimant in &claimants {
        if let Some(winner) = accepted.iter().find(|acc| claimants_conflict(acc, claimant)) {
            if claimant.is_self {
                return Ok(Some(NameConflict {
                    winner_label: winner.label.clone(),
                    winner_type: winner.record_type,
                    winner_content: winner.content.clone(),
                }));
            }
            continue;
        }
        accepted.push(claimant);
    }

    let shared_peers: Vec<&str> = accepted
        .iter()
        .filter(|c| !c.is_self && c.record_type == record_type && c.content == content)
        .map(|c| c.label.as_str())
        .collect();
    if !shared_peers.is_empty() {
        warn!(
            "CloudflareDNSRecord {self_ns}/{self_name} shares {norm_name} ({record_type:?}) in zone {zone_id} with {}",
            shared_peers.join(", ")
        );
    }

    Ok(None)
}

/// Whether another live (non-deleting) `CloudflareDNSRecord` still references `(zone_id, record_id)`.
/// When `cf_record` is provided, a peer only counts if its current spec still targets that Cloudflare record's
/// `(normalized name, type)` and has not moved to a different resolved zone.
async fn has_referencing_peer(
    self_rec: &CloudflareDNSRecord,
    zone_id: &str,
    record_id: &str,
    all_records: &[CloudflareDNSRecord],
    cf_record: Option<&cloudflare::DnsRecordInfo>,
    ctx: &Context,
) -> bool {
    let cf_norm_name = cf_record.map(|cf| normalize_dns_name(&cf.name));

    for peer in all_records {
        if is_same_record(self_rec, peer) || peer.metadata.deletion_timestamp.is_some() {
            continue;
        }
        if !peer
            .status
            .as_ref()
            .is_some_and(|status| status.zone_id == zone_id && status.record_id == record_id)
        {
            continue;
        }

        if let (Some(cf), Some(cf_name)) = (cf_record, cf_norm_name.as_deref()) {
            let peer_type = peer.spec.ty.unwrap_or_default();
            if normalize_dns_name(&peer.spec.name) != cf_name
                || !cf.record_type.eq_ignore_ascii_case(peer_type.as_str())
            {
                continue;
            }
        }

        let peer_ns = peer.metadata.namespace.as_deref().unwrap_or("default");
        if let Ok(Some(peer_zone_id)) =
            resolve_zone_id(&peer.spec.zone, &ctx.client, peer_ns, &ctx.cloudflare_api).await
        {
            if peer_zone_id != zone_id {
                continue;
            }
        }

        let peer_name = peer.metadata.name.as_deref().unwrap_or("");
        info!(
            "Cloudflare record {zone_id}/{record_id} is still referenced by {peer_ns}/{peer_name}, skipping Cloudflare delete"
        );
        return true;
    }

    false
}

/// Deletes `(zone_id, record_id)` from Cloudflare unless another live (non-deleting) `CloudflareDNSRecord` in the
/// cluster still references it.
async fn delete_cloudflare_record_if_unreferenced(
    ctx: &Context,
    self_rec: &CloudflareDNSRecord,
    zone_id: &str,
    record_id: &str,
    all_records: &[CloudflareDNSRecord],
    cf_record: Option<&cloudflare::DnsRecordInfo>,
) -> Result<(), ReconcileError> {
    if has_referencing_peer(self_rec, zone_id, record_id, all_records, cf_record, ctx).await {
        return Ok(());
    }

    ctx.cloudflare_api.delete_dns_record(zone_id, record_id).await?;
    Ok(())
}

pub async fn update_conditions(
    resource: &CloudflareDNSRecord,
    ctx: &Context,
    conditions: Vec<Condition>,
) -> Result<(), ReconcileError> {
    let name = resource.metadata.name.as_deref().ok_or_eyre("missing name")?;
    let ns = resource.metadata.namespace.as_deref().unwrap_or("default");

    // Read the live status before applying so a stale cache without status cannot overwrite record_id/zone_id with "".
    let api = Api::<CloudflareDNSRecord>::namespaced(ctx.client.clone(), ns);
    let Some(live) = api.get_opt(name).await? else {
        return Ok(());
    };
    if live.metadata.uid != resource.metadata.uid {
        return Ok(());
    }

    let status = CloudflareDNSRecordStatus {
        conditions: Some(conditions),
        ..live.status.unwrap_or_default()
    };
    write_status(resource, ctx, status).await
}

/// Writes the status, the only part of a CloudflareDNSRecord that the reconcile changes. It patches the status
/// subresource, which, unlike the object itself, the API server never creates. So a reconcile that runs while the
/// object gets deleted cannot bring the object back.
async fn write_status(
    resource: &CloudflareDNSRecord,
    ctx: &Context,
    status: CloudflareDNSRecordStatus,
) -> Result<(), ReconcileError> {
    let name = resource.metadata.name.as_deref().ok_or_eyre("missing name")?;
    let ns = resource.metadata.namespace.as_deref().unwrap_or("default");

    let patched = CloudflareDNSRecord {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        spec: resource.spec.clone(),
        status: Some(status),
    };

    Api::<CloudflareDNSRecord>::namespaced(ctx.client.clone(), ns)
        .patch_status(name, &PatchParams::apply("dns.cloudflare.com"), &Patch::Apply(&patched))
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloudflare::MockCloudflare;
    use http::{
        Method,
        Request,
        Response,
    };
    use kube::client::Body;
    use serde_json::json;

    #[tokio::test]
    async fn status_write_cannot_recreate_a_deleted_record() {
        // regression: the reconcile applied the whole object, which recreated a record that was deleted while the
        // reconcile ran (pr-6843-admin-dns, 2026-11-26)
        let (service, mut api_server) = tower_test::mock::pair::<Request<Body>, Response<Body>>();
        let ctx = Context {
            client: kube::Client::new(service, "default"),
            cloudflare_api: cloudflare::CloudflareApi::new(String::new()),
            do_dns_check: false,
            dns_check_tx: tokio::sync::mpsc::channel(1).0,
            dns_lookup_success: Default::default(),
            reconcile_lock: Default::default(),
        };
        let spec =
            json!({ "name": "example.com", "content": { "value": "1.2.3.4" }, "zone": { "id": { "value": "zone" } } });
        let mut record = CloudflareDNSRecord::new("web", serde_json::from_value(spec).unwrap());
        record.metadata.namespace = Some("app".to_string());

        let deleted_record = async {
            let next_request = tokio::time::timeout(std::time::Duration::from_secs(5), api_server.next_request());
            let (request, respond) = next_request.await.expect("a request in time").expect("a request");
            assert_eq!(request.method(), Method::PATCH);
            assert_eq!(
                request.uri().path(),
                "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/web/status"
            );
            let not_found = json!({ "kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "NotFound", "code": 404, "message": "not found" });
            respond.send_response(
                Response::builder()
                    .status(404)
                    .body(Body::from(not_found.to_string().into_bytes()))
                    .unwrap(),
            );
        };
        let (result, ()) = tokio::join!(write_status(&record, &ctx, Default::default()), deleted_record);

        // main.rs logs this error as a warning
        assert!(matches!(result, Err(ReconcileError::Kube(kube::Error::Api(err))) if err.code == 404));
    }

    #[tokio::test]
    async fn apply_does_not_delete_record_when_cache_has_stale_name() {
        // regression: apply fetched the live record and deleted it when its spec.name differed from the cached copy,
        // then waited forever for the finalizer that only this controller removes
        let (service, mut api_server) = tower_test::mock::pair::<Request<Body>, Response<Body>>();
        let ctx = Arc::new(Context {
            client: kube::Client::new(service, "default"),
            cloudflare_api: cloudflare::CloudflareApi::new(String::new()),
            do_dns_check: false,
            dns_check_tx: tokio::sync::mpsc::channel(1).0,
            dns_lookup_success: Default::default(),
            reconcile_lock: Default::default(),
        });

        // A Service without a public IP makes apply stop with a "missing content" status before it calls Cloudflare,
        // so this test needs no Cloudflare mock.
        let cached_spec = json!({
            "name": "old.example.com",
            "content": { "service": { "name": "web-svc" } },
            "zone": { "id": { "value": "zone" } }
        });
        let mut cached = CloudflareDNSRecord::new("web", serde_json::from_value(cached_spec).unwrap());
        cached.metadata.namespace = Some("app".to_string());
        cached.metadata.uid = Some("uid-1".to_string());
        cached.metadata.finalizers = Some(vec!["dns.cloudflare.com/delete-dns-record".to_string()]);

        let mut live = cached.clone();
        live.spec.name = "new.example.com".to_string();
        let live_json = serde_json::to_string(&live).unwrap();

        let mock_api = async {
            let mut saw_delete = false;
            let mut saw_status_patch = false;
            while let Some((request, respond)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), api_server.next_request())
                    .await
                    .expect("api server timed out waiting for request")
            {
                let path = request.uri().path();
                match (request.method(), path) {
                    (&Method::GET, "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/web") => {
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(live_json.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (&Method::DELETE, "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/web") => {
                        saw_delete = true;
                        let err = json!({
                            "kind": "Status",
                            "apiVersion": "v1",
                            "status": "Failure",
                            "reason": "InternalError",
                            "code": 500,
                            "message": "unexpected self-delete"
                        });
                        respond.send_response(
                            Response::builder()
                                .status(500)
                                .body(Body::from(err.to_string().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (&Method::GET, "/api/v1/namespaces/app/services/web-svc") => {
                        let svc = json!({
                            "apiVersion": "v1",
                            "kind": "Service",
                            "metadata": { "name": "web-svc", "namespace": "app" },
                            "spec": { "type": "ClusterIP" }
                        });
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(svc.to_string().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (
                        &Method::PATCH,
                        "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/web/status",
                    ) => {
                        saw_status_patch = true;
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(live_json.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (method, path) => panic!("unexpected request: {method} {path}"),
                }
            }
            (saw_delete, saw_status_patch)
        };

        let run_apply = async move { apply(Arc::new(cached), ctx).await };
        let (result, (saw_delete, saw_status_patch)) = tokio::join!(run_apply, mock_api);

        assert!(!saw_delete, "apply must not delete the record it reconciles");
        assert!(saw_status_patch, "apply should write status instead of deleting");
        assert!(result.is_ok(), "apply returned error: {result:?}");
    }

    fn sample_record(status: Option<CloudflareDNSRecordStatus>) -> CloudflareDNSRecord {
        let spec = json!({
            "name": "app.example.com",
            "content": { "value": "1.2.3.4" },
            "zone": { "id": { "value": "zone-1" } }
        });
        let mut record = CloudflareDNSRecord::new("web", serde_json::from_value(spec).unwrap());
        record.metadata.namespace = Some("app".to_string());
        record.metadata.uid = Some("uid-1".to_string());
        record.status = status;
        record
    }

    fn sample_status(zone_id: &str, record_id: &str) -> CloudflareDNSRecordStatus {
        CloudflareDNSRecordStatus {
            record_id: record_id.to_string(),
            zone_id: zone_id.to_string(),
            pending: false,
            conditions: None,
        }
    }

    async fn run_cleanup_test(
        cached: CloudflareDNSRecord,
        get_status: u16,
        get_body: String,
        cf_status: u16,
        cf_body: &'static str,
    ) -> (Result<(), ReconcileError>, Vec<String>) {
        let cf = MockCloudflare::start(cf_status, cf_body);
        let (service, mut api_server) = tower_test::mock::pair::<Request<Body>, Response<Body>>();
        let ctx = Arc::new(Context {
            client: kube::Client::new(service, "default"),
            cloudflare_api: cloudflare::CloudflareApi::with_base_url("token".to_string(), &cf.base_url),
            do_dns_check: false,
            dns_check_tx: tokio::sync::mpsc::channel(1).0,
            dns_lookup_success: Default::default(),
            reconcile_lock: Default::default(),
        });

        let empty_list = json!({
            "apiVersion": "dns.cloudflare.com/v1alpha1",
            "kind": "CloudflareDNSRecordList",
            "metadata": {},
            "items": []
        })
        .to_string();

        let mock_api = async {
            while let Some((request, respond)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), api_server.next_request())
                    .await
                    .expect("api server timed out")
            {
                assert_eq!(request.method(), Method::GET);
                let (status, body) = match request.uri().path() {
                    "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/web" => {
                        (get_status, get_body.clone())
                    }
                    "/apis/dns.cloudflare.com/v1alpha1/cloudflarednsrecords" => (200, empty_list.clone()),
                    path => panic!("unexpected path: {path}"),
                };
                respond.send_response(
                    Response::builder()
                        .status(status)
                        .body(Body::from(body.into_bytes()))
                        .unwrap(),
                );
            }
        };

        let run_cleanup = async move { cleanup(Arc::new(cached), ctx).await };
        let (result, ()) = tokio::join!(run_cleanup, mock_api);
        (result, cf.finish())
    }

    #[tokio::test]
    async fn cleanup_deletes_cloudflare_record_using_live_status_when_cache_lacks_it() {
        // regression: cleanup read status from the controller cache, so a deletion right after apply
        // could see status=None and drop the finalizer without deleting the Cloudflare record
        let cached = sample_record(None);
        let live = sample_record(Some(sample_status("zone-1", "rec-1")));
        let (result, cf_requests) = run_cleanup_test(
            cached,
            200,
            serde_json::to_string(&live).unwrap(),
            200,
            r#"{"success":true,"errors":[],"messages":[],"result":{"id":"rec-1"}}"#,
        )
        .await;

        assert!(result.is_ok(), "cleanup failed: {result:?}");
        assert_eq!(cf_requests, vec!["DELETE /zones/zone-1/dns_records/rec-1"]);
    }

    #[tokio::test]
    async fn cleanup_keeps_finalizer_by_returning_error_on_cloudflare_delete_failure() {
        // regression: cleanup logged and ignored Cloudflare delete errors and returned Ok,
        // causing kube::runtime::finalizer to drop the finalizer while the Cloudflare record stayed behind
        let live = sample_record(Some(sample_status("zone-1", "rec-1")));
        let (result, cf_requests) = run_cleanup_test(
            live.clone(),
            200,
            serde_json::to_string(&live).unwrap(),
            500,
            r#"{"success":false,"errors":[{"code":1000,"message":"internal error"}],"messages":[],"result":null}"#,
        )
        .await;

        assert_eq!(cf_requests, vec!["DELETE /zones/zone-1/dns_records/rec-1"]);
        assert!(
            matches!(result, Err(ReconcileError::Other(_))),
            "expected cleanup to return Err so finalizer stays, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn cleanup_keeps_finalizer_when_cloudflare_returns_200_success_false() {
        let live = sample_record(Some(sample_status("zone-1", "rec-1")));
        let (result, cf_requests) = run_cleanup_test(
            live.clone(),
            200,
            serde_json::to_string(&live).unwrap(),
            200,
            r#"{"success":false,"errors":[{"code":1000,"message":"failed"}],"messages":[],"result":null}"#,
        )
        .await;

        assert_eq!(cf_requests, vec!["DELETE /zones/zone-1/dns_records/rec-1"]);
        assert!(matches!(result, Err(ReconcileError::Other(_))));
    }

    #[tokio::test]
    async fn cleanup_succeeds_when_cloudflare_record_is_already_gone() {
        let live = sample_record(Some(sample_status("zone-1", "rec-1")));
        let (result, cf_requests) = run_cleanup_test(
            live.clone(),
            200,
            serde_json::to_string(&live).unwrap(),
            404,
            r#"{"success":false,"errors":[{"code":81044,"message":"Record does not exist."}],"messages":[],"result":null}"#,
        )
        .await;

        assert_eq!(cf_requests, vec!["DELETE /zones/zone-1/dns_records/rec-1"]);
        assert!(result.is_ok(), "404 should let finalizer be removed: {result:?}");
    }

    #[tokio::test]
    async fn cleanup_skips_cloudflare_delete_when_live_status_has_no_record_id() {
        // Mirrors what update_conditions writes when apply fails before creating a Cloudflare record.
        let live = sample_record(Some(CloudflareDNSRecordStatus::default()));
        let (result, cf_requests) = run_cleanup_test(
            live.clone(),
            200,
            serde_json::to_string(&live).unwrap(),
            500,
            r#"{"success":false,"errors":[],"messages":[],"result":null}"#,
        )
        .await;

        assert!(cf_requests.is_empty(), "should not call Cloudflare: {cf_requests:?}");
        assert!(result.is_ok(), "cleanup failed: {result:?}");
    }

    #[tokio::test]
    async fn cleanup_keeps_finalizer_when_live_get_fails() {
        let live = sample_record(Some(sample_status("zone-1", "rec-1")));
        let err_body = json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "reason": "InternalError",
            "code": 500,
            "message": "apiserver unavailable"
        });
        let (result, cf_requests) = run_cleanup_test(
            live,
            500,
            err_body.to_string(),
            200,
            r#"{"success":true,"errors":[],"messages":[],"result":{"id":"rec-1"}}"#,
        )
        .await;

        assert!(cf_requests.is_empty(), "should not call Cloudflare: {cf_requests:?}");
        assert!(matches!(result, Err(ReconcileError::Kube(_))));
    }

    #[tokio::test]
    async fn cleanup_succeeds_when_record_is_already_deleted() {
        let cached = sample_record(Some(sample_status("zone-1", "rec-1")));
        let not_found = json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "reason": "NotFound",
            "code": 404,
            "message": "not found"
        });
        let (result, cf_requests) = run_cleanup_test(
            cached,
            404,
            not_found.to_string(),
            200,
            r#"{"success":true,"errors":[],"messages":[],"result":{"id":"rec-1"}}"#,
        )
        .await;

        assert!(cf_requests.is_empty(), "should not call Cloudflare: {cf_requests:?}");
        assert!(result.is_ok(), "already-deleted record should not error: {result:?}");
    }

    #[tokio::test]
    async fn cleanup_aborts_when_live_record_has_different_uid() {
        let cached = sample_record(None);
        let mut live = sample_record(Some(sample_status("zone-1", "new-rec")));
        live.metadata.uid = Some("uid-2".to_string());
        let (result, cf_requests) = run_cleanup_test(
            cached,
            200,
            serde_json::to_string(&live).unwrap(),
            200,
            r#"{"success":true,"errors":[],"messages":[],"result":{"id":"new-rec"}}"#,
        )
        .await;

        assert!(
            cf_requests.is_empty(),
            "must not delete replacement record: {cf_requests:?}"
        );
        assert!(matches!(result, Err(ReconcileError::Other(_))));
    }

    #[tokio::test]
    async fn cleanup_returns_rate_limit_error_on_cloudflare_429() {
        let live = sample_record(Some(sample_status("zone-1", "rec-1")));
        let (result, cf_requests) = run_cleanup_test(
            live.clone(),
            200,
            serde_json::to_string(&live).unwrap(),
            429,
            r#"{"success":false,"errors":[{"code":10000,"message":"rate limited"}],"messages":[],"result":null}"#,
        )
        .await;

        assert_eq!(cf_requests, vec!["DELETE /zones/zone-1/dns_records/rec-1"]);
        match result {
            Err(ReconcileError::Other(err)) => {
                assert!(cloudflare::is_cloudflare_rate_limit(&err), "expected 429: {err:?}");
            }
            other => panic!("expected rate-limit error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_conditions_preserves_live_record_ids_when_cache_lacks_status() {
        // regression: update_conditions built status from the cached copy, so a stale cache without status
        // overwrote live record_id/zone_id with "" and caused cleanup to skip the Cloudflare delete
        let (service, mut api_server) = tower_test::mock::pair::<Request<Body>, Response<Body>>();
        let ctx = Context {
            client: kube::Client::new(service, "default"),
            cloudflare_api: cloudflare::CloudflareApi::new(String::new()),
            do_dns_check: false,
            dns_check_tx: tokio::sync::mpsc::channel(1).0,
            dns_lookup_success: Default::default(),
            reconcile_lock: Default::default(),
        };

        let cached = sample_record(None);
        let live = sample_record(Some(sample_status("zone-1", "rec-1")));
        let live_json = serde_json::to_string(&live).unwrap();
        let cond = error_condition(&cached, "missing content", "service has no ip", None);

        let mock_api = async {
            let mut patched_status = None;
            while let Some((request, respond)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), api_server.next_request())
                    .await
                    .expect("api server timed out")
            {
                match (request.method(), request.uri().path()) {
                    (&Method::GET, "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/web") => {
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(live_json.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (
                        &Method::PATCH,
                        "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/web/status",
                    ) => {
                        let bytes = request.into_body().collect_bytes().await.unwrap();
                        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        patched_status = body.get("status").cloned();
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(live_json.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (method, path) => panic!("unexpected request: {method} {path}"),
                }
            }
            patched_status
        };

        let run_update = async move { update_conditions(&cached, &ctx, vec![cond]).await };
        let (result, patched_status) = tokio::join!(run_update, mock_api);

        assert!(result.is_ok(), "update_conditions failed: {result:?}");
        let status = patched_status.expect("expected status PATCH");
        assert_eq!(status["zone_id"], "zone-1");
        assert_eq!(status["record_id"], "rec-1");
    }

    #[tokio::test]
    async fn cleanup_skips_cloudflare_delete_when_another_record_shares_record_id() {
        // regression: two CloudflareDNSRecords sharing a Cloudflare name stored the same record_id, so deleting
        // either one deleted the Cloudflare record for both (pr-N-admin-dns / pr-N-admin, 2026-11-26)
        let cf = MockCloudflare::start(
            200,
            r#"{"success":true,"errors":[],"messages":[],"result":{"id":"rec-1"}}"#,
        );
        let (service, mut api_server) = tower_test::mock::pair::<Request<Body>, Response<Body>>();
        let ctx = Arc::new(Context {
            client: kube::Client::new(service, "default"),
            cloudflare_api: cloudflare::CloudflareApi::with_base_url("token".to_string(), &cf.base_url),
            do_dns_check: false,
            dns_check_tx: tokio::sync::mpsc::channel(1).0,
            dns_lookup_success: Default::default(),
            reconcile_lock: Default::default(),
        });

        let mut deleting = sample_record(Some(sample_status("zone-1", "rec-1")));
        deleting.metadata.name = Some("pr-1-admin-dns".to_string());
        deleting.metadata.uid = Some("uid-old".to_string());
        deleting.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(chrono::Utc::now()));

        let mut peer = sample_record(Some(sample_status("zone-1", "rec-1")));
        peer.metadata.name = Some("pr-1-admin".to_string());
        peer.metadata.uid = Some("uid-new".to_string());

        let deleting_json = serde_json::to_string(&deleting).unwrap();
        let list_json = json!({
            "apiVersion": "dns.cloudflare.com/v1alpha1",
            "kind": "CloudflareDNSRecordList",
            "metadata": {},
            "items": [&deleting, &peer]
        })
        .to_string();

        let mock_api = async {
            while let Some((request, respond)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), api_server.next_request())
                    .await
                    .expect("api server timed out")
            {
                let body = match (request.method(), request.uri().path()) {
                    (
                        &Method::GET,
                        "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/pr-1-admin-dns",
                    ) => deleting_json.clone(),
                    (&Method::GET, "/apis/dns.cloudflare.com/v1alpha1/cloudflarednsrecords") => list_json.clone(),
                    (method, path) => panic!("unexpected request: {method} {path}"),
                };
                respond.send_response(
                    Response::builder()
                        .status(200)
                        .body(Body::from(body.into_bytes()))
                        .unwrap(),
                );
            }
        };

        let run_cleanup = async move { cleanup(Arc::new(deleting), ctx).await };
        let (result, ()) = tokio::join!(run_cleanup, mock_api);

        let cf_requests = cf.finish();
        assert!(result.is_ok(), "cleanup failed: {result:?}");
        assert!(
            cf_requests.is_empty(),
            "cleanup must not delete Cloudflare record still referenced by a peer: {cf_requests:?}"
        );
    }

    fn named_record(
        name: &str,
        uid: &str,
        cf_name: &str,
        content: &str,
        status: Option<CloudflareDNSRecordStatus>,
    ) -> CloudflareDNSRecord {
        let spec = json!({
            "name": cf_name,
            "content": { "value": content },
            "zone": { "id": { "value": "zone-1" } }
        });
        let mut record = CloudflareDNSRecord::new(name, serde_json::from_value(spec).unwrap());
        record.metadata.namespace = Some("app".to_string());
        record.metadata.uid = Some(uid.to_string());
        record.status = status;
        record
    }

    fn cf_record_json(id: &str, name: &str, record_type: &str, content: &str) -> serde_json::Value {
        json!({
            "id": id,
            "name": name,
            "type": record_type,
            "content": content,
            "ttl": 300,
            "proxiable": true,
            "proxied": false,
            "created_on": "2026-01-01T00:00:00Z",
            "modified_on": "2026-01-01T00:00:00Z",
            "meta": {},
            "tags": []
        })
    }

    #[tokio::test]
    async fn cleanup_deletes_cloudflare_record_when_only_peer_is_also_deleting() {
        let cf = MockCloudflare::start(
            200,
            r#"{"success":true,"errors":[],"messages":[],"result":{"id":"rec-1"}}"#,
        );
        let (service, mut api_server) = tower_test::mock::pair::<Request<Body>, Response<Body>>();
        let ctx = Arc::new(Context {
            client: kube::Client::new(service, "default"),
            cloudflare_api: cloudflare::CloudflareApi::with_base_url("token".to_string(), &cf.base_url),
            do_dns_check: false,
            dns_check_tx: tokio::sync::mpsc::channel(1).0,
            dns_lookup_success: Default::default(),
            reconcile_lock: Default::default(),
        });

        let now = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(chrono::Utc::now());
        let mut deleting = named_record(
            "pr-1-admin-dns",
            "uid-old",
            "pr-1-admin.dev.hyper.video",
            "1.2.3.4",
            Some(sample_status("zone-1", "rec-1")),
        );
        deleting.metadata.deletion_timestamp = Some(now.clone());

        let mut peer = named_record(
            "pr-1-admin",
            "uid-new",
            "pr-1-admin.dev.hyper.video",
            "1.2.3.4",
            Some(sample_status("zone-1", "rec-1")),
        );
        peer.metadata.deletion_timestamp = Some(now);

        let deleting_json = serde_json::to_string(&deleting).unwrap();
        let list_json = json!({
            "apiVersion": "dns.cloudflare.com/v1alpha1",
            "kind": "CloudflareDNSRecordList",
            "metadata": {},
            "items": [&deleting, &peer]
        })
        .to_string();

        let mock_api = async {
            while let Some((request, respond)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), api_server.next_request())
                    .await
                    .expect("api server timed out")
            {
                let body = match (request.method(), request.uri().path()) {
                    (
                        &Method::GET,
                        "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/pr-1-admin-dns",
                    ) => deleting_json.clone(),
                    (&Method::GET, "/apis/dns.cloudflare.com/v1alpha1/cloudflarednsrecords") => list_json.clone(),
                    (method, path) => panic!("unexpected request: {method} {path}"),
                };
                respond.send_response(
                    Response::builder()
                        .status(200)
                        .body(Body::from(body.into_bytes()))
                        .unwrap(),
                );
            }
        };

        let run_cleanup = async move { cleanup(Arc::new(deleting), ctx).await };
        let (result, ()) = tokio::join!(run_cleanup, mock_api);

        let cf_requests = cf.finish();
        assert!(result.is_ok(), "cleanup failed: {result:?}");
        assert_eq!(cf_requests, vec!["DELETE /zones/zone-1/dns_records/rec-1"]);
    }

    #[tokio::test]
    async fn apply_refuses_conflicting_content_for_same_name_and_type() {
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-1", "app.example.com", "A", "1.2.3.4")]
        })
        .to_string();
        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => (200, list_cf.clone()),
            other => panic!("conflicting record must not send Cloudflare {other}"),
        });

        let owner = named_record(
            "owner",
            "uid-owner",
            "app.example.com",
            "1.2.3.4",
            Some(sample_status("zone-1", "rec-1")),
        );
        let intruder = named_record("intruder", "uid-intruder", "APP.EXAMPLE.COM.", "9.9.9.9", None);
        let (result, patched_status, cf_requests) = run_apply_test(intruder.clone(), vec![owner, intruder], cf).await;

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(cf_requests, vec!["GET /zones/zone-1/dns_records?page=1"]);
        let status = patched_status.expect("expected NameConflict status patch");
        assert_eq!(status["record_id"], "");
        assert_eq!(status["zone_id"], "");
        assert_eq!(status["conditions"][0]["status"], "False");
        assert_eq!(status["conditions"][0]["reason"], "NameConflict");
    }

    #[tokio::test]
    async fn apply_shares_record_when_peer_has_same_name_type_and_content() {
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-1", "pr-1-admin.dev.hyper.video", "A", "1.2.3.4")]
        })
        .to_string();
        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => (200, list_cf.clone()),
            other => panic!("unexpected Cloudflare request: {other}"),
        });

        let existing = named_record(
            "pr-1-admin-dns",
            "uid-old",
            "pr-1-admin.dev.hyper.video",
            "1.2.3.4",
            Some(sample_status("zone-1", "rec-1")),
        );
        let newcomer = named_record("pr-1-admin", "uid-new", "pr-1-admin.dev.hyper.video", "1.2.3.4", None);
        let (result, patched_status, cf_requests) =
            run_apply_test(newcomer.clone(), vec![existing, newcomer], cf).await;

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(cf_requests, vec!["GET /zones/zone-1/dns_records?page=1"]);
        let status = patched_status.expect("expected Ready status patch");
        assert_eq!(status["record_id"], "rec-1");
        assert_eq!(status["zone_id"], "zone-1");
        assert_eq!(status["conditions"][0]["status"], "True");
    }

    #[tokio::test]
    async fn apply_deletes_old_cloudflare_record_on_rename_when_unreferenced() {
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-old", "old.example.com", "A", "1.2.3.4")]
        })
        .to_string();
        let empty_cf = r#"{"success":true,"errors":[],"messages":[],"result":[]}"#.to_string();
        let create_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": cf_record_json("rec-new", "new.example.com", "A", "1.2.3.4")
        })
        .to_string();
        let delete_cf = r#"{"success":true,"errors":[],"messages":[],"result":{"id":"rec-old"}}"#.to_string();
        let deleted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let deleted_flag = deleted.clone();

        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => {
                if deleted_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    (200, empty_cf.clone())
                } else {
                    (200, list_cf.clone())
                }
            }
            "DELETE" => {
                deleted_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                (200, delete_cf.clone())
            }
            "POST" => (200, create_cf.clone()),
            other => panic!("unexpected Cloudflare request: {other}"),
        });

        let renamed = named_record(
            "web",
            "uid-1",
            "new.example.com",
            "1.2.3.4",
            Some(sample_status("zone-1", "rec-old")),
        );
        let (result, patched_status, cf_requests) = run_apply_test(renamed.clone(), vec![renamed], cf).await;

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(
            cf_requests,
            vec![
                "GET /zones/zone-1/dns_records?page=1",
                "DELETE /zones/zone-1/dns_records/rec-old",
                "GET /zones/zone-1/dns_records?page=1",
                "POST /zones/zone-1/dns_records",
            ]
        );
        let status = patched_status.expect("expected status patch");
        assert_eq!(status["record_id"], "rec-new");
    }

    #[tokio::test]
    async fn apply_keeps_old_cloudflare_record_on_rename_when_peer_still_references_it() {
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-old", "old.example.com", "A", "1.2.3.4")]
        })
        .to_string();
        let create_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": cf_record_json("rec-new", "new.example.com", "A", "1.2.3.4")
        })
        .to_string();

        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => (200, list_cf.clone()),
            "POST" => (200, create_cf.clone()),
            other => panic!("unexpected Cloudflare request: {other}"),
        });

        let renamed = named_record(
            "web",
            "uid-1",
            "new.example.com",
            "1.2.3.4",
            Some(sample_status("zone-1", "rec-old")),
        );
        let peer = named_record(
            "peer",
            "uid-2",
            "old.example.com",
            "1.2.3.4",
            Some(sample_status("zone-1", "rec-old")),
        );
        let (result, patched_status, cf_requests) = run_apply_test(renamed.clone(), vec![renamed, peer], cf).await;

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(
            cf_requests,
            vec!["GET /zones/zone-1/dns_records?page=1", "POST /zones/zone-1/dns_records"]
        );
        let status = patched_status.expect("expected status patch");
        assert_eq!(status["record_id"], "rec-new");
    }

    fn record_with_spec(
        name: &str,
        uid: &str,
        created_secs: i64,
        spec: serde_json::Value,
        status: Option<CloudflareDNSRecordStatus>,
    ) -> CloudflareDNSRecord {
        let mut record = CloudflareDNSRecord::new(name, serde_json::from_value(spec).unwrap());
        record.metadata.namespace = Some("app".to_string());
        record.metadata.uid = Some(uid.to_string());
        record.metadata.creation_timestamp = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
            chrono::DateTime::from_timestamp(created_secs, 0).unwrap(),
        ));
        record.status = status;
        record
    }

    async fn run_apply_test(
        target: CloudflareDNSRecord,
        all_items: Vec<CloudflareDNSRecord>,
        cf: MockCloudflare,
    ) -> (Result<(), ReconcileError>, Option<serde_json::Value>, Vec<String>) {
        let (service, mut api_server) = tower_test::mock::pair::<Request<Body>, Response<Body>>();
        let ctx = Arc::new(Context {
            client: kube::Client::new(service, "default"),
            cloudflare_api: cloudflare::CloudflareApi::with_base_url("token".to_string(), &cf.base_url),
            do_dns_check: false,
            dns_check_tx: tokio::sync::mpsc::channel(1).0,
            dns_lookup_success: Default::default(),
            reconcile_lock: Default::default(),
        });

        let target_name = target.metadata.name.clone().unwrap();
        let target_json = serde_json::to_string(&target).unwrap();
        let list_json = json!({
            "apiVersion": "dns.cloudflare.com/v1alpha1",
            "kind": "CloudflareDNSRecordList",
            "metadata": {},
            "items": all_items
        })
        .to_string();
        let get_path = format!("/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/{target_name}");
        let patch_path = format!("{get_path}/status");

        let mock_api = async {
            let mut patched_status = None;
            while let Some((request, respond)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), api_server.next_request())
                    .await
                    .expect("api server timed out")
            {
                let path = request.uri().path();
                if request.method() == Method::GET && path == get_path {
                    respond.send_response(
                        Response::builder()
                            .status(200)
                            .body(Body::from(target_json.clone().into_bytes()))
                            .unwrap(),
                    );
                } else if request.method() == Method::GET
                    && path == "/apis/dns.cloudflare.com/v1alpha1/cloudflarednsrecords"
                {
                    respond.send_response(
                        Response::builder()
                            .status(200)
                            .body(Body::from(list_json.clone().into_bytes()))
                            .unwrap(),
                    );
                } else if request.method() == Method::PATCH && path == patch_path {
                    let bytes = request.into_body().collect_bytes().await.unwrap();
                    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    patched_status = body.get("status").cloned();
                    respond.send_response(
                        Response::builder()
                            .status(200)
                            .body(Body::from(target_json.clone().into_bytes()))
                            .unwrap(),
                    );
                } else {
                    panic!("unexpected request: {} {}", request.method(), path);
                }
            }
            patched_status
        };

        let run_apply = async move { apply(Arc::new(target), ctx).await };
        let (result, patched_status) = tokio::join!(run_apply, mock_api);
        (result, patched_status, cf.finish())
    }

    #[tokio::test]
    async fn apply_refuses_older_record_renamed_onto_name_owned_by_newer_record() {
        // Older record O owned a.example.com (rec-a) and was renamed onto b.example.com owned by newer N (rec-b).
        // N must keep rec-b; O must get NameConflict, delete its old rec-a, and clear its status IDs.
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [
                cf_record_json("rec-a", "a.example.com", "A", "1.1.1.1"),
                cf_record_json("rec-b", "b.example.com", "A", "2.2.2.2"),
            ]
        })
        .to_string();
        let delete_cf = r#"{"success":true,"errors":[],"messages":[],"result":{"id":"rec-a"}}"#.to_string();
        let cf = MockCloudflare::with_handler(move |method, path| match (method, path) {
            ("GET", _) => (200, list_cf.clone()),
            ("DELETE", "/zones/zone-1/dns_records/rec-a") => (200, delete_cf.clone()),
            other => panic!("unexpected Cloudflare request: {other:?}"),
        });

        let older_renamed = record_with_spec(
            "old-rec",
            "uid-old",
            100,
            json!({ "name": "b.example.com", "content": { "value": "1.1.1.1" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-a")),
        );
        let newer_owner = record_with_spec(
            "new-rec",
            "uid-new",
            200,
            json!({ "name": "b.example.com", "content": { "value": "2.2.2.2" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-b")),
        );

        let (result, patched_status, cf_requests) =
            run_apply_test(older_renamed.clone(), vec![older_renamed, newer_owner], cf).await;

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(
            cf_requests,
            vec![
                "GET /zones/zone-1/dns_records?page=1",
                "DELETE /zones/zone-1/dns_records/rec-a",
            ]
        );
        let status = patched_status.expect("expected NameConflict status patch");
        assert_eq!(status["record_id"], "");
        assert_eq!(status["zone_id"], "");
        assert_eq!(status["conditions"][0]["reason"], "NameConflict");
    }

    #[tokio::test]
    async fn apply_refuses_older_shared_record_when_its_content_changes() {
        // Older A and newer B shared rec-1 (1.1.1.1). A's content changes to 9.9.9.9.
        // B still matches what Cloudflare serves, so B wins; A gets NameConflict, clears its status IDs, and does
        // NOT delete rec-1 because B still references it.
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-1", "shared.example.com", "A", "1.1.1.1")]
        })
        .to_string();
        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => (200, list_cf.clone()),
            other => panic!("must not modify Cloudflare: {other}"),
        });

        let older_edited = record_with_spec(
            "a",
            "uid-a",
            100,
            json!({ "name": "shared.example.com", "content": { "value": "9.9.9.9" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-1")),
        );
        let newer_unchanged = record_with_spec(
            "b",
            "uid-b",
            200,
            json!({ "name": "shared.example.com", "content": { "value": "1.1.1.1" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-1")),
        );

        let (result, patched_status, cf_requests) =
            run_apply_test(older_edited.clone(), vec![older_edited, newer_unchanged], cf).await;

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(cf_requests, vec!["GET /zones/zone-1/dns_records?page=1"]);
        let status = patched_status.expect("expected NameConflict status patch");
        assert_eq!(status["record_id"], "");
        assert_eq!(status["zone_id"], "");
        assert_eq!(status["conditions"][0]["reason"], "NameConflict");
    }

    #[tokio::test]
    async fn apply_keeps_owner_rank_when_owner_peer_content_lookup_errors() {
        // Peer A owns rec-1 via Service `web`. B claims the same name with literal 9.9.9.9.
        // If GET Service `web` fails transiently (500), A keeps its rank from its live Cloudflare record so B
        // gets NameConflict instead of replacing rec-1.
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-1", "app.example.com", "A", "1.2.3.4")]
        })
        .to_string();
        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => (200, list_cf.clone()),
            other => panic!("must not write to Cloudflare on peer error: {other}"),
        });

        let (service, mut api_server) = tower_test::mock::pair::<Request<Body>, Response<Body>>();
        let ctx = Arc::new(Context {
            client: kube::Client::new(service, "default"),
            cloudflare_api: cloudflare::CloudflareApi::with_base_url("token".to_string(), &cf.base_url),
            do_dns_check: false,
            dns_check_tx: tokio::sync::mpsc::channel(1).0,
            dns_lookup_success: Default::default(),
            reconcile_lock: Default::default(),
        });

        let owner = record_with_spec(
            "owner",
            "uid-owner",
            100,
            json!({ "name": "app.example.com", "content": { "service": { "name": "web" } }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-1")),
        );
        let intruder = record_with_spec(
            "intruder",
            "uid-intruder",
            200,
            json!({ "name": "app.example.com", "content": { "value": "9.9.9.9" }, "zone": { "id": { "value": "zone-1" } } }),
            None,
        );
        let intruder_json = serde_json::to_string(&intruder).unwrap();
        let list_json = json!({
            "apiVersion": "dns.cloudflare.com/v1alpha1",
            "kind": "CloudflareDNSRecordList",
            "metadata": {},
            "items": [&owner, &intruder]
        })
        .to_string();
        let svc_err = json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "reason": "InternalError",
            "code": 500,
            "message": "apiserver hiccup"
        })
        .to_string();

        let mock_api = async {
            let mut patched_status = None;
            while let Some((request, respond)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), api_server.next_request())
                    .await
                    .expect("api server timed out")
            {
                match (request.method(), request.uri().path()) {
                    (
                        &Method::GET,
                        "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/intruder",
                    ) => {
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(intruder_json.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (&Method::GET, "/apis/dns.cloudflare.com/v1alpha1/cloudflarednsrecords") => {
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(list_json.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (&Method::GET, "/api/v1/namespaces/app/services/web") => {
                        respond.send_response(
                            Response::builder()
                                .status(500)
                                .body(Body::from(svc_err.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (
                        &Method::PATCH,
                        "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/intruder/status",
                    ) => {
                        let bytes = request.into_body().collect_bytes().await.unwrap();
                        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        patched_status = body.get("status").cloned();
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(intruder_json.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (method, path) => panic!("unexpected request: {method} {path}"),
                }
            }
            patched_status
        };

        let run_apply = async move { apply(Arc::new(intruder), ctx).await };
        let (result, patched_status) = tokio::join!(run_apply, mock_api);

        let cf_requests = cf.finish();
        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(cf_requests, vec!["GET /zones/zone-1/dns_records?page=1"]);
        let status = patched_status.expect("expected NameConflict status patch");
        assert_eq!(status["conditions"][0]["reason"], "NameConflict");
    }

    #[tokio::test]
    async fn apply_allows_matching_third_claimant_despite_older_refused_claimant() {
        // A owns rec-1 (1.2.3.4). B (older than C) wants 9.9.9.9 (refused). C (newer than B) wants 1.2.3.4.
        // C matches the winner (A) and must be allowed to share rec-1.
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-1", "app.example.com", "A", "1.2.3.4")]
        })
        .to_string();
        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => (200, list_cf.clone()),
            other => panic!("unexpected Cloudflare request: {other}"),
        });

        let owner_a = record_with_spec(
            "a",
            "uid-a",
            100,
            json!({ "name": "app.example.com", "content": { "value": "1.2.3.4" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-1")),
        );
        let loser_b = record_with_spec(
            "b",
            "uid-b",
            200,
            json!({ "name": "app.example.com", "content": { "value": "9.9.9.9" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(CloudflareDNSRecordStatus::default()),
        );
        let matcher_c = record_with_spec(
            "c",
            "uid-c",
            300,
            json!({ "name": "app.example.com", "content": { "value": "1.2.3.4" }, "zone": { "id": { "value": "zone-1" } } }),
            None,
        );

        let (result, patched_status, cf_requests) =
            run_apply_test(matcher_c.clone(), vec![owner_a, loser_b, matcher_c], cf).await;

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(cf_requests, vec!["GET /zones/zone-1/dns_records?page=1"]);
        let status = patched_status.expect("expected Ready status patch");
        assert_eq!(status["record_id"], "rec-1");
        assert_eq!(status["conditions"][0]["status"], "True");
    }

    #[tokio::test]
    async fn apply_refuses_cname_conflicting_with_existing_a_record() {
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-a", "app.example.com", "A", "1.2.3.4")]
        })
        .to_string();
        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => (200, list_cf.clone()),
            other => panic!("must not write conflicting CNAME: {other}"),
        });

        let a_owner = record_with_spec(
            "a-rec",
            "uid-a",
            100,
            json!({ "name": "app.example.com", "type": "A", "content": { "value": "1.2.3.4" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-a")),
        );
        let cname_new = record_with_spec(
            "cname-rec",
            "uid-c",
            200,
            json!({ "name": "app.example.com", "type": "CNAME", "content": { "value": "target.example.com" }, "zone": { "id": { "value": "zone-1" } } }),
            None,
        );

        let (result, patched_status, cf_requests) =
            run_apply_test(cname_new.clone(), vec![a_owner, cname_new], cf).await;

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(cf_requests, vec!["GET /zones/zone-1/dns_records?page=1"]);
        let status = patched_status.expect("expected NameConflict status patch");
        assert_eq!(status["conditions"][0]["reason"], "NameConflict");
    }

    #[tokio::test]
    async fn apply_deletes_old_a_record_before_creating_cname_on_type_change() {
        // regression: filtering update_dns_record_and_wait by type without delete-before-create made A -> CNAME
        // edits fail forever because Cloudflare rejects creating CNAME while A exists at the same name
        let deleted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let deleted_flag = deleted.clone();
        let list_before = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-a", "app.example.com", "A", "1.2.3.4")]
        })
        .to_string();
        let list_after = r#"{"success":true,"errors":[],"messages":[],"result":[]}"#.to_string();
        let delete_cf = r#"{"success":true,"errors":[],"messages":[],"result":{"id":"rec-a"}}"#.to_string();
        let create_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": cf_record_json("rec-cname", "app.example.com", "CNAME", "target.example.com")
        })
        .to_string();

        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => {
                if deleted_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    (200, list_after.clone())
                } else {
                    (200, list_before.clone())
                }
            }
            "DELETE" => {
                deleted_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                (200, delete_cf.clone())
            }
            "POST" => {
                assert!(
                    deleted_flag.load(std::sync::atomic::Ordering::Relaxed),
                    "old A record must be deleted before creating CNAME"
                );
                (200, create_cf.clone())
            }
            other => panic!("unexpected Cloudflare request: {other}"),
        });

        let changed = record_with_spec(
            "web",
            "uid-1",
            100,
            json!({ "name": "app.example.com", "type": "CNAME", "content": { "value": "target.example.com" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-a")),
        );

        let (result, patched_status, cf_requests) = run_apply_test(changed.clone(), vec![changed], cf).await;

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(
            cf_requests,
            vec![
                "GET /zones/zone-1/dns_records?page=1",
                "DELETE /zones/zone-1/dns_records/rec-a",
                "GET /zones/zone-1/dns_records?page=1",
                "POST /zones/zone-1/dns_records",
            ]
        );
        let status = patched_status.expect("expected status patch");
        assert_eq!(status["record_id"], "rec-cname");
    }

    #[tokio::test]
    async fn apply_does_not_create_new_record_when_rename_delete_fails() {
        // Delete-before-create ensures a transient delete error does not create an unrecorded new Cloudflare record.
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-old", "old.example.com", "A", "1.2.3.4")]
        })
        .to_string();
        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => (200, list_cf.clone()),
            "DELETE" => (
                500,
                r#"{"success":false,"errors":[{"code":1000,"message":"boom"}],"messages":[],"result":null}"#
                    .to_string(),
            ),
            other => panic!("must not create new record when delete fails: {other}"),
        });

        let renamed = record_with_spec(
            "web",
            "uid-1",
            100,
            json!({ "name": "new.example.com", "content": { "value": "1.2.3.4" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-old")),
        );

        let (result, patched_status, cf_requests) = run_apply_test(renamed.clone(), vec![renamed], cf).await;

        assert!(result.is_err(), "expected delete failure to return Err");
        assert!(patched_status.is_none(), "status must stay on old record_id");
        assert_eq!(
            cf_requests,
            vec![
                "GET /zones/zone-1/dns_records?page=1",
                "DELETE /zones/zone-1/dns_records/rec-old",
            ]
        );
    }

    #[tokio::test]
    async fn apply_deletes_old_cloudflare_record_on_zone_change() {
        let empty_cf = r#"{"success":true,"errors":[],"messages":[],"result":[]}"#.to_string();
        let delete_cf = r#"{"success":true,"errors":[],"messages":[],"result":{"id":"rec-old"}}"#.to_string();
        let create_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": cf_record_json("rec-new", "app.example.com", "A", "1.2.3.4")
        })
        .to_string();
        let cf = MockCloudflare::with_handler(move |method, path| match (method, path) {
            ("GET", "/zones/zone-2/dns_records?page=1") => (200, empty_cf.clone()),
            ("DELETE", "/zones/zone-1/dns_records/rec-old") => (200, delete_cf.clone()),
            ("POST", "/zones/zone-2/dns_records") => (200, create_cf.clone()),
            other => panic!("unexpected Cloudflare request: {other:?}"),
        });

        let moved_zone = record_with_spec(
            "web",
            "uid-1",
            100,
            json!({ "name": "app.example.com", "content": { "value": "1.2.3.4" }, "zone": { "id": { "value": "zone-2" } } }),
            Some(sample_status("zone-1", "rec-old")),
        );

        let (result, patched_status, cf_requests) = run_apply_test(moved_zone.clone(), vec![moved_zone], cf).await;

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(
            cf_requests,
            vec![
                "GET /zones/zone-2/dns_records?page=1",
                "DELETE /zones/zone-1/dns_records/rec-old",
                "POST /zones/zone-2/dns_records",
            ]
        );
        let status = patched_status.expect("expected status patch");
        assert_eq!(status["zone_id"], "zone-2");
        assert_eq!(status["record_id"], "rec-new");
    }

    #[tokio::test]
    async fn apply_ignores_unresolvable_non_owner_peer_when_owner_updates_ip() {
        // A owns rec-1 (1.2.3.4) and updates to 5.6.7.8. Leftover peer P owns nothing and points to a deleted
        // Service (404). P must be skipped so A can update Cloudflare cleanly.
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-1", "app.example.com", "A", "1.2.3.4")]
        })
        .to_string();
        let delete_cf = r#"{"success":true,"errors":[],"messages":[],"result":{"id":"rec-1"}}"#.to_string();
        let create_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": cf_record_json("rec-2", "app.example.com", "A", "5.6.7.8")
        })
        .to_string();
        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => (200, list_cf.clone()),
            "DELETE" => (200, delete_cf.clone()),
            "POST" => (200, create_cf.clone()),
            other => panic!("unexpected Cloudflare request: {other}"),
        });

        let (service, mut api_server) = tower_test::mock::pair::<Request<Body>, Response<Body>>();
        let ctx = Arc::new(Context {
            client: kube::Client::new(service, "default"),
            cloudflare_api: cloudflare::CloudflareApi::with_base_url("token".to_string(), &cf.base_url),
            do_dns_check: false,
            dns_check_tx: tokio::sync::mpsc::channel(1).0,
            dns_lookup_success: Default::default(),
            reconcile_lock: Default::default(),
        });

        let owner = record_with_spec(
            "owner",
            "uid-owner",
            100,
            json!({ "name": "app.example.com", "content": { "value": "5.6.7.8" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-1")),
        );
        let broken_peer = record_with_spec(
            "leftover",
            "uid-peer",
            50,
            json!({ "name": "app.example.com", "content": { "service": { "name": "deleted-svc" } }, "zone": { "id": { "value": "zone-1" } } }),
            Some(CloudflareDNSRecordStatus::default()),
        );
        let owner_json = serde_json::to_string(&owner).unwrap();
        let list_json = json!({
            "apiVersion": "dns.cloudflare.com/v1alpha1",
            "kind": "CloudflareDNSRecordList",
            "metadata": {},
            "items": [&owner, &broken_peer]
        })
        .to_string();
        let not_found = json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "reason": "NotFound",
            "code": 404,
            "message": "service not found"
        })
        .to_string();

        let mock_api = async {
            let mut patched_status = None;
            while let Some((request, respond)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), api_server.next_request())
                    .await
                    .expect("api server timed out")
            {
                match (request.method(), request.uri().path()) {
                    (&Method::GET, "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/owner") => {
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(owner_json.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (&Method::GET, "/apis/dns.cloudflare.com/v1alpha1/cloudflarednsrecords") => {
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(list_json.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (&Method::GET, "/api/v1/namespaces/app/services/deleted-svc") => {
                        respond.send_response(
                            Response::builder()
                                .status(404)
                                .body(Body::from(not_found.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (
                        &Method::PATCH,
                        "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/owner/status",
                    ) => {
                        let bytes = request.into_body().collect_bytes().await.unwrap();
                        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        patched_status = body.get("status").cloned();
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(owner_json.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (method, path) => panic!("unexpected request: {method} {path}"),
                }
            }
            patched_status
        };

        let run_apply = async move { apply(Arc::new(owner), ctx).await };
        let (result, patched_status) = tokio::join!(run_apply, mock_api);

        let cf_requests = cf.finish();
        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(
            cf_requests,
            vec![
                "GET /zones/zone-1/dns_records?page=1",
                "DELETE /zones/zone-1/dns_records/rec-1",
                "POST /zones/zone-1/dns_records",
            ]
        );
        let status = patched_status.expect("expected Ready status patch");
        assert_eq!(status["record_id"], "rec-2");
    }

    #[tokio::test]
    async fn apply_keeps_owner_rank_when_owner_peer_service_has_no_ip() {
        // Owner peer's Service returns Ok(None) (ClusterIP). Owner still keeps rank via its live Cloudflare record.
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-1", "app.example.com", "A", "1.2.3.4")]
        })
        .to_string();
        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => (200, list_cf.clone()),
            other => panic!("must not modify Cloudflare: {other}"),
        });

        let (service, mut api_server) = tower_test::mock::pair::<Request<Body>, Response<Body>>();
        let ctx = Arc::new(Context {
            client: kube::Client::new(service, "default"),
            cloudflare_api: cloudflare::CloudflareApi::with_base_url("token".to_string(), &cf.base_url),
            do_dns_check: false,
            dns_check_tx: tokio::sync::mpsc::channel(1).0,
            dns_lookup_success: Default::default(),
            reconcile_lock: Default::default(),
        });

        let owner = record_with_spec(
            "owner",
            "uid-owner",
            200,
            json!({ "name": "app.example.com", "content": { "service": { "name": "web" } }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-1")),
        );
        let challenger = record_with_spec(
            "challenger",
            "uid-challenger",
            100,
            json!({ "name": "app.example.com", "content": { "value": "9.9.9.9" }, "zone": { "id": { "value": "zone-1" } } }),
            None,
        );
        let challenger_json = serde_json::to_string(&challenger).unwrap();
        let list_json = json!({
            "apiVersion": "dns.cloudflare.com/v1alpha1",
            "kind": "CloudflareDNSRecordList",
            "metadata": {},
            "items": [&owner, &challenger]
        })
        .to_string();
        let svc_json = json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": { "name": "web", "namespace": "app" },
            "spec": { "type": "ClusterIP" }
        })
        .to_string();

        let mock_api = async {
            let mut patched_status = None;
            while let Some((request, respond)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), api_server.next_request())
                    .await
                    .expect("api server timed out")
            {
                match (request.method(), request.uri().path()) {
                    (
                        &Method::GET,
                        "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/challenger",
                    ) => {
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(challenger_json.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (&Method::GET, "/apis/dns.cloudflare.com/v1alpha1/cloudflarednsrecords") => {
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(list_json.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (&Method::GET, "/api/v1/namespaces/app/services/web") => {
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(svc_json.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (
                        &Method::PATCH,
                        "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/challenger/status",
                    ) => {
                        let bytes = request.into_body().collect_bytes().await.unwrap();
                        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        patched_status = body.get("status").cloned();
                        respond.send_response(
                            Response::builder()
                                .status(200)
                                .body(Body::from(challenger_json.clone().into_bytes()))
                                .unwrap(),
                        );
                    }
                    (method, path) => panic!("unexpected request: {method} {path}"),
                }
            }
            patched_status
        };

        let run_apply = async move { apply(Arc::new(challenger), ctx).await };
        let (result, patched_status) = tokio::join!(run_apply, mock_api);

        let cf_requests = cf.finish();
        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(cf_requests, vec!["GET /zones/zone-1/dns_records?page=1"]);
        let status = patched_status.expect("expected NameConflict status patch");
        assert_eq!(status["conditions"][0]["reason"], "NameConflict");
    }

    #[tokio::test]
    async fn apply_prefers_owner_over_older_refused_peer_when_owner_content_changes() {
        // Tests the second rank key (owns_this_cf_record): newer owner updates content from 1.2.3.4 to 5.6.7.8
        // while an older refused peer wants 9.9.9.9. Neither matches 1.2.3.4, so the owner beats the older peer.
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-1", "app.example.com", "A", "1.2.3.4")]
        })
        .to_string();
        let delete_cf = r#"{"success":true,"errors":[],"messages":[],"result":{"id":"rec-1"}}"#.to_string();
        let create_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": cf_record_json("rec-2", "app.example.com", "A", "5.6.7.8")
        })
        .to_string();
        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => (200, list_cf.clone()),
            "DELETE" => (200, delete_cf.clone()),
            "POST" => (200, create_cf.clone()),
            other => panic!("unexpected Cloudflare request: {other}"),
        });

        let older_refused = record_with_spec(
            "older",
            "uid-older",
            100,
            json!({ "name": "app.example.com", "content": { "value": "9.9.9.9" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(CloudflareDNSRecordStatus::default()),
        );
        let newer_owner = record_with_spec(
            "owner",
            "uid-owner",
            200,
            json!({ "name": "app.example.com", "content": { "value": "5.6.7.8" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-1")),
        );

        let (result, patched_status, cf_requests) =
            run_apply_test(newer_owner.clone(), vec![older_refused, newer_owner], cf).await;

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(
            cf_requests,
            vec![
                "GET /zones/zone-1/dns_records?page=1",
                "DELETE /zones/zone-1/dns_records/rec-1",
                "POST /zones/zone-1/dns_records",
            ]
        );
        let status = patched_status.expect("expected Ready status patch");
        assert_eq!(status["record_id"], "rec-2");
    }

    #[tokio::test]
    async fn apply_converges_when_two_shared_a_records_both_change_to_cname() {
        // A and B shared A record rec-a, and both specs moved to the same CNAME target.
        // Because B's spec no longer targets type A, A deletes rec-a and creates the CNAME cleanly.
        let deleted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let deleted_flag = deleted.clone();
        let list_before = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-a", "app.example.com", "A", "1.2.3.4")]
        })
        .to_string();
        let list_after = r#"{"success":true,"errors":[],"messages":[],"result":[]}"#.to_string();
        let delete_cf = r#"{"success":true,"errors":[],"messages":[],"result":{"id":"rec-a"}}"#.to_string();
        let create_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": cf_record_json("rec-cname", "app.example.com", "CNAME", "target.example.com")
        })
        .to_string();

        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => {
                if deleted_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    (200, list_after.clone())
                } else {
                    (200, list_before.clone())
                }
            }
            "DELETE" => {
                deleted_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                (200, delete_cf.clone())
            }
            "POST" => {
                assert!(deleted_flag.load(std::sync::atomic::Ordering::Relaxed));
                (200, create_cf.clone())
            }
            other => panic!("unexpected Cloudflare request: {other}"),
        });

        let a = record_with_spec(
            "a",
            "uid-a",
            100,
            json!({ "name": "app.example.com", "type": "CNAME", "content": { "value": "target.example.com" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-a")),
        );
        let b = record_with_spec(
            "b",
            "uid-b",
            200,
            json!({ "name": "app.example.com", "type": "CNAME", "content": { "value": "target.example.com" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-a")),
        );

        let (result, patched_status, cf_requests) = run_apply_test(a.clone(), vec![a, b], cf).await;

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(
            cf_requests,
            vec![
                "GET /zones/zone-1/dns_records?page=1",
                "DELETE /zones/zone-1/dns_records/rec-a",
                "GET /zones/zone-1/dns_records?page=1",
                "POST /zones/zone-1/dns_records",
            ]
        );
        let status = patched_status.expect("expected Ready status patch");
        assert_eq!(status["record_id"], "rec-cname");
    }

    #[tokio::test]
    async fn apply_allows_aaaa_alongside_a_despite_older_refused_cname() {
        // A owns A record; B (older than C) wants CNAME (refused by A); C wants AAAA.
        // Because acceptance runs across all types in rank order, B is refused by A and cannot block C.
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-a", "app.example.com", "A", "1.2.3.4")]
        })
        .to_string();
        let create_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": cf_record_json("rec-aaaa", "app.example.com", "AAAA", "2001:db8::1")
        })
        .to_string();
        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => (200, list_cf.clone()),
            "POST" => (200, create_cf.clone()),
            other => panic!("unexpected Cloudflare request: {other}"),
        });

        let a_owner = record_with_spec(
            "a-rec",
            "uid-a",
            100,
            json!({ "name": "app.example.com", "type": "A", "content": { "value": "1.2.3.4" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-a")),
        );
        let cname_loser = record_with_spec(
            "cname-rec",
            "uid-b",
            200,
            json!({ "name": "app.example.com", "type": "CNAME", "content": { "value": "target.example.com" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(CloudflareDNSRecordStatus::default()),
        );
        let aaaa_new = record_with_spec(
            "aaaa-rec",
            "uid-c",
            300,
            json!({ "name": "app.example.com", "type": "AAAA", "content": { "value": "2001:db8::1" }, "zone": { "id": { "value": "zone-1" } } }),
            None,
        );

        let (result, patched_status, cf_requests) =
            run_apply_test(aaaa_new.clone(), vec![a_owner, cname_loser, aaaa_new], cf).await;

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(
            cf_requests,
            vec!["GET /zones/zone-1/dns_records?page=1", "POST /zones/zone-1/dns_records"]
        );
        let status = patched_status.expect("expected Ready status patch");
        assert_eq!(status["record_id"], "rec-aaaa");
        assert_eq!(status["conditions"][0]["status"], "True");
    }

    #[tokio::test]
    async fn apply_keeps_old_record_when_winner_matches_it_before_adopting() {
        // A owns rec-1 (1.1.1.1). Newer B also wants 1.1.1.1 but has not reconciled yet (status: None).
        // A's content changes to 9.9.9.9. B wins because it matches Cloudflare; A must NOT delete rec-1
        // and must keep its status reference to rec-1 until B adopts it.
        let list_cf = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": [cf_record_json("rec-1", "app.example.com", "A", "1.1.1.1")]
        })
        .to_string();
        let cf = MockCloudflare::with_handler(move |method, _path| match method {
            "GET" => (200, list_cf.clone()),
            other => panic!("must not delete record winner relies on: {other}"),
        });

        let a_edited = record_with_spec(
            "a",
            "uid-a",
            100,
            json!({ "name": "app.example.com", "content": { "value": "9.9.9.9" }, "zone": { "id": { "value": "zone-1" } } }),
            Some(sample_status("zone-1", "rec-1")),
        );
        let b_unadopted = record_with_spec(
            "b",
            "uid-b",
            200,
            json!({ "name": "app.example.com", "content": { "value": "1.1.1.1" }, "zone": { "id": { "value": "zone-1" } } }),
            None,
        );

        let (result, patched_status, cf_requests) =
            run_apply_test(a_edited.clone(), vec![a_edited, b_unadopted], cf).await;

        assert!(result.is_ok(), "apply failed: {result:?}");
        assert_eq!(cf_requests, vec!["GET /zones/zone-1/dns_records?page=1"]);
        let status = patched_status.expect("expected NameConflict status patch");
        assert_eq!(status["zone_id"], "zone-1");
        assert_eq!(status["record_id"], "rec-1");
        assert_eq!(status["conditions"][0]["reason"], "NameConflict");
    }

    #[tokio::test]
    async fn cleanup_deletes_record_when_only_peer_is_refused_with_empty_ids() {
        let cf = MockCloudflare::start(
            200,
            r#"{"success":true,"errors":[],"messages":[],"result":{"id":"rec-1"}}"#,
        );
        let (service, mut api_server) = tower_test::mock::pair::<Request<Body>, Response<Body>>();
        let ctx = Arc::new(Context {
            client: kube::Client::new(service, "default"),
            cloudflare_api: cloudflare::CloudflareApi::with_base_url("token".to_string(), &cf.base_url),
            do_dns_check: false,
            dns_check_tx: tokio::sync::mpsc::channel(1).0,
            dns_lookup_success: Default::default(),
            reconcile_lock: Default::default(),
        });

        let mut deleting = named_record(
            "owner",
            "uid-owner",
            "app.example.com",
            "1.2.3.4",
            Some(sample_status("zone-1", "rec-1")),
        );
        deleting.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(chrono::Utc::now()));
        let refused_peer = named_record(
            "refused",
            "uid-refused",
            "app.example.com",
            "9.9.9.9",
            Some(CloudflareDNSRecordStatus::default()),
        );

        let deleting_json = serde_json::to_string(&deleting).unwrap();
        let list_json = json!({
            "apiVersion": "dns.cloudflare.com/v1alpha1",
            "kind": "CloudflareDNSRecordList",
            "metadata": {},
            "items": [&deleting, &refused_peer]
        })
        .to_string();

        let mock_api = async {
            while let Some((request, respond)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), api_server.next_request())
                    .await
                    .expect("api server timed out")
            {
                let body = match (request.method(), request.uri().path()) {
                    (&Method::GET, "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/owner") => {
                        deleting_json.clone()
                    }
                    (&Method::GET, "/apis/dns.cloudflare.com/v1alpha1/cloudflarednsrecords") => list_json.clone(),
                    (method, path) => panic!("unexpected request: {method} {path}"),
                };
                respond.send_response(
                    Response::builder()
                        .status(200)
                        .body(Body::from(body.into_bytes()))
                        .unwrap(),
                );
            }
        };

        let run_cleanup = async move { cleanup(Arc::new(deleting), ctx).await };
        let (result, ()) = tokio::join!(run_cleanup, mock_api);

        let cf_requests = cf.finish();
        assert!(result.is_ok(), "cleanup failed: {result:?}");
        assert_eq!(cf_requests, vec!["DELETE /zones/zone-1/dns_records/rec-1"]);
    }
}
