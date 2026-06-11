//! zeroship CLI — serve, deploy, login, secret, var.
//!
//! Commands:
//!   zeroship serve   <file-or-dir> [--port=3000] [--workers=0]
//!   zeroship deploy  <path-to-.zship> --app=<id> [--control=URL] [--token=PAT]
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
        "Usage: zeroship deploy <path-to-.zship> --app=<name-or-id> [--control=http://localhost:9090] [--token=<PAT>]",
    );
    let app = flag_str(args, "--app=").expect("--app=<name-or-id> is required");
    let control_url = flag_str(args, "--control=")
        .or_else(|| std::env::var("ZEROSHIP_CONTROL_URL").ok())
        .unwrap_or_else(|| "http://localhost:9090".into());
    let token = resolve_bearer_token(args).unwrap_or_else(|e| {
        eprintln!("zeroship deploy: {e}");
        std::process::exit(1);
    });

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
            &format!("Authorization: Bearer {token}"),
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
    eprintln!("  zeroship deploy   <path-to-.zship> --app=<id> [--control=URL] [--token=PAT]");
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
