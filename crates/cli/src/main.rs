//! zeroship CLI — serve, deploy, login, secret, var.
//!
//! Commands:
//!   zeroship serve   <file-or-dir> [--port=3000] [--workers=0]
//!   zeroship deploy  <path-to-.zship> --app=<name> [--control=URL] [--token=PAT] [--no-create]
//!
//! `build` and `inspect` were removed in the artifact-layout redesign —
//! the canonical build path is now `@zeroship/vite-plugin`, which emits
//! `.zship` archives. `deploy` uploads those archives directly to the
//! control plane. Deploy/secret/var commands read the bearer token from
//! `--token=PAT`, `ZEROSHIP_TOKEN`, or credentials saved by `zeroship login`.

use std::path::PathBuf;
use std::sync::Arc;

use zeroship_runtime::{ModuleEntry, NativePlugin};

mod auth;
mod secrets;

fn main() {
    // CLI's stdout/stderr is the user's product (e.g. `zeroship deploy`
    // prints the deploy hash for scripts to capture). Library tracing
    // emissions (runtime, plugin crates) are kept quiet by default —
    // operators surface them with `RUST_LOG=info`.
    zeroship_core::observability::init_tracing("warn");

    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "serve" => cmd_serve(&args),
        "deploy" => cmd_deploy(&args),
        "login" => exit_on_error("login", auth::cmd_login(&args)),
        "logout" => exit_on_error("logout", auth::cmd_logout()),
        "whoami" => exit_on_error("whoami", auth::cmd_whoami()),
        "secret" => secrets::cmd_secret(&args),
        "var" => secrets::cmd_var(&args),
        _ => print_usage(),
    }
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

