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
        CloudflareDNSRecord,
        CloudflareDNSRecordStatus,
        ZoneNameOrId,
    },
};
use eyre::OptionExt as _;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::{
    api::{
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
    let client = &ctx.client;
    let ns = resource.metadata.namespace.as_deref().unwrap_or("default");
    let name = resource.metadata.name.as_deref().ok_or_eyre("missing name")?;
    let is_new = resource.status.is_none();
    let gen = resource.metadata.generation;

    info!("processing reconcile request");

    let domain_or_record_text = resource.spec.name.as_str();

    let Some(content) = resource.spec.lookup_content(client, ns).await? else {
        let msg = format!("unable to resolve content for CloudflareDNSRecord {ns}/{name}");
        error!("{msg}");
        update_conditions(
            &resource,
            &ctx,
            vec![error_condition(&resource, "missing content", msg, gen)],
        )
        .await?;
        return Ok(());
    };

    let zone = match &resource.spec.zone {
        ZoneNameOrId::Name(it) => it.lookup(client, ns).await?.map(Zone::name),
        ZoneNameOrId::Id(it) => it.lookup(client, ns).await?.map(Zone::id),
    };

    let Some(zone) = zone else {
        let msg = format!(
            "unable to resolve {:?} for CloudflareDNSRecord {ns}/{name}",
            resource.spec.zone
        );
        error!("{msg}");
        update_conditions(
            &resource,
            &ctx,
            vec![error_condition(&resource, "missing zone", msg, gen)],
        )
        .await?;
        return Ok(());
    };

    let Some(zone) = zone.resolve(&ctx.cloudflare_api).await? else {
        let msg = format!("unable to resolve zone for CloudflareDNSRecord {ns}/{name}");
        error!("{msg}");
        update_conditions(
            &resource,
            &ctx,
            vec![error_condition(&resource, "missing zone", msg, gen)],
        )
        .await?;
        return Ok(());
    };
    let Zone::Identifier(zone_id) = zone.clone() else {
        unreachable!();
    };

    debug!("updating dns record for CloudflareDNSRecord {ns}/{name}");

    let record = ctx
        .cloudflare_api
        .update_dns_record_and_wait(cloudflare::CreateRecordArgs {
            zone,
            name: domain_or_record_text.to_string(),
            record_type: resource.spec.ty.unwrap_or_default(),
            content,
            comment: resource.spec.comment.clone(),
            ttl: resource.spec.ttl,
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
        success_condition(&resource, gen)
    } else {
        let msg = "The DNS record has not propagated yet. This is expected to take some time.".to_string();
        error_condition(&resource, "pending", msg, gen)
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

    if is_new && ctx.do_dns_check {
        let _ = ctx
            .dns_check_tx
            .send(DnsCheckRequest::CheckSingleRecord {
                name: name.to_string(),
                namespace: ns.to_string(),
            })
            .await;
    }

    write_status(&resource, &ctx, status).await
}

/// This functions runs before the resource is deleted. It'll try to delete the DNS record from Cloudflare.
#[instrument(level = "debug", skip_all)]
pub async fn cleanup(resource: Arc<CloudflareDNSRecord>, ctx: Arc<Context>) -> Result<(), ReconcileError> {
    let ns = resource.metadata.namespace.as_deref().unwrap_or("default");
    let name = resource.metadata.name.as_deref().ok_or_eyre("missing name")?;

    info!("delete request: CloudflareDNSRecord {ns}/{name}");

    let Some(status) = resource.status.as_ref() else {
        error!("missing status for CloudflareDNSRecord {ns}/{name}");
        return Ok(());
    };

    if let Err(err) = ctx
        .cloudflare_api
        .delete_dns_record(&status.zone_id, &status.record_id)
        .await
    {
        error!("Unable to delete dns record for cloudflare: {err}");
    }

    Ok(())
}

pub async fn update_conditions(
    resource: &CloudflareDNSRecord,
    ctx: &Context,
    conditions: Vec<Condition>,
) -> Result<(), ReconcileError> {
    let status = CloudflareDNSRecordStatus {
        conditions: Some(conditions),
        ..resource.status.clone().unwrap_or_default()
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
}
