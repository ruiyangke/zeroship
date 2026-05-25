//! zeroship CLI — serve, deploy, secret, var, migrate.
//!
//! Commands:
//!   zeroship serve   <file-or-dir> [--port=3000] [--workers=0]
//!   zeroship deploy  <path-to-.zship> --app=<id> [--control=URL] [--key=KEY]
//!   zeroship migrate <subcommand>     # P5.5 PR 8 — migration tooling
//!
//! `build` and `inspect` were removed in the artifact-layout redesign —
//! the canonical build path is now `@zeroship/vite-plugin`, which emits
//! `.zship` archives. `deploy` uploads those archives directly to the
//! control plane.

use std::path::PathBuf;
use std::sync::Arc;

use zeroship_runtime::{ModuleEntry, NativePlugin};

mod migrate;
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
        "secret" => secrets::cmd_secret(&args),
        "var" => secrets::cmd_var(&args),
        "migrate" => migrate::cmd_migrate(&args),
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
    let port = flag_u16(args, "--port=").unwrap_or(3000);
    let workers: usize = flag_str(args, "--workers=")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let cpu_limit = flag_str(args, "--cpu-limit=")
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_millis);
    let wall_timeout = flag_str(args, "--wall-timeout=")
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_millis);
    // Dev default: 512 MB. Single-tenant dev apps routinely load big libraries
    // (LangChain + provider SDKs = ~100 MB by themselves). The production
    // worker's 128 MB default is sized for multi-tenant isolation, not for
    // single-process dev. CLI flag or ZEROSHIP_HEAP_LIMIT_MB overrides.
    let heap_limit_bytes = flag_str(args, "--heap-limit-mb=")
        .or_else(|| std::env::var("ZEROSHIP_HEAP_LIMIT_MB").ok())
        .and_then(|s| s.parse::<usize>().ok())
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

    eprintln!("[zeroship] Starting server on port {port}");

    // Opt-in db plugin: when DATABASE_URL is set, register the db plugin
    // so JS `zeroship.db.*` works in the dev path (e.g. `vite-plugin` spawns
    // `zeroship serve` with DATABASE_URL forwarded from `.env`).
    let mut plugins: Vec<Arc<dyn NativePlugin>> = Vec::new();
    if let Ok(url) = std::env::var("DATABASE_URL") {
        if !url.is_empty() {
            plugins.push(Arc::new(zeroship_plugin_db::DbPlugin::new(url)));
            eprintln!("[zeroship] db plugin registered (DATABASE_URL set)");
        }
    }

    // Storage plugin: always on in dev. Data lives under
    // `$ZEROSHIP_STORAGE_ROOT` or (default) `<cwd>/.zeroship/storage`. The
    // vite-plugin's scaffolded .gitignore already excludes `.zeroship/` so
    // uploads aren't checked into git.
    let storage_root: PathBuf = std::env::var_os("ZEROSHIP_STORAGE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".zeroship/storage"));
    plugins.push(Arc::new(zeroship_plugin_storage::StoragePlugin::new(storage_root.clone())));
    eprintln!("[zeroship] storage plugin registered (root={})", storage_root.display());

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
            zeroship_plugin_kv::KvPlugin::with_backend(
                Arc::new(zeroship_plugin_kv::Redis::new(url))
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
            zeroship_plugin_kv::KvPlugin::with_backend(Arc::new(backend))
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
// Schema discovery (Stage 5c): the runtime reads `default.schema`
// off the loaded entry module — no manifest-side resolver. A raw
// `.js` deploy with `export default { schema: {...} }` is enough; no
// JS-side resolver wiring required even if this CLI grows a
// `zeroship build` command later.
fn cmd_deploy(args: &[String]) {
    let input = args.get(2).expect(
        "Usage: zeroship deploy <path-to-.zship> --app=<name-or-id> [--control=http://localhost:9090] [--key=<master-key>]",
    );
    let app = flag_str(args, "--app=").expect("--app=<name-or-id> is required");
    let control_url = flag_str(args, "--control=")
        .or_else(|| std::env::var("ZEROSHIP_CONTROL_URL").ok())
        .unwrap_or_else(|| "http://localhost:9090".into());
    let master_key = flag_str(args, "--key=")
        .or_else(|| std::env::var("ZEROSHIP_MASTER_KEY").ok())
        .unwrap_or_default();

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

    let deploy_url = format!("{control_url}/api/apps/{app}/deploy");
    let response = std::process::Command::new("curl")
        .args([
            "-s",
            "-w",
            "\n%{http_code}",
            "-X",
            "POST",
            &deploy_url,
            "-H",
            &format!("Authorization: Bearer {master_key}"),
            "-H",
            "Content-Type: application/x-zship",
            "--data-binary",
            "@-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(&body).ok();
            }
            child.wait_with_output()
        });

    match response {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let lines: Vec<&str> = stdout.trim().rsplitn(2, '\n').collect();
            let (status_str, body_text) = if lines.len() == 2 {
                (lines[0], lines[1])
            } else {
                (lines[0], "")
            };
            let status: u16 = status_str.parse().unwrap_or(0);
            if status == 200 {
                eprintln!("Deployed successfully!");
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(body_text) {
                    if let Some(hash) = json.get("deploy_hash").and_then(|h| h.as_str()) {
                        eprintln!("  deploy_hash: {hash}");
                    }
                }
            } else {
                eprintln!("Deploy failed (HTTP {status}): {body_text}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Failed to run curl: {e}");
            eprintln!("Make sure curl is installed and the control plane is running.");
            std::process::exit(1);
        }
    }
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
    eprintln!("  zeroship deploy   <path-to-.zship> --app=<id> [--control=URL] [--key=KEY]");
    eprintln!("                   Upload a pre-built .zship to the control plane.");
    eprintln!("  zeroship secret   set|list|rm  --app=<uuid>");
    eprintln!("  zeroship var      set|list|rm  --app=<uuid>");
    eprintln!("  zeroship migrate  scan-mask-usage [--path=<dir>] [--format=text|json]");
    eprintln!("                   P5.5 migration aid — flag pre-masking field reads.");
    eprintln!();
    eprintln!("Builds go through @zeroship/vite-plugin. There is no `zeroship build`.");
}

fn flag_u16(args: &[String], prefix: &str) -> Option<u16> {
    args.iter()
        .find(|a| a.starts_with(prefix))
        .and_then(|a| a.strip_prefix(prefix))
        .and_then(|s| s.parse().ok())
}

pub(crate) fn flag_str(args: &[String], prefix: &str) -> Option<String> {
    args.iter()
        .find(|a| a.starts_with(prefix))
        .and_then(|a| a.strip_prefix(prefix))
        .map(|s| s.to_string())
}
