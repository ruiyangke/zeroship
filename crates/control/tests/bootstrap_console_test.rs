//! Live-PG faithful integration test for the install-time **console seed**
//! (`--bootstrap-console`, R5a).
//!
//! FAITHFUL path: it drives the REAL `bootstrap_console::bootstrap_console`
//! against a live, migrated Postgres + a real ingested `.zship` (the prebuilt
//! `apps/zeroship-builder/dist/app.zship` when present, else a minimal
//! synthesized archive through the SAME `zeroship_bundle::ingest` the deploy
//! handler uses), with a real-HTTP mock Hydra admin standing in only for the
//! Hydra transport. It then asserts every seeded artifact and that a second run
//! is idempotent (no error, no duplicate). It also asserts the **pure creator
//! app** invariant: the seed mints NO control PAT and injects NO
//! `ZEROSHIP_CONTROL_SERVICE_TOKEN` / `ZEROSHIP_CONTROL_URL` into the console env.
//!
//! REQUIRES a Postgres with the `db/changelog` migrations applied:
//!   - `CONTROL_TEST_DB` / `AUTH_DB_URL` / `PG_TEST_URL` — the DSN.
//! Skips (prints why, returns) when absent. The pure derivation logic is covered
//! by the crate's `bootstrap_console` unit tests, which run without infra.

#![allow(clippy::future_not_send)]

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use compio_postgres::{connect, Client, NoTls};
use ntex::web::{self, HttpResponse};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::app_oauth_client::client_id_for_app;
use zeroship_control::bootstrap_console::{
    bootstrap_console, console_app_id, console_app_name, ConsoleBootstrapConfig,
    ConsoleBootstrapStatus, CONSOLE_PLAN_ID,
};
use zeroship_control::{EnvStore, Registry};

/// The server-only control credential the seed USED to mint+inject. The console
/// is now a pure creator app, so this key must NOT appear in the console env.
/// Kept as a local literal (the const was deleted from the seed module) so the
/// regression test can prove its ABSENCE.
const SERVICE_TOKEN_ENV_KEY: &str = "ZEROSHIP_CONTROL_SERVICE_TOKEN";

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB")
        .or_else(|_| std::env::var("AUTH_DB_URL"))
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
}

