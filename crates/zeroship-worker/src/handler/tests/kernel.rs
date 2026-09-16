use super::*;

#[compio::test]
async fn dispatch_resolves_full_kernel_kv_storage_db_auth() {
    use testcontainers::{
        core::{IntoContainerPort, WaitFor},
        runners::SyncRunner,
        GenericImage,
    };
    let redis = GenericImage::new("redis", "7")
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .start()
        .expect("kernel dispatch test requires Docker to start Redis");
    let endpoint = format!(
        "{}:{}",
        redis.get_host().unwrap(),
        redis.get_host_port_ipv4(6379).unwrap()
    );

    let source = br#"
    export default {
      async fetch(req, env) {
        const present = {
          db: typeof env.db,
          kv: typeof env.kv,
          storage: typeof env.storage,
          auth: typeof env.auth,
        };
        await env.kv.set("phase2-key", "phase2-value");
        const kvBack = await env.kv.get("phase2-key");
        const b64 = btoa("hi");
        await env.storage.put("uploads", "f.txt", b64, "text/plain");
        const raw = await env.storage.get("uploads", "f.txt");
        const got = raw ? JSON.parse(raw) : null;
        const user = await env.auth.getUser();
        return Response.json({
          present,
          kvBack,
          storageBack: got ? got.bytesBase64 : null,
          user,
        });
      }
    }
"#;
    let storage_root = tempfile::tempdir().expect("private object storage");
    let worker = Worker::with_kernel(
        10,
        crate::cache::KernelConfig {
            workflows: Default::default(),
            // This case checks DB namespace registration; PostgreSQL operations
            // are exercised by the database and workflow fixtures.
            db_service: Some(crate::cache::fixture::database_service(
                "postgresql://unused:unused@127.0.0.1:1/unused",
            )),
            kv_store: Some(
                zeroship_kv::KvStore::open(&zeroship_kv::KvConfig::Redis {
                    redis: zeroship_kv::RedisConfig::new(zeroship_kv::Topology::Standalone {
                        endpoint,
                    }),
                })
                .unwrap(),
            ),
            storage_backend: Some(StorageBackendConfig::Local(storage_root.path().to_owned())),
            meter: std::sync::Arc::new(zeroship_metering::Meter::new()),
        },
    );
    let app_id = worker.app_id.clone();
    worker
        .load(source, AppRuntimeLimits::default(), &Manifest::default())
        .await;
    let app = test::init_service(web::App::new().configure(worker.configure())).await;

    let req = test::TestRequest::post()
        .uri(&format!("/dispatch/{}", app_id.as_str()))
        .header("authorization", gateway_authorization())
        .set_payload(dispatch_frame(
            "GET",
            "http://example.test/kernel-probe",
            b"",
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "full-kernel dispatch must succeed; a missing env.* namespace 500s. body: {}",
        String::from_utf8_lossy(&body)
    );
    let v: serde_json::Value = serde_json::from_slice(&body).expect("handler returned JSON");

    assert_eq!(v["present"]["db"], "object", "env.db must resolve");
    assert_eq!(v["present"]["kv"], "object", "env.kv must resolve");
    assert_eq!(
        v["present"]["storage"], "object",
        "env.storage must resolve"
    );
    assert_eq!(v["present"]["auth"], "object", "env.auth must resolve");
    assert_eq!(v["kvBack"], "phase2-value", "kv round-trip");
    assert_eq!(
        v["storageBack"], "aGk=",
        "storage round-trip (base64 of \"hi\")"
    );
    assert!(v["user"].is_null(), "auth.getUser() returns null sans user");
}
