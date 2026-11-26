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

/// Runs before the resource is deleted. Deletes the DNS record from Cloudflare, or returns an error so
/// `kube::runtime::finalizer` keeps the finalizer and retries.
#[instrument(level = "debug", skip_all)]
pub async fn cleanup(resource: Arc<CloudflareDNSRecord>, ctx: Arc<Context>) -> Result<(), ReconcileError> {
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

    ctx.cloudflare_api
        .delete_dns_record(&status.zone_id, &status.record_id)
        .await?;

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

    struct MockCloudflare {
        base_url: String,
        requests: Arc<std::sync::Mutex<Vec<String>>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl MockCloudflare {
        fn start(status: u16, body: &'static str) -> Self {
            use std::{
                io::{
                    Read as _,
                    Write as _,
                },
                sync::atomic::Ordering,
                time::Duration,
            };

            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();

            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let reqs = requests.clone();
            let stop_flag = stop.clone();

            let handle = std::thread::spawn(move || {
                while !stop_flag.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                            let mut buf = [0u8; 4096];
                            let n = stream.read(&mut buf).unwrap_or(0);
                            let req_text = String::from_utf8_lossy(&buf[..n]);
                            if let Some(line) = req_text.lines().next() {
                                let mut parts = line.split_whitespace();
                                if let (Some(method), Some(path)) = (parts.next(), parts.next()) {
                                    reqs.lock().unwrap().push(format!("{method} {path}"));
                                }
                            }
                            let resp = format!(
                                "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = stream.write_all(resp.as_bytes());
                            let _ = stream.flush();
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });

            Self {
                base_url,
                requests,
                stop,
                handle: Some(handle),
            }
        }

        fn finish(mut self) -> Vec<String> {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                handle.join().unwrap();
            }
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for MockCloudflare {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
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
        });

        let mock_api = async {
            while let Some((request, respond)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), api_server.next_request())
                    .await
                    .expect("api server timed out")
            {
                assert_eq!(request.method(), Method::GET);
                assert_eq!(
                    request.uri().path(),
                    "/apis/dns.cloudflare.com/v1alpha1/namespaces/app/cloudflarednsrecords/web"
                );
                respond.send_response(
                    Response::builder()
                        .status(get_status)
                        .body(Body::from(get_body.clone().into_bytes()))
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
}