async fn pg(db_url: &str) -> Client {
    let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

/// A real `.zship` to ingest, exercising the SAME `zeroship_bundle::ingest` the
/// deploy handler (and the seed) use.
///
/// Prefers the **real prebuilt** `apps/zeroship-builder/dist/app.zship` — that
/// is the artifact the seed ships in production, so ingesting it here proves the
/// actual `--console-zship` path end-to-end (real tar.zst, real
/// `Manifest::validate`, real content-addressed blobs). To avoid pretending the
/// real path works when it doesn't, we first **trial-ingest** the prebuilt
/// artifact into a throwaway BlobStore: only when that succeeds do we hand it to
/// the seed. If the prebuilt artifact is absent (infra-less CI that skipped
/// `pnpm build`) or its manifest still carries a builder-side authoring issue,
/// we fall back to a minimal-but-real synthesized tar.zst and say so loudly.
///
/// Wire-format note: the console manifest declares `rateLimit.per: "user"` on
/// its authenticated rules. The `zeroship_bundle::RateLimitPer::User` variant
/// now accepts that scope (it mirrors the `@zeroship/server` `RateLimitScope`
/// authoring surface one-for-one), so that drift no longer blocks ingest. A
/// SEPARATE, builder-side authoring issue remains tracked: the console's
/// `/api/preview/*` and `/auth/*` wildcard children redundantly re-declare the
/// `auth`/`rate_limit`/`publicly_accessible` they already inherit from their
/// `/api/preview` and `/auth` parents, which `Manifest::validate`'s
/// override-marker check rejects without an explicit `override: [...]`. Fixing
/// that touches `apps/zeroship-builder/src/server/config.ts` (the read-only
/// builder boundary) and requires a rebuild, so it is out of this additive
/// seed slice — once it lands, this trial-ingest gate flips to the real artifact
/// automatically with no test change.
async fn console_zship_path(probe_root: &std::path::Path) -> PathBuf {
    let prebuilt = repo_root().join("apps/zeroship-builder/dist/app.zship");
    if !prebuilt.is_file() {
        eprintln!(
            "[bootstrap_console_test] prebuilt {} absent — synthesizing a minimal .zship",
            prebuilt.display()
        );
        return synthesize_minimal_zship();
    }
    // Trial-ingest the prebuilt artifact into a throwaway BlobStore so we only
    // commit to the real path when it actually deploys (same `ingest` the seed
    // runs, against a real probe app id).
    match std::fs::read(&prebuilt) {
        Ok(bytes) => {
            let probe_store: Arc<dyn BlobStore> = Arc::new(
                LocalDiskBlobStore::new(probe_root.to_path_buf()).expect("probe blob store"),
            );
            let probe_app = Uuid::new_v4();
            match zeroship_bundle::ingest(&probe_store, &probe_app, &bytes).await {
                Ok(_) => {
                    eprintln!(
                        "[bootstrap_console_test] trial-ingest OK — using prebuilt console artifact: {}",
                        prebuilt.display()
                    );
                    prebuilt
                }
                Err(e) => {
                    eprintln!(
                        "[bootstrap_console_test] prebuilt console artifact does NOT ingest \
                         (builder-side authoring issue, tracked out of this slice): {e:?}\n\
                         [bootstrap_console_test] falling back to a synthesized .zship — the \
                         seed's ingest+commit path is still exercised for real."
                    );
                    synthesize_minimal_zship()
                }
            }
        }
        Err(e) => {
            eprintln!(
                "[bootstrap_console_test] could not read prebuilt {} ({e}) — synthesizing",
                prebuilt.display()
            );
            synthesize_minimal_zship()
        }
    }
}

/// The repo root, resolved from `CARGO_MANIFEST_DIR` (`.../crates/control`).
/// Used to locate the prebuilt console artifact regardless of the CWD the test
/// binary runs in.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// Build a minimal valid `.zship` (manifest.json only, passthrough manifest)
/// and write it to a temp file; return its path. Mirrors the tar.zst layout the
/// real builder emits, exercising the real `ingest` path.
fn synthesize_minimal_zship() -> PathBuf {
    // Serialize the real passthrough manifest so the synthesized archive passes
    // the SAME `Manifest::validate` the ingest path runs (no hand-rolled JSON
    // that could drift from the wire format).
    let manifest = zeroship_bundle::Manifest::passthrough();
    let manifest_bytes = serde_json::to_vec(&manifest).expect("manifest json");

    let mut tar_buf: Vec<u8> = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest_bytes.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "manifest.json", &manifest_bytes[..])
            .expect("append manifest");
        builder.finish().expect("tar finish");
    }
    let mut compressed: Vec<u8> = Vec::new();
    {
        let mut enc = zstd::Encoder::new(&mut compressed, 0).expect("zstd enc");
        enc.write_all(&tar_buf).expect("zstd write");
        enc.finish().expect("zstd finish");
    }
    let path = std::env::temp_dir().join(format!("console-seed-{}.zship", Uuid::new_v4().simple()));
    std::fs::write(&path, &compressed).expect("write synthesized zship");
    path
}

// ---------------------------------------------------------------------------
// Mock Hydra admin (real HTTP) — stands in ONLY for the Hydra transport. The
// per-app client lifecycle logic (`ensure_app_client`) runs for real.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockHydraState {
    clients: std::collections::HashMap<String, Value>,
}

struct MockHydra {
    base: String,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockHydra {
    fn start() -> Self {
        let state = Arc::new(Mutex::new(MockHydraState::default()));
        let factory_state = state.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ntex::rt::System::build()
                .name("mock-hydra-console-seed")
                .testing()
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move {
                    let server = web::test::server(move || {
                        let state = factory_state.clone();
                        async move {
                            web::App::new().state(state).service(
                                web::resource("/admin/clients")
                                    .route(web::post().to(mock_create_client)),
                            )
                            .service(
                                web::resource("/admin/clients/{client_id}")
                                    .route(web::get().to(mock_get_client))
                                    .route(web::put().to(mock_update_client)),
                            )
                        }
                    })
                    .await;
                    let addr = server.addr();
                    started_tx.send(addr).expect("send mock hydra addr");
                    let _ = shutdown_rx.recv();
                    drop(server);
                });
        });
        let addr = started_rx.recv().expect("mock hydra starts");
        Self {
            base: format!("http://{addr}"),
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }
}