fn cmd_serve(args: &[String]) {
    let input = args.get(2).expect(
        "Usage: zeroship serve <file> [--port=3000] [--workers=0] [--cpu-limit=MS] [--wall-timeout=MS]",
    );
    if let Err(e) = check_unknown_serve_flags(args) {
        eprintln!("zeroship serve: {e}");
        eprintln!("Usage: zeroship serve <file> [--port=3000] [--workers=0] [--cpu-limit=MS] [--wall-timeout=MS]");
        std::process::exit(1);
    }
    let port = parse_flag_u16(args, "--port").unwrap_or(3000);
    let workers: usize = parse_flag_usize(args, "--workers").unwrap_or(0);
    let cpu_limit = parse_flag_u64(args, "--cpu-limit")
        .map(std::time::Duration::from_millis);
    let wall_timeout = parse_flag_u64(args, "--wall-timeout")
        .map(std::time::Duration::from_millis);
    // Dev default: 512 MB. Single-tenant dev apps routinely load big libraries
    // (LangChain + provider SDKs = ~100 MB by themselves). The production
    // worker's 128 MB default is sized for multi-tenant isolation, not for
    // single-process dev. CLI flag or ZEROSHIP_HEAP_LIMIT_MB overrides.
    let heap_limit_bytes = parse_flag_usize(args, "--heap-limit-mb")
        .or_else(|| std::env::var("ZEROSHIP_HEAP_LIMIT_MB").ok().and_then(|s| s.parse().ok()))
        .map(|mb| mb * 1024 * 1024)
        .or(Some(512 * 1024 * 1024));

    let input_path = PathBuf::from(input);
    if !input_path.is_file() {
        eprintln!(
            "zeroship serve: expected a JS file path; got {}",
            input_path.display()
        );
        eprintln!("Directory builds now go through @zeroship/vite-plugin.");
        std::process::exit(1);
    }

    let source = std::fs::read_to_string(&input_path).unwrap_or_else(|e| {
        eprintln!("Failed to read {}: {e}", input_path.display());
        std::process::exit(1);
    });
    let name = input_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    eprintln!(
        "[zeroship] Loaded {} ({:.1}KB)",
        input_path.display(),
        source.len() as f64 / 1024.0
    );
    let modules = vec![ModuleEntry {
        specifier: name,
        source,
    }];

    // Pre-check port availability so a bind failure surfaces as a clean error
    // message instead of a panic stacktrace. We briefly bind the port with the
    // standard library (synchronously, before spinning up V8 / compio), then
    // immediately drop the socket. The window between this probe and the real
    // bind inside `start_server` is tiny; a race is benign (both paths produce
    // "address in use") and vastly better than the previous panic stacktrace.
    if let Err(e) = std::net::TcpListener::bind(format!("0.0.0.0:{port}")) {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            eprintln!("zeroship serve: port {port} is already in use");
            eprintln!("Hint: use --port=<N> to choose a different port.");
        } else {
            eprintln!("zeroship serve: cannot bind port {port}: {e}");
        }
        std::process::exit(1);
    }
    // Socket is released here so `start_server` can bind the real listener.
    eprintln!("[zeroship] Starting server on port {port}");

    // Opt-in db plugin: when DATABASE_URL is set, register the db plugin
    // so JS `zeroship.db.*` works in the dev path (e.g. `vite-plugin` spawns
    // `zeroship serve` with DATABASE_URL forwarded from `.env`).
    let mut plugins: Vec<Arc<dyn NativePlugin>> = Vec::new();

    // Dev metering infrastructure: a process-wide meter the db/kv/storage
    // producers emit raw usage metrics into, for namespace/behaviour parity
    // with the production worker kernel. There is NO `env.meter` creator API
    // (metering is infrastructure — app code can't forge/suppress it). In dev
    // there is no control plane to flush to, so NO flush task is spawned: the
    // meter accumulates locally and is never drained (intentional — dev
    // doesn't bill). The production worker pairs the same meter with a
    // `spawn_flush_task` → control POST.
    let dev_meter = Arc::new(zeroship_metering::Meter::new());
    eprintln!("[zeroship] metering infrastructure on (dev: no flush — local accumulation only)");

    if let Ok(url) = std::env::var("DATABASE_URL") {
        if !url.is_empty() {
            plugins.push(Arc::new(zeroship_plugin_db::DbPlugin::new(
                url,
                Some(Arc::clone(&dev_meter)),
            )));
            eprintln!("[zeroship] db plugin registered (DATABASE_URL set)");
        }
    }

    // Storage plugin: always on in dev. `$ZEROSHIP_STORAGE_URL` selects the
    // backend through the SAME parser the worker uses (`--storage-url`): a
    // bare path or `file://…` → LocalFs (default `<cwd>/.zeroship/storage`);
    // `s3://…` → the S3 backend (creds from the AWS env vars). The
    // vite-plugin's scaffolded .gitignore already excludes `.zeroship/`.
    let storage_url = std::env::var("ZEROSHIP_STORAGE_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "file://.zeroship/storage".to_string());
    // `file://` is config ergonomics for a local path; strip the scheme so
    // the parser sees a bare path. `s3://` falls through to the S3 leg.
    let storage_arg = storage_url.strip_prefix("file://").unwrap_or(&storage_url);
    let storage_cfg = match zeroship_plugin_storage::StorageBackendConfig::parse(storage_arg) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[zeroship] invalid ZEROSHIP_STORAGE_URL: {e}");
            std::process::exit(2);
        }
    };
    let storage_kind = storage_cfg.kind();
    match zeroship_plugin_storage::build_backend(&storage_cfg) {
        Ok(backend) => {
            plugins.push(Arc::new(zeroship_plugin_storage::StoragePlugin::with_backend_and_meter(
                backend,
                Some(Arc::clone(&dev_meter)),
            )));
            eprintln!("[zeroship] storage plugin registered (backend={storage_kind})");
        }
        Err(e) => {
            eprintln!("[zeroship] storage backend init failed: {e}");
            std::process::exit(2);
        }
    }

    // Auth plugin: always on. Stateless — the callbacks read the
    // per-request user from `RuntimeState` (set from the verified
    // `ZeroShip-User` header). Pinned here AND in the worker's
    // `create_plugins()` so `env.auth.getUser()` resolves on both the dev
    // (`zeroship serve`) and the production worker path.
    plugins.push(Arc::new(zeroship_runtime::auth::AuthPlugin));
    eprintln!("[zeroship] auth plugin registered");

    // KV plugin backend selection, in priority order:
    //   1. ZEROSHIP_KV_URL set → Redis (distributed-correctness: shared
    //                            across workers/regions).
    //   2. otherwise           → redb (single-process persistent embedded
    //                            store; self-host / dev tier). Path is
    //                            ZEROSHIP_KV_PATH if set, else the default
    //                            `./.zeroship/kv.redb`.
    let kv_plugin = match std::env::var("ZEROSHIP_KV_URL") {
        Ok(url) if !url.is_empty() => {
            eprintln!("[zeroship] kv plugin registered (redis)");
            zeroship_plugin_kv::KvPlugin::with_backend_and_meter(
                Arc::new(zeroship_plugin_kv::Redis::new(url)),
                Some(Arc::clone(&dev_meter)),
            )
        }
        _ => {
            let kv_path: PathBuf = std::env::var_os("ZEROSHIP_KV_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".zeroship/kv.redb"));
            // Create the parent dir so a default `./.zeroship/kv.redb`
            // opens cleanly on a fresh checkout.
            if let Some(parent) = kv_path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!(
                        "[zeroship] kv: failed to create dir '{}': {e}",
                        parent.display()
                    );
                    std::process::exit(1);
                }
            }
            let backend = zeroship_plugin_kv::RedbBackend::open(&kv_path)
                .unwrap_or_else(|e| {
                    eprintln!(
                        "[zeroship] kv: failed to open redb at '{}': {e}",
                        kv_path.display()
                    );
                    std::process::exit(1);
                });
            eprintln!("[zeroship] kv plugin registered (redb; path={})", kv_path.display());
            zeroship_plugin_kv::KvPlugin::with_backend_and_meter(
                Arc::new(backend),
                Some(Arc::clone(&dev_meter)),
            )
        }
    };
    plugins.push(Arc::new(kv_plugin));

    // Forward process env to the V8 runtime so `process.env.FOO` works in JS.
    // Important for dev: the vite-plugin sets ZEROSHIP_ENTRY / ZEROSHIP_VITE_WS
    // in the spawned child env, and user apps expect access to OPENAI_API_KEY
    // etc. Without this, `process.env` in V8 is empty.
    let env_vars: std::collections::HashMap<String, String> = std::env::vars().collect();

    zeroship_runtime::serve::start_server(
        modules,
        zeroship_runtime::serve::ServerOptions {
            port,
            workers,
            cpu_limit,
            wall_timeout,
            heap_limit_bytes,
            env_vars,
            plugins,
        },
    );
}