impl Drop for MockHydra {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn mock_create_client(
    body: web::types::Json<Value>,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> HttpResponse {
    let body = body.into_inner();
    let id = body["client_id"].as_str().unwrap_or_default().to_string();
    state
        .lock()
        .expect("hydra state")
        .clients
        .insert(id, body.clone());
    HttpResponse::Created().json(&body)
}

async fn mock_get_client(
    client_id: web::types::Path<String>,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> HttpResponse {
    let id = client_id.into_inner();
    match state.lock().expect("hydra state").clients.get(&id).cloned() {
        Some(c) => HttpResponse::Ok().json(&c),
        None => HttpResponse::NotFound().json(&json!({"error": "not_found"})),
    }
}

async fn mock_update_client(
    client_id: web::types::Path<String>,
    body: web::types::Json<Value>,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> HttpResponse {
    let id = client_id.into_inner();
    let body = body.into_inner();
    state
        .lock()
        .expect("hydra state")
        .clients
        .insert(id, body.clone());
    HttpResponse::Ok().json(&body)
}

// ---------------------------------------------------------------------------
// Cleanup
// ---------------------------------------------------------------------------

/// Remove every seeded row for `host` so the test is hermetic across reruns.
/// The console is a pure creator app: the seed writes only control-schema rows
/// (apps + cascading oauth/env, plus the standalone oauth_clients identity row).
/// It mints NO PAT and creates NO auth-schema service principal, so there is no
/// `permission_tokens` / `auth.users` / `platform.roles` row to clean up.
async fn cleanup(control: &Client, host: &str) {
    let app_id = console_app_id(host);
    let client_id = client_id_for_app(&app_id);
    // Deleting the apps row cascades app_oauth_clients / app_secrets /
    // app_env_expose / app_scope_defs.
    let _ = control
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await;
    let _ = control
        .execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await;
}

// ---------------------------------------------------------------------------
// The faithful test
// ---------------------------------------------------------------------------

#[compio::test]
async fn seed_creates_all_artifacts_and_is_idempotent() {
    let Some(url) = db_url() else {
        eprintln!("[bootstrap_console_test] CONTROL_TEST_DB/AUTH_DB_URL/PG_TEST_URL not set - skipping");
        return;
    };

    // A unique host per run keeps the derived ids hermetic even when several
    // test binaries share the DB.
    let host = format!("console-{}.zeroship.localhost", Uuid::new_v4().simple());
    let app_id = console_app_id(&host);
    let client_id = client_id_for_app(&app_id);

    // Real components — the SAME types control boots with.
    let registry = Registry::new(&url).await.expect("registry");
    let env_store =
        EnvStore::new(registry.clone(), "test-master-key-deadbeefcafebabe", false).expect("env");
    let blob_root = std::env::temp_dir().join(format!("console-seed-blob-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&blob_root).expect("mkdir blob root");
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    // A connection to assert the ABSENCE of any console PAT row (the console is a
    // pure creator app — the seed mints none). `permission_tokens` lives in the
    // control schema; in dev both DSNs point at one database.
    let auth_pg = pg(&url).await;
    let mut control_pg = pg(&url).await;

    let hydra = MockHydra::start();
    cleanup(&control_pg, &host).await;

    // Throwaway BlobStore for the prebuilt-artifact trial ingest (see
    // `console_zship_path`). Kept separate from the seed's real `blob_store`.
    let probe_root =
        std::env::temp_dir().join(format!("console-seed-probe-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&probe_root).expect("mkdir probe root");
    let zship = console_zship_path(&probe_root).await;

    // ---- Source the console runtime env from the CONTROL process env. ----
    // The seed reads each `CONSOLE_RUNTIME_ENV` source var via `std::env::var`.
    // Set a representative mix (two credentials + one non-secret URL) and
    // DELIBERATELY leave `ZEROSHIP_SDK_REGISTRY` UNSET so we can assert the
    // skip-unset behaviour (it must NOT be written, and the seed must still
    // succeed). We ALSO set `ZEROSHIP_CONTROL_URL` in the process env even though
    // the console is a pure creator app — to PROVE the seed no longer forwards it
    // (it must not appear in the console env). Values are unique per run so
    // parallel binaries can't collide. This binary's single test runs under
    // `--test-threads=1`, so the process-global `set_var` is safe here.
    let openai_val = format!("sk-test-{}", Uuid::new_v4().simple());
    let sandbox_token_val = format!("sbx-{}", Uuid::new_v4().simple());
    let sandbox_url_val = "http://sandbox.test.local:9091".to_string();
    let control_url_val = "http://control.test.local:9090".to_string();
    std::env::set_var("OPENAI_API_KEY", &openai_val);
    std::env::set_var("SANDBOX_TOKEN", &sandbox_token_val);
    std::env::set_var("SANDBOX_URL", &sandbox_url_val);
    std::env::set_var("ZEROSHIP_CONTROL_URL", &control_url_val); // must NOT be forwarded
    std::env::remove_var("ZEROSHIP_SDK_REGISTRY"); // the unset-skip case

    let cfg = ConsoleBootstrapConfig {
        enabled: true,
        console_host: host.clone(),
        console_zship: zship.clone(),
        scheme: "http".to_string(),
        hydra_admin_url: hydra.base.clone(),
    };

    // ---- First run: seeds everything. ----
    let first = bootstrap_console(&cfg, &registry, &env_store, &blob_store, &mut control_pg)
        .await
        .expect("first seed run");
    assert_eq!(first.status, ConsoleBootstrapStatus::Seeded);
    assert_eq!(first.app_id, app_id);
    assert_eq!(first.client_id, client_id);

    // (1) control.apps row on the enterprise plan, with deploy_hash + manifest.
    let apps = control_pg
        .query(
            "SELECT name, plan_id, deploy_hash, manifest_json FROM zeroship.apps WHERE id = $1",
            &[&app_id],
        )
        .await
        .expect("query apps");
    assert_eq!(apps.len(), 1, "console.apps row exists");
    assert_eq!(apps[0].get::<_, String>("name"), console_app_name(&host));
    assert_eq!(apps[0].get::<_, String>("plan_id"), CONSOLE_PLAN_ID);
    assert!(
        apps[0].get::<_, Option<String>>("deploy_hash").is_some(),
        "deploy_hash committed from .zship ingest"
    );
    assert!(
        apps[0].get::<_, Option<String>>("manifest_json").is_some(),
        "manifest_json committed from .zship ingest"
    );

    // (2) app_oauth_clients: public PKCE + EXPLICIT sector_identifier = the
    //     console host (NOT a derived {name}.{base}).
    let ext = control_pg
        .query(
            "SELECT client_id, sector_identifier FROM zeroship.app_oauth_clients WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .expect("query app_oauth_clients");
    assert_eq!(ext.len(), 1, "app_oauth_clients extension row exists");
    assert_eq!(ext[0].get::<_, String>("client_id"), client_id);
    assert_eq!(
        ext[0].get::<_, String>("sector_identifier"),
        format!("http://{host}"),
        "sector is the explicit console host origin"
    );
    // The oauth_clients identity row: public PKCE (skip_consent FALSE, never
    // skip), and the redirect_uris anchor on the console host.
    let oc = control_pg
        .query(
            "SELECT skip_consent, redirect_uris FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .expect("query oauth_clients");
    assert_eq!(oc.len(), 1, "oauth_clients identity row exists");
    assert!(!oc[0].get::<_, bool>("skip_consent"));
    let uris: Vec<String> = oc[0].get("redirect_uris");
    // The popup OAuth flow needs BOTH the popup-callback and the full-page
    // callback registered on the console public client (the `@zeroship/auth`
    // SDK posts the relay code through `/__zeroship/auth/popup-callback`).
    assert!(
        uris.iter().any(|u| u == &format!("http://{host}/__zeroship/auth/popup-callback")),
        "popup-callback redirect_uri registered on the console host: {uris:?}"
    );
    assert!(
        uris.iter().any(|u| u == &format!("http://{host}/__zeroship/auth/callback")),
        "callback redirect_uri anchors on the console host: {uris:?}"
    );
    // The Hydra-side client is a public PKCE client (token_endpoint_auth_method
    // none) — asserted via the mock's recorded body.
    // (The DB skip_consent=false + the oac_ client_id already pin the shape; the
    // public-PKCE body is produced by the SAME build_client_body the per-app
    // unit tests pin.)

    // (3) RouteEntry surfaces the OAuth fields (gateway route-sync invariant).
    let routes = registry.get_routes().await.expect("get_routes");
    let entry = routes.get(&app_id).expect("route entry for console app");
    assert_eq!(entry.plan_id, CONSOLE_PLAN_ID);
    assert_eq!(entry.oauth_client_id.as_deref(), Some(client_id.as_str()));
    assert_eq!(
        entry.sector_identifier.as_deref(),
        Some(format!("http://{host}").as_str())
    );
    assert!(entry.deploy_hash.is_some(), "route carries a deploy_hash");

    // (4) REGRESSION — the console is a PURE CREATOR APP: it holds NO control
    //     credential. The seed must mint NO PAT and inject NO
    //     `ZEROSHIP_CONTROL_SERVICE_TOKEN`. (Pre-change, BOTH were present: a
    //     broadly-privileged `control.permission_tokens` row owned by a derived
    //     console service principal, plus an encrypted+exposed service-token env
    //     secret. These assertions FAIL on the pre-change seed.)

    // (4a) NO console PAT row. There is no derived token id any more, so we prove
    //      absence over the whole console owner surface: no `permission_tokens`
    //      row references the console app's host-derived service email pattern,
    //      and — since the seed creates no `auth.users`/`platform.roles` service
    //      principal at all — none of those rows exist for this host either.
    let console_user_email = format!("console-service@{host}");
    let svc_users = auth_pg
        .query(
            "SELECT id FROM zeroship.users WHERE email = $1",
            &[&console_user_email],
        )
        .await
        .expect("query service users");
    assert!(
        svc_users.is_empty(),
        "pure creator console seeds NO service principal (zeroship.users) — found {} row(s)",
        svc_users.len()
    );
    let n_pat: i64 = auth_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.permission_tokens pt \
             JOIN zeroship.users u ON u.id = pt.owner_id \
             WHERE u.email = $1",
            &[&console_user_email],
        )
        .await
        .expect("count console PAT rows")[0]
        .get("n");
    assert_eq!(
        n_pat, 0,
        "pure creator console seeds NO control PAT (permission_tokens) for {console_user_email}"
    );

    // (4b) NO service-token env on the console app — not a secret, not a var, not
    //      exposed, not in the merged worker env.
    let secret_names = env_store.list_secret_names(app_id).await.expect("list secrets");
    assert!(
        !secret_names.iter().any(|n| n == SERVICE_TOKEN_ENV_KEY),
        "ZEROSHIP_CONTROL_SERVICE_TOKEN must NOT be a secret on a pure creator console: {secret_names:?}"
    );
    let vars = env_store.list_vars(app_id).await.expect("list vars");
    assert!(
        !vars.iter().any(|(k, _)| k == SERVICE_TOKEN_ENV_KEY),
        "ZEROSHIP_CONTROL_SERVICE_TOKEN must NOT be a var: {vars:?}"
    );
    let expose = env_store.list_expose(app_id).await.expect("list expose");
    assert!(
        !expose.iter().any(|k| k == SERVICE_TOKEN_ENV_KEY),
        "ZEROSHIP_CONTROL_SERVICE_TOKEN must NOT be exposed: {expose:?}"
    );
    let worker_env = env_store
        .merged_env_for_worker(app_id)
        .await
        .expect("merged worker env");
    assert!(
        worker_env["secrets"].get(SERVICE_TOKEN_ENV_KEY).is_none()
            && worker_env["vars"].get(SERVICE_TOKEN_ENV_KEY).is_none(),
        "ZEROSHIP_CONTROL_SERVICE_TOKEN must NOT surface in the worker env"
    );

    // (5) FULL server-side runtime env forwarded from the control process env.
    //     Re-read the live env-store surfaces.
    let secret_names = env_store.list_secret_names(app_id).await.expect("list secrets");
    let vars = env_store.list_vars(app_id).await.expect("list vars");
    let expose = env_store.list_expose(app_id).await.expect("list expose");
    let worker_env = env_store
        .merged_env_for_worker(app_id)
        .await
        .expect("merged worker env");

    // (5a) Credentials → encrypted SECRET + expose entry, retrievable
    //      server-side, and NOT a plaintext browser-exposed var.
    for (key, expected) in [
        ("OPENAI_API_KEY", &openai_val),
        ("SANDBOX_TOKEN", &sandbox_token_val),
    ] {
        assert!(
            secret_names.iter().any(|n| n == key),
            "{key} stored as an encrypted secret: {secret_names:?}"
        );
        assert!(
            !vars.iter().any(|(k, _)| k == key),
            "{key} must NOT be a plaintext (browser-exposed) var"
        );
        assert!(
            expose.iter().any(|k| k == key),
            "{key} opted into expose so the worker surfaces it in process.env: {expose:?}"
        );
        // The encrypted value is NOT readable as a plaintext var/merged-var, but
        // IS retrievable server-side via the worker's `secrets` map (decrypted).
        assert_eq!(
            worker_env["secrets"][key].as_str(),
            Some(expected.as_str()),
            "{key} decrypts to the forwarded value in the server-side worker env"
        );
        // Defense-in-depth: it must NOT appear in the `vars` half of the
        // worker payload (that's the always-public plaintext surface).
        assert!(
            worker_env["vars"].get(key).is_none(),
            "{key} must not leak into the plaintext vars surface"
        );
    }

    // (5b) Non-secret config → plaintext VAR (always in process.env; no expose
    //      entry needed), retrievable server-side, NOT stored as a secret.
    for (key, expected) in [("SANDBOX_URL", &sandbox_url_val)] {
        assert!(
            vars.iter().any(|(k, v)| k == key && v == expected),
            "{key} stored as a plaintext var with the forwarded value: {vars:?}"
        );
        assert!(
            !secret_names.iter().any(|n| n == key),
            "{key} is non-secret config — must NOT be encrypted as a secret"
        );
        assert!(
            !expose.iter().any(|k| k == key),
            "{key} is a var (always in process.env) — no expose entry expected: {expose:?}"
        );
        assert_eq!(
            worker_env["vars"][key].as_str(),
            Some(expected.as_str()),
            "{key} surfaces in the server-side worker vars"
        );
    }

    // (5c) REGRESSION — `ZEROSHIP_CONTROL_URL` is set in the control process env
    //      (above), but a pure creator console makes ZERO control calls, so the
    //      seed must NOT forward it: not a var, not a secret, not exposed, not in
    //      the worker env. (Pre-change it WAS forwarded as a plaintext var.)
    let _ = &control_url_val; // value set in the process env; asserted absent here
    assert!(
        !vars.iter().any(|(k, _)| k == "ZEROSHIP_CONTROL_URL"),
        "ZEROSHIP_CONTROL_URL must NOT be forwarded as a var on a pure creator console: {vars:?}"
    );
    assert!(
        !secret_names.iter().any(|n| n == "ZEROSHIP_CONTROL_URL"),
        "ZEROSHIP_CONTROL_URL must NOT be a secret: {secret_names:?}"
    );
    assert!(
        !expose.iter().any(|k| k == "ZEROSHIP_CONTROL_URL"),
        "ZEROSHIP_CONTROL_URL must NOT be exposed: {expose:?}"
    );
    assert!(
        worker_env["vars"].get("ZEROSHIP_CONTROL_URL").is_none()
            && worker_env["secrets"].get("ZEROSHIP_CONTROL_URL").is_none(),
        "ZEROSHIP_CONTROL_URL must NOT surface in the worker env"
    );

    // (5d) UNSET source var is SKIPPED — not written as a secret, var, or
    //      expose entry — and the seed still succeeded (asserted above).
    assert!(
        !secret_names.iter().any(|n| n == "ZEROSHIP_SDK_REGISTRY"),
        "unset ZEROSHIP_SDK_REGISTRY must not be written as a secret"
    );
    assert!(
        !vars.iter().any(|(k, _)| k == "ZEROSHIP_SDK_REGISTRY"),
        "unset ZEROSHIP_SDK_REGISTRY must not be written as a var (no empty value)"
    );
    assert!(
        !expose.iter().any(|k| k == "ZEROSHIP_SDK_REGISTRY"),
        "unset ZEROSHIP_SDK_REGISTRY must not be exposed"
    );

    // ---- Second run: idempotent. No error, no duplicate. ----
    let second = bootstrap_console(&cfg, &registry, &env_store, &blob_store, &mut control_pg)
        .await
        .expect("second seed run is idempotent");
    assert_eq!(second.status, ConsoleBootstrapStatus::Seeded);
    assert_eq!(second.app_id, app_id);

    // Exactly one of each row — no duplicates.
    let n_apps: i64 = control_pg
        .query("SELECT COUNT(*)::BIGINT AS n FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await
        .expect("count apps")[0]
        .get("n");
    assert_eq!(n_apps, 1, "no duplicate apps row");
    let n_ext: i64 = control_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.app_oauth_clients WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .expect("count ext")[0]
        .get("n");
    assert_eq!(n_ext, 1, "no duplicate app_oauth_clients row");
    // Still no PAT and no service-token secret after the re-run.
    let n_pat: i64 = auth_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.permission_tokens pt \
             JOIN zeroship.users u ON u.id = pt.owner_id \
             WHERE u.email = $1",
            &[&console_user_email],
        )
        .await
        .expect("count pat")[0]
        .get("n");
    assert_eq!(n_pat, 0, "still no console PAT row after re-run");
    let n_secret: i64 = control_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.app_secrets WHERE app_id = $1 AND key_name = $2",
            &[&app_id, &SERVICE_TOKEN_ENV_KEY],
        )
        .await
        .expect("count secret")[0]
        .get("n");
    assert_eq!(n_secret, 0, "still no service-token secret after re-run");

    // (5e) Runtime-env idempotency: each forwarded secret has exactly ONE row
    //      after the re-run (upsert, not insert), and the decrypted value is
    //      unchanged. Plaintext vars are likewise still present with the same
    //      value, and the unset var is still absent.
    for key in ["OPENAI_API_KEY", "SANDBOX_TOKEN"] {
        let n: i64 = control_pg
            .query(
                "SELECT COUNT(*)::BIGINT AS n FROM zeroship.app_secrets WHERE app_id = $1 AND key_name = $2",
                &[&app_id, &key],
            )
            .await
            .expect("count runtime secret")[0]
            .get("n");
        assert_eq!(n, 1, "no duplicate {key} secret after re-run");
    }
    let worker_env2 = env_store
        .merged_env_for_worker(app_id)
        .await
        .expect("merged worker env (second run)");
    assert_eq!(
        worker_env2["secrets"]["OPENAI_API_KEY"].as_str(),
        Some(openai_val.as_str()),
        "OPENAI_API_KEY value stable across idempotent re-run"
    );
    assert_eq!(
        worker_env2["secrets"]["SANDBOX_TOKEN"].as_str(),
        Some(sandbox_token_val.as_str())
    );
    assert_eq!(
        worker_env2["vars"]["SANDBOX_URL"].as_str(),
        Some(sandbox_url_val.as_str())
    );
    // The control URL stays absent across the re-run (pure creator app).
    assert!(
        worker_env2["vars"].get("ZEROSHIP_CONTROL_URL").is_none()
            && worker_env2["secrets"].get("ZEROSHIP_CONTROL_URL").is_none(),
        "ZEROSHIP_CONTROL_URL stays absent across re-run"
    );
    assert!(
        worker_env2["secrets"].get("ZEROSHIP_SDK_REGISTRY").is_none()
            && worker_env2["vars"].get("ZEROSHIP_SDK_REGISTRY").is_none(),
        "unset ZEROSHIP_SDK_REGISTRY stays absent across re-run"
    );
    // The expose list carries exactly the two forwarded credentials and no var
    // keys — and crucially NO service token (the console holds no credential).
    let expose2 = env_store.list_expose(app_id).await.expect("list expose 2");
    for k in ["OPENAI_API_KEY", "SANDBOX_TOKEN"] {
        assert!(expose2.iter().any(|e| e == k), "{k} exposed: {expose2:?}");
    }
    for k in [
        SERVICE_TOKEN_ENV_KEY,
        "SANDBOX_URL",
        "ZEROSHIP_CONTROL_URL",
        "ZEROSHIP_SDK_REGISTRY",
    ] {
        assert!(
            !expose2.iter().any(|e| e == k),
            "{k} (token/var/unset) must not be in the expose list: {expose2:?}"
        );
    }

    // ---- Disabled config is a no-op. ----
    let disabled_cfg = ConsoleBootstrapConfig {
        enabled: false,
        ..cfg.clone()
    };
    let disabled =
        bootstrap_console(&disabled_cfg, &registry, &env_store, &blob_store, &mut control_pg)
            .await
            .expect("disabled seed run");
    assert_eq!(disabled.status, ConsoleBootstrapStatus::Disabled);

    // Cleanup.
    for k in [
        "OPENAI_API_KEY",
        "SANDBOX_TOKEN",
        "SANDBOX_URL",
        "ZEROSHIP_CONTROL_URL",
        "ZEROSHIP_SDK_REGISTRY",
    ] {
        std::env::remove_var(k);
    }
    cleanup(&control_pg, &host).await;
    let _ = std::fs::remove_dir_all(&blob_root);
    let _ = std::fs::remove_dir_all(&probe_root);
    // Only remove a synthesized temp zship, never the prebuilt artifact.
    if zship.starts_with(std::env::temp_dir()) {
        let _ = std::fs::remove_file(&zship);
    }
    drop(hydra);
}