// ---------------------------------------------------------------------------
// deploy
// ---------------------------------------------------------------------------

/// Upload a pre-built `.zship` archive to the control plane. The
/// vite-plugin emits these; this command is a thin curl wrapper that
/// posts the bytes to `POST /api/apps/{id}/deploy`.
//
// Schema discovery is migration-first: the runtime reads the generated
// descriptor carried by the `.zship`, not a schema object off the entry module.
// A raw `.js` deploy with database tables needs committed migrations plus the
// generated descriptor before it can install typed `env.db`.
fn cmd_deploy(args: &[String]) {
    let input = args.get(2).expect(
        "Usage: zeroship deploy <path-to-.zship> --app=<name> [--control=http://localhost:9090] [--token=<PAT>]",
    );
    let app = flag_str(args, "--app=").expect("--app=<name> is required");
    let control_url = flag_str(args, "--control=")
        .or_else(|| std::env::var("ZEROSHIP_CONTROL_URL").ok())
        .unwrap_or_else(|| "http://localhost:9090".into());
    let token = resolve_bearer_token(args).unwrap_or_else(|e| {
        eprintln!("zeroship deploy: {e}");
        std::process::exit(1);
    });
    let auto_create = deploy_auto_create(args);

    let input_path = PathBuf::from(input);
    let body = std::fs::read(&input_path).unwrap_or_else(|e| {
        eprintln!("Failed to read {}: {e}", input_path.display());
        eprintln!("Run `vite build` (with @zeroship/vite-plugin) to produce a .zship archive.");
        std::process::exit(1);
    });

    eprintln!(
        "Deploying {} ({:.1}KB) to {control_url}/api/apps/{app}/deploy...",
        input_path.display(),
        body.len() as f64 / 1024.0,
    );

    let mut client = CurlControlClient;
    match deploy_archive(&mut client, &control_url, &app, &token, &body, auto_create) {
        Ok(outcome) => {
            if let Some(created) = outcome.created_app {
                eprintln!("created app {} ({})", created.name, created.id);
            }
            eprintln!("Deployed successfully!");
            if let Some(hash) = outcome.deploy_hash {
                eprintln!("  deploy_hash: {hash}");
            }
        }
        Err(e) => {
            eprintln!("zeroship deploy: {e}");
            std::process::exit(1);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ControlResponse {
    status: u16,
    body: String,
}

trait ControlClient {
    fn deploy_zship(
        &mut self,
        control_url: &str,
        app: &str,
        token: &str,
        body: &[u8],
    ) -> Result<ControlResponse, String>;

    fn list_apps(&mut self, control_url: &str, token: &str) -> Result<ControlResponse, String>;

    fn create_app(
        &mut self,
        control_url: &str,
        token: &str,
        name: &str,
    ) -> Result<ControlResponse, String>;
}

struct CurlControlClient;

impl ControlClient for CurlControlClient {
    fn deploy_zship(
        &mut self,
        control_url: &str,
        app: &str,
        token: &str,
        body: &[u8],
    ) -> Result<ControlResponse, String> {
        let deploy_url = format!("{control_url}/api/apps/{app}/deploy");
        let auth = format!("Authorization: Bearer {token}");
        let mut command = std::process::Command::new("curl");
        command.args([
            "-s",
            "-w",
            "\n%{http_code}",
            "-X",
            "POST",
            &deploy_url,
            "-H",
            &auth,
            "-H",
            "Content-Type: application/x-zship",
            "--data-binary",
            "@-",
        ]);
        run_curl(&mut command, Some(body))
    }

    fn list_apps(&mut self, control_url: &str, token: &str) -> Result<ControlResponse, String> {
        let url = format!("{control_url}/api/apps");
        let auth = format!("Authorization: Bearer {token}");
        let mut command = std::process::Command::new("curl");
        command.args(["-s", "-w", "\n%{http_code}", "-H", &auth, &url]);
        run_curl(&mut command, None)
    }

    fn create_app(
        &mut self,
        control_url: &str,
        token: &str,
        name: &str,
    ) -> Result<ControlResponse, String> {
        let url = format!("{control_url}/api/apps");
        let auth = format!("Authorization: Bearer {token}");
        let body = serde_json::to_vec(&serde_json::json!({ "name": name }))
            .map_err(|e| format!("serialize create-app request: {e}"))?;
        let mut command = std::process::Command::new("curl");
        command.args([
            "-s",
            "-w",
            "\n%{http_code}",
            "-X",
            "POST",
            &url,
            "-H",
            &auth,
            "-H",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
        ]);
        run_curl(&mut command, Some(&body))
    }
}

fn run_curl(
    command: &mut std::process::Command,
    stdin_body: Option<&[u8]>,
) -> Result<ControlResponse, String> {
    let stdin = if stdin_body.is_some() {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    };
    let mut child = command
        .stdin(stdin)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!("failed to run curl: {e}; make sure curl is installed and the control plane is running")
        })?;

    if let Some(body) = stdin_body {
        use std::io::Write;
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "failed to open curl stdin".to_string())?;
        stdin
            .write_all(body)
            .map_err(|e| format!("failed to write request body to curl: {e}"))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to wait for curl: {e}"))?;
    Ok(parse_curl_response(output))
}

fn parse_curl_response(output: std::process::Output) -> ControlResponse {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim_end_matches(&['\r', '\n'][..]);
    let (body, status_str) = stdout.rsplit_once('\n').unwrap_or(("", stdout));
    ControlResponse {
        status: status_str.trim().parse().unwrap_or(0),
        body: body.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeployOutcome {
    deploy_hash: Option<String>,
    created_app: Option<CreatedApp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CreatedApp {
    name: String,
    id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedApp {
    id: String,
    created: Option<CreatedApp>,
}

fn deploy_archive<C: ControlClient>(
    client: &mut C,
    control_url: &str,
    app: &str,
    token: &str,
    body: &[u8],
    auto_create: bool,
) -> Result<DeployOutcome, String> {
    if !is_uuid(app) {
        let resolved = resolve_or_create_app(client, control_url, token, app, None, auto_create)?;
        let response = client.deploy_zship(control_url, &resolved.id, token, body)?;
        if response.status != 200 {
            return Err(format!(
                "Deploy failed (HTTP {}): {}",
                response.status, response.body
            ));
        }
        return deploy_success(response.body, resolved.created);
    }

    let first = client.deploy_zship(control_url, app, token, body)?;
    if first.status == 200 {
        return deploy_success(first.body, None);
    }

    if !auto_create || !should_resolve_or_create_after_deploy_failure(&first) {
        return Err(format!("Deploy failed (HTTP {}): {}", first.status, first.body));
    }

    let resolved = resolve_or_create_app(client, control_url, token, app, Some(first.status), true)?;
    let retry = client.deploy_zship(control_url, &resolved.id, token, body)?;
    if retry.status != 200 {
        return Err(format!("Deploy failed (HTTP {}): {}", retry.status, retry.body));
    }
    deploy_success(retry.body, resolved.created)
}

fn should_resolve_or_create_after_deploy_failure(response: &ControlResponse) -> bool {
    response.status == 404
}

fn resolve_or_create_app<C: ControlClient>(
    client: &mut C,
    control_url: &str,
    token: &str,
    name: &str,
    deploy_status: Option<u16>,
    auto_create: bool,
) -> Result<ResolvedApp, String> {
    if let Some(id) = find_existing_app(client, control_url, token, name)? {
        return Ok(ResolvedApp { id, created: None });
    }

    if !auto_create {
        return Err(format!(
            "app `{name}` not found; remove --no-create to create it on first deploy"
        ));
    }

    let create = client.create_app(control_url, token, name)?;
    if create.status != 201 && create.status != 200 {
        let context = deploy_status
            .map(|status| format!("deploy returned HTTP {status}; "))
            .unwrap_or_default();
        return Err(format!(
            "{context}app auto-create failed (HTTP {}): {}",
            create.status, create.body
        ));
    }

    let id = parse_app_id(&create.body, "create app response")?;
    Ok(ResolvedApp {
        id: id.clone(),
        created: Some(CreatedApp {
            name: name.to_string(),
            id,
        }),
    })
}

fn find_existing_app<C: ControlClient>(
    client: &mut C,
    control_url: &str,
    token: &str,
    name: &str,
) -> Result<Option<String>, String> {
    let list = client.list_apps(control_url, token)?;
    if list.status != 200 {
        return Err(format!(
            "app lookup failed (HTTP {}): {}",
            list.status, list.body
        ));
    }
    find_app_id_by_name(&list.body, name)
}

fn deploy_success(body: String, created_app: Option<CreatedApp>) -> Result<DeployOutcome, String> {
    let deploy_hash = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|json| {
            json.get("deploy_hash")
                .and_then(|hash| hash.as_str())
                .map(|hash| hash.to_string())
        });
    Ok(DeployOutcome {
        deploy_hash,
        created_app,
    })
}

fn deploy_auto_create(args: &[String]) -> bool {
    !args.iter().any(|arg| arg == "--no-create")
}

fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23].iter().all(|&idx| bytes[idx] == b'-')
        && bytes.iter().enumerate().all(|(idx, byte)| {
            matches!(idx, 8 | 13 | 18 | 23) || byte.is_ascii_hexdigit()
        })
}

fn find_app_id_by_name(body: &str, name: &str) -> Result<Option<String>, String> {
    let json = serde_json::from_str::<serde_json::Value>(body)
        .map_err(|e| format!("parse app list response: {e}"))?;
    let apps = json
        .as_array()
        .ok_or_else(|| "parse app list response: expected array".to_string())?;
    for app in apps {
        if app.get("name").and_then(|n| n.as_str()) == Some(name) {
            return Ok(Some(parse_app_id_value(app, "app list response")?));
        }
    }
    Ok(None)
}

fn parse_app_id(body: &str, context: &str) -> Result<String, String> {
    let json = serde_json::from_str::<serde_json::Value>(body)
        .map_err(|e| format!("parse {context}: {e}"))?;
    parse_app_id_value(&json, context)
}

fn parse_app_id_value(json: &serde_json::Value, context: &str) -> Result<String, String> {
    json.get("id")
        .and_then(|id| id.as_str())
        .map(|id| id.to_string())
        .ok_or_else(|| format!("parse {context}: missing string id"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn print_usage() {
    eprintln!("zeroship — JavaScript runtime powered by V8 + io_uring");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  zeroship serve    <file> [--port=3000] [--workers=0]");
    eprintln!("                   Run a single JS file with the V8 runtime.");
    eprintln!("  zeroship deploy   <path-to-.zship> --app=<name> [--control=URL] [--token=PAT] [--no-create]");
    eprintln!("                   Upload a pre-built .zship to the control plane.");
    eprintln!("                   Token source: --token, ZEROSHIP_TOKEN, or zeroship login.");
    eprintln!("  zeroship login    [--auth-url=https://auth.zeroship.ai]");
    eprintln!("                   Sign in with OAuth Device Authorization Grant.");
    eprintln!("  zeroship whoami");
    eprintln!("                   Show the signed-in account.");
    eprintln!("  zeroship logout");
    eprintln!("                   Revoke and delete local CLI credentials.");
    eprintln!("  zeroship secret   set|list|rm  --app=<uuid> [--control=URL] [--token=PAT]");
    eprintln!("  zeroship var      set|list|rm  --app=<uuid> [--control=URL] [--token=PAT]");
    eprintln!();
    eprintln!("Builds go through @zeroship/vite-plugin. There is no `zeroship build`.");
}

fn exit_on_error(command: &str, result: Result<(), String>) {
    if let Err(e) = result {
        eprintln!("zeroship {command}: {e}");
        std::process::exit(1);
    }
}

/// Parse a named flag that accepts both `--flag=VALUE` and `--flag VALUE` forms.
/// Returns the raw string value, or `None` if the flag is absent.
///
/// `flag_name` must be the bare name including the leading `--` (e.g. `"--port"`).
/// Matching is exact: `"--port"` does NOT match `"--port-extra"`.
fn parse_flag(args: &[String], flag_name: &str) -> Option<String> {
    let prefix_eq = format!("{flag_name}=");
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(val) = arg.strip_prefix(&*prefix_eq) {
            return Some(val.to_string());
        }
        if arg == flag_name {
            // Space form: value is the NEXT argument.
            return iter.next().cloned();
        }
    }
    None
}

fn parse_flag_u16(args: &[String], flag_name: &str) -> Option<u16> {
    parse_flag(args, flag_name).and_then(|s| s.parse().ok())
}

fn parse_flag_usize(args: &[String], flag_name: &str) -> Option<usize> {
    parse_flag(args, flag_name).and_then(|s| s.parse().ok())
}

fn parse_flag_u64(args: &[String], flag_name: &str) -> Option<u64> {
    parse_flag(args, flag_name).and_then(|s| s.parse().ok())
}

/// Known flags accepted by `zeroship serve` (bare names, no `=`).
const SERVE_KNOWN_FLAGS: &[&str] = &[
    "--port",
    "--workers",
    "--cpu-limit",
    "--wall-timeout",
    "--heap-limit-mb",
];

/// Return `Err` if any `--flag` argument in `args[2..]` is not a known `serve` flag.
/// Positional args (no leading `--`) and the values after a space-separated flag
/// are left unchecked.
pub(crate) fn check_unknown_serve_flags(args: &[String]) -> Result<(), String> {
    // args[0] = binary, args[1] = "serve", args[2] = <file>; flags start at index 3.
    let mut iter = args.iter().skip(3);
    while let Some(arg) = iter.next() {
        if !arg.starts_with("--") {
            // positional arg — skip (also covers numeric values from space-form flags)
            continue;
        }
        // Strip any `=value` suffix so `--port=3000` matches `--port`.
        let flag_name = match arg.find('=') {
            Some(idx) => &arg[..idx],
            None => arg.as_str(),
        };
        if !SERVE_KNOWN_FLAGS.contains(&flag_name) {
            return Err(format!(
                "unknown flag `{flag_name}`; run `zeroship serve --help` or see the usage above"
            ));
        }
        // If this is the space form (no `=`), consume the next token as the value.
        if !arg.contains('=') {
            iter.next(); // skip the value token
        }
    }
    Ok(())
}

/// Legacy equals-only flag parser kept for callers that have not been migrated
/// to `parse_flag`. New code should use `parse_flag` / `parse_flag_u16` etc.
pub(crate) fn flag_str(args: &[String], prefix: &str) -> Option<String> {
    args.iter()
        .find(|a| a.starts_with(prefix))
        .and_then(|a| a.strip_prefix(prefix))
        .map(|s| s.to_string())
}

const MISSING_TOKEN_HINT: &str =
    "no API token found; run `zeroship login`, pass `--token=<PAT>`, or set ZEROSHIP_TOKEN";

pub(crate) fn resolve_bearer_token(args: &[String]) -> Result<String, String> {
    resolve_bearer_token_from(
        args,
        || std::env::var("ZEROSHIP_TOKEN").ok(),
        crate::auth::load_credentials,
    )
}

fn resolve_bearer_token_from<EnvToken, LoadCredentials>(
    args: &[String],
    env_token: EnvToken,
    load_credentials: LoadCredentials,
) -> Result<String, String>
where
    EnvToken: FnOnce() -> Option<String>,
    LoadCredentials: FnOnce() -> Result<auth::Credentials, String>,
{
    if let Some(token) = flag_str(args, "--token=").and_then(non_empty_token) {
        return Ok(token);
    }
    if let Some(token) = env_token().and_then(non_empty_token) {
        return Ok(token);
    }

    let creds = load_credentials().map_err(|e| format!("{MISSING_TOKEN_HINT}: {e}"))?;
    non_empty_token(creds.access_token).ok_or_else(|| MISSING_TOKEN_HINT.to_string())
}

fn non_empty_token(token: String) -> Option<String> {
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    // -----------------------------------------------------------------------
    // ISS-57: arg-parser robustness
    // -----------------------------------------------------------------------

    /// `--port=N` and `--port N` (space form) must both resolve to N.
    #[test]
    fn flag_u16_accepts_both_equals_and_space_forms() {
        // equals form
        let args = s(&["zeroship", "serve", "app.js", "--port=8080"]);
        assert_eq!(parse_flag_u16(&args, "--port"), Some(8080));

        // space form
        let args = s(&["zeroship", "serve", "app.js", "--port", "8080"]);
        assert_eq!(parse_flag_u16(&args, "--port"), Some(8080));

        // absent → None
        let args = s(&["zeroship", "serve", "app.js"]);
        assert_eq!(parse_flag_u16(&args, "--port"), None);
    }

    /// `--workers=N` and `--workers N` must both resolve to N.
    #[test]
    fn flag_usize_accepts_both_equals_and_space_forms() {
        let args = s(&["zeroship", "serve", "app.js", "--workers=4"]);
        assert_eq!(parse_flag_usize(&args, "--workers"), Some(4usize));

        let args = s(&["zeroship", "serve", "app.js", "--workers", "4"]);
        assert_eq!(parse_flag_usize(&args, "--workers"), Some(4usize));

        let args = s(&["zeroship", "serve", "app.js"]);
        assert_eq!(parse_flag_usize(&args, "--workers"), None);
    }

    /// An unrecognized `--flag` in the `serve` command must surface as an error,
    /// not be silently swallowed so the user gets the default instead.
    #[test]
    fn unknown_serve_flag_is_rejected() {
        // Typo: `--prot` instead of `--port`.
        let args = s(&["zeroship", "serve", "app.js", "--prot=9000"]);
        let result = check_unknown_serve_flags(&args);
        assert!(result.is_err(), "typo'd flag should be rejected");
        let msg = result.unwrap_err();
        assert!(msg.contains("--prot"), "error should name the unknown flag: {msg}");

        // Known flags are accepted.
        let args = s(&[
            "zeroship", "serve", "app.js",
            "--port=3000", "--workers=2", "--cpu-limit=500",
            "--wall-timeout=2000", "--heap-limit-mb=512",
        ]);
        assert!(check_unknown_serve_flags(&args).is_ok());
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum FakeCall {
        Deploy(String),
        List,
        Create(String),
    }

    #[derive(Default)]
    struct FakeControlClient {
        calls: Vec<FakeCall>,
        deploys: VecDeque<ControlResponse>,
        lists: VecDeque<ControlResponse>,
        creates: VecDeque<ControlResponse>,
    }

    impl FakeControlClient {
        fn with_deploy(mut self, status: u16, body: &str) -> Self {
            self.deploys.push_back(ControlResponse {
                status,
                body: body.to_string(),
            });
            self
        }

        fn with_list(mut self, status: u16, body: &str) -> Self {
            self.lists.push_back(ControlResponse {
                status,
                body: body.to_string(),
            });
            self
        }

        fn with_create(mut self, status: u16, body: &str) -> Self {
            self.creates.push_back(ControlResponse {
                status,
                body: body.to_string(),
            });
            self
        }
    }

    impl ControlClient for FakeControlClient {
        fn deploy_zship(
            &mut self,
            _control_url: &str,
            app: &str,
            _token: &str,
            _body: &[u8],
        ) -> Result<ControlResponse, String> {
            self.calls.push(FakeCall::Deploy(app.to_string()));
            self.deploys
                .pop_front()
                .ok_or_else(|| "unexpected deploy call".to_string())
        }

        fn list_apps(
            &mut self,
            _control_url: &str,
            _token: &str,
        ) -> Result<ControlResponse, String> {
            self.calls.push(FakeCall::List);
            self.lists
                .pop_front()
                .ok_or_else(|| "unexpected list call".to_string())
        }

        fn create_app(
            &mut self,
            _control_url: &str,
            _token: &str,
            name: &str,
        ) -> Result<ControlResponse, String> {
            self.calls.push(FakeCall::Create(name.to_string()));
            self.creates
                .pop_front()
                .ok_or_else(|| "unexpected create call".to_string())
        }
    }

    #[test]
    fn deploy_creates_missing_app_after_404_and_retries_by_id() {
        let missing_app = "33333333-3333-4333-8333-333333333333";
        let mut client = FakeControlClient::default()
            .with_deploy(404, r#"{"error":"app not found"}"#)
            .with_list(200, "[]")
            .with_create(
                201,
                r#"{"id":"11111111-1111-4111-8111-111111111111","name":"33333333-3333-4333-8333-333333333333"}"#,
            )
            .with_deploy(200, r#"{"deploy_hash":"sha256:abc"}"#);

        let outcome = deploy_archive(
            &mut client,
            "http://control.test",
            missing_app,
            "token",
            b"zship",
            true,
        )
        .expect("deploy should create and retry");

        assert_eq!(outcome.deploy_hash.as_deref(), Some("sha256:abc"));
        assert_eq!(
            outcome.created_app,
            Some(CreatedApp {
                name: missing_app.to_string(),
                id: "11111111-1111-4111-8111-111111111111".to_string(),
            })
        );
        assert_eq!(
            client.calls,
            vec![
                FakeCall::Deploy(missing_app.to_string()),
                FakeCall::List,
                FakeCall::Create(missing_app.to_string()),
                FakeCall::Deploy("11111111-1111-4111-8111-111111111111".to_string()),
            ]
        );
    }

    #[test]
    fn deploy_name_create_path_resolves_before_upload() {
        let mut client = FakeControlClient::default()
            .with_list(200, "[]")
            .with_create(
                201,
                r#"{"id":"22222222-2222-4222-8222-222222222222","name":"calendar"}"#,
            )
            .with_deploy(200, r#"{"deploy_hash":"sha256:def"}"#);

        let outcome = deploy_archive(
            &mut client,
            "http://control.test",
            "calendar",
            "token",
            b"zship",
            true,
        )
        .expect("name deploy should create and retry by id");

        assert_eq!(outcome.deploy_hash.as_deref(), Some("sha256:def"));
        assert_eq!(
            client.calls,
            vec![
                FakeCall::List,
                FakeCall::Create("calendar".to_string()),
                FakeCall::Deploy("22222222-2222-4222-8222-222222222222".to_string()),
            ]
        );
    }

    #[test]
    fn deploy_no_create_suppresses_missing_app_provisioning() {
        let missing_app = "44444444-4444-4444-8444-444444444444";
        let args = s(&[
            "zeroship",
            "deploy",
            "dist/app.zship",
            "--app=44444444-4444-4444-8444-444444444444",
            "--no-create",
        ]);
        assert!(!deploy_auto_create(&args));

        let mut client = FakeControlClient::default()
            .with_deploy(404, r#"{"error":"app not found"}"#);

        let err = deploy_archive(
            &mut client,
            "http://control.test",
            missing_app,
            "token",
            b"zship",
            deploy_auto_create(&args),
        )
        .expect_err("--no-create should keep the original deploy failure");

        assert!(err.contains("HTTP 404"), "{err}");
        assert_eq!(client.calls, vec![FakeCall::Deploy(missing_app.to_string())]);
    }

    #[test]
    fn deploy_existing_app_success_path_is_unchanged() {
        let mut client = FakeControlClient::default()
            .with_deploy(200, r#"{"deploy_hash":"sha256:existing"}"#);

        let outcome = deploy_archive(
            &mut client,
            "http://control.test",
            "11111111-1111-4111-8111-111111111111",
            "token",
            b"zship",
            true,
        )
        .expect("existing app deploy");

        assert_eq!(outcome.deploy_hash.as_deref(), Some("sha256:existing"));
        assert_eq!(outcome.created_app, None);
        assert_eq!(
            client.calls,
            vec![FakeCall::Deploy(
                "11111111-1111-4111-8111-111111111111".to_string()
            )]
        );
    }

    // -----------------------------------------------------------------------
    // Existing tests
    // -----------------------------------------------------------------------

    #[test]
    fn resolves_bearer_token_from_flag_env_then_credentials() {
        let args = vec![
            "zeroship".to_string(),
            "deploy".to_string(),
            "app.zship".to_string(),
            "--token=flag-token".to_string(),
        ];
        let token = resolve_bearer_token_from(
            &args,
            || panic!("env token should not be read when --token is present"),
            || panic!("credentials should not be read when --token is present"),
        )
        .expect("flag token");
        assert_eq!(token, "flag-token");

        let args = vec![
            "zeroship".to_string(),
            "deploy".to_string(),
            "app.zship".to_string(),
        ];
        let token = resolve_bearer_token_from(
            &args,
            || Some("env-token".to_string()),
            || panic!("credentials should not be read when ZEROSHIP_TOKEN is present"),
        )
        .expect("env token");
        assert_eq!(token, "env-token");

        let token = resolve_bearer_token_from(
            &args,
            || None,
            || {
                Ok(auth::Credentials {
                    access_token: "credential-token".to_string(),
                    refresh_token: "refresh-token".to_string(),
                    expires_at: u64::MAX,
                    auth_url: "http://auth.test".to_string(),
                    client_id: "zeroship-cli".to_string(),
                })
            },
        )
        .expect("credential token");
        assert_eq!(token, "credential-token");
    }

    #[test]
    fn missing_bearer_token_explains_login_and_token_flag() {
        let args = vec![
            "zeroship".to_string(),
            "deploy".to_string(),
            "app.zship".to_string(),
        ];
        let err = resolve_bearer_token_from(&args, || None, || Err("not signed in".to_string()))
            .expect_err("missing token should fail");

        assert!(err.contains("zeroship login"), "{err}");
        assert!(err.contains("--token=<PAT>"), "{err}");
    }
}
