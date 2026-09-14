//! zeroship CLI - serve, deploy, migrate, config, login, secret, var, dev.
//!
//! Commands:
//!   zeroship serve   <file-or-dir> [--port=3000] [--workers=0]
//!   zeroship deploy  [<path-to-.zship>] [--app=<id>] [--app-name=<name>] [--control=URL] [--token=TOKEN] [--no-create] [--config=PATH] [--env=NAME]
//!   zeroship migrate [<path-to-migrations.ir.json>] [--app=<id>] [--app-name=<name>]
//!                    [--control=URL] [--token=TOKEN] [--config=PATH] [--env=NAME] [--yes]
//!   zeroship config show [--config=PATH] [--env=NAME]
//!   zeroship config path [--config=PATH]
//!   zeroship login [--control=URL] [--config=PATH] [--env=NAME]
//!   zeroship dev init [--secrets-dir=PATH] [--env-file=PATH]
//!
//! `build` and `inspect` were removed in the artifact-layout redesign —
//! the canonical build path is now `@zeroship/vite-plugin`, which emits
//! `.zship` archives. `deploy` uploads those archives directly to the
//! control plane. Deploy/secret/var commands read the bearer token from
//! `--token=TOKEN`, `ZEROSHIP_TOKEN`, or credentials saved by `zeroship login`.

use std::path::PathBuf;
use std::sync::Arc;

use zeroship_core::AppId;
use zeroship_runtime::{ModuleEntry, NativePlugin};

mod auth;
mod dev;
mod migrate;
mod organizations;
mod parent_death;
mod project_config;
mod project_keys;
mod secrets;

zeroship_core::declare_env_consumer!(
    /// The creator CLI's own environment surface.
    ///
    /// `zeroship` runs on a creator's machine, not as a service an operator
    /// configures, so its zeroship-owned names are DECLARED (class `cli`)
    /// rather than generated: they get a recorded read site and no server
    /// TOML overlay. Names it reads that somebody else owns - `HOME`,
    /// `XDG_CONFIG_HOME`, `DATABASE_URL`, `PATH` - stay class `external`.
    pub(crate) ZeroshipCliConsumer,
    target = "zeroship",
    scope = "cli");

fn main() {
    // FIRST, before the port probe and before any state dir is opened: from
    // here on this process holds resources whose owner must not outlive the dev
    // server that spawned it. See `parent_death` for what happened when it did.
    parent_death::arm_from_env();

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
        "migrate" => exit_on_error("migrate", migrate::cmd_migrate(&args)),
        "config" => exit_on_error("config", project_config::cmd_config(&args)),
        "login" => exit_on_error("login", auth::cmd_login(&args)),
        "logout" => exit_on_error("logout", auth::cmd_logout()),
        "whoami" => exit_on_error("whoami", auth::cmd_whoami()),
        "dev" => exit_on_error("dev", dev::cmd_dev(&args)),
        "organization" => exit_on_error("organization", organizations::cmd_organization(&args)),
        "secret" => secrets::cmd_secret(&args),
        "var" => secrets::cmd_var(&args),
        _ => print_usage(),
    }
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

fn cmd_serve(args: &[String]) {
    // THE ONE PLACE THE DEV RELAXATION IS STATED. `zeroship serve` is the
    // single-process dev-tier runtime by identity - it is what
    // `@zeroship/vite-plugin` spawns (`sdks/vite-plugin/src/dev-server.ts:970`)
    // with `ZEROSHIP_DEV=1` (`dev-server.ts:939`), and the only vector on
    // which SQLite is an accepted `env.db` backend.
    //
    // The runtime's SSRF guard no longer reads the environment at all; it
    // answers what this call stated (`crates/zeroship-runtime/src/transport/
    // ssrf.rs`, `dev_mode_enabled`). `zeroship-worker` makes no such call, so
    // a `ZEROSHIP_DEV=1` that leaks into a production worker's environment
    // changes nothing there - the same standard the worker already applies to
    // its database backend at `crates/zeroship-worker/src/main.rs:146-147`.
    //
    // It runs before anything builds a `cyper::Client` or a `DevAuthSettings`,
    // both of which resolve the mode once and keep the answer
    // (`transport/client.rs`, `core/serve.rs:1749`).
    zeroship_runtime::set_dev_mode(zeroship_runtime::dev_mode_from_process_env());

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
    let dev_entry_loader = parse_flag(args, "--dev-entry-loader");
    // Dev default: 512 MB. Single-tenant dev apps routinely load big libraries
    // (LangChain + provider SDKs = ~100 MB by themselves). The production
    // worker's 128 MB default is sized for multi-tenant isolation, not for
    // single-process dev. CLI flag or ZEROSHIP_HEAP_LIMIT_MB overrides.
    let heap_limit_bytes = parse_flag_usize(args, "--heap-limit-mb")
        .or_else(|| {
            zeroship_core::declared_env!(cli, "ZEROSHIP_HEAP_LIMIT_MB", crate::ZeroshipCliConsumer)
                .and_then(|s| s.parse().ok())
        })
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

    // Forward process env to the V8 runtime so `process.env.FOO` works in JS.
    // Important for dev: the vite-plugin sets ZEROSHIP_ENTRY / ZEROSHIP_VITE_WS
    // in the spawned child env. Without this, `process.env` in V8 is empty.
    // Class `creator`, not `cli`: the names in this snapshot belong to the
    // app being served, not to the platform, so there is nothing here for the
    // platform to enumerate. This is the ONE legitimate whole-environment read.
    let mut env_vars: std::collections::HashMap<String, String> =
        zeroship_core::read_process_env_snapshot!(crate::ZeroshipCliConsumer)
            .into_iter()
            .collect();
    // The single-app dev host owns its namespace just as the worker does.
    let dev_app_id = resolve_dev_app_id(&mut env_vars).unwrap_or_else(|error| {
        eprintln!("zeroship serve: {error}");
        std::process::exit(2);
    });

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
    // Say NOT READABLE, not just "local". The producers below are four writers
    // into this meter and there are ZERO readers in dev - no flush, no snapshot,
    // no endpoint. "local accumulation only" is true but reads like "your usage
    // is tracked here", and a creator who wants to see it locally goes looking
    // for a surface that does not exist. Deployed is where usage becomes
    // visible; see docs/pilot/e2e-scenarios.md row 14.
    eprintln!(
        "[zeroship] metering on (dev: counters accumulate but are NOT READABLE - \
         no flush, no snapshot API; usage is only observable on a deployed app)"
    );

    if let Some(url) =
        zeroship_core::declared_env!(external, "DATABASE_URL", crate::ZeroshipCliConsumer)
    {
        if !url.is_empty() {
            // The dev vector's composition point. One `DbService`, built before
            // the runtime exists, owning the validated configuration and the
            // plugin prototype the runtime clones — the same shape the worker
            // uses, so `zeroship serve` and a deployed app resolve `env.db`
            // through identical machinery.
            //
            // A URL naming no supported backend fails HERE, with the same
            // exit(2) the invalid-`ZEROSHIP_STORAGE_URL` arm below already
            // uses, rather than surfacing inside the creator's first query.
            let service = match zeroship_data_orm::connection::ConnectionFactory::for_url(&url)
                .and_then(|connection| {
                    zeroship_data_v8::service::DbService::new(
                        zeroship_data_v8::service::DbServiceConfig {
                            project_keys: project_keys::load(
                                std::path::Path::new(".zeroship/private"),
                                &dev_app_id,
                            ).map_err(|error| zeroship_data_orm::error::DbError::config("local_project_key", error))?,
                            connection,
                            cdc_relay: None,
                            meter: Some(Arc::clone(&dev_meter)),
                        },
                    )
                }) {
                Ok(service) => service,
                Err(e) => {
                    eprintln!("[zeroship] invalid DATABASE_URL: {e}");
                    std::process::exit(2);
                }
            };
            plugins.push(service.plugin());
            eprintln!("[zeroship] db plugin registered (DATABASE_URL set)");
        }
    }

    // Storage plugin: always on in dev. `$ZEROSHIP_STORAGE_URL` selects the
    // backend through the SAME parser the worker uses (`--storage-url`): a
    // bare path or `file://…` → LocalFs (default `<cwd>/.zeroship/storage`);
    // `s3://…` → the S3 backend (creds from the AWS env vars). The
    // vite-plugin's scaffolded .gitignore already excludes `.zeroship/`.
    let storage_url =
        zeroship_core::declared_env!(cli, "ZEROSHIP_STORAGE_URL", crate::ZeroshipCliConsumer)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "file://.zeroship/storage".to_string());
    let storage_cfg = match zeroship_storage::StorageBackendConfig::parse(&storage_url) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[zeroship] invalid ZEROSHIP_STORAGE_URL: {e}");
            std::process::exit(2);
        }
    };
    let storage_kind = storage_cfg.kind();
    match zeroship_storage::StorageStore::open(&storage_cfg) {
        Ok(backend) => {
            plugins.push(Arc::new(zeroship_storage_v8::StorageBinding::new(
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

    // Resolve the host's runtime configuration before constructing storage.
    let kv_config = match zeroship_core::declared_env!(
        cli,
        "ZEROSHIP_KV_CONFIG_FILE",
        crate::ZeroshipCliConsumer
    ) {
        Some(path) if !path.is_empty() => {
            let contents =
                zeroship_core::config::secrets::read_secret_file(&path).unwrap_or_else(|error| {
                    eprintln!("[zeroship] KV configuration: {error}");
                    std::process::exit(1);
                });
            zeroship_kv::KvConfig::from_toml(&contents).unwrap_or_else(|error| {
                eprintln!("[zeroship] KV configuration: {error}");
                std::process::exit(1);
            })
        }
        _ => zeroship_kv::KvConfig::Redb {
            path: zeroship_core::declared_env_os!(
                cli,
                "ZEROSHIP_KV_PATH",
                crate::ZeroshipCliConsumer
            )
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".zeroship/kv.redb")),
        },
    };
    let kv_store = zeroship_kv::KvStore::open(&kv_config).unwrap_or_else(|error| {
        eprintln!("[zeroship] kv backend init failed: {error}");
        std::process::exit(1);
    });
    eprintln!(
        "[zeroship] kv binding registered (backend={})",
        kv_config.kind()
    );
    plugins.push(Arc::new(zeroship_kv_v8::KvBinding::new(
        kv_store,
        Some(Arc::clone(&dev_meter)),
    )));

    let workflow_db_path: PathBuf = zeroship_core::declared_env_os!(
        cli,
        "ZEROSHIP_WORKFLOW_SQLITE_PATH",
        crate::ZeroshipCliConsumer
    )
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".zeroship/workflows.sqlite"));
    if let Some(parent) = workflow_db_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!(
                "[zeroship] workflows: failed to create dir '{}': {e}",
                parent.display()
            );
            std::process::exit(1);
        }
    }
    let workflow_peer_plugins = plugins.clone();
    let workflow_binding = zeroship_workflow_v8::WorkflowBinding::dev_sqlite(
        &workflow_db_path,
        modules.clone(),
        env_vars.clone(),
        workflow_peer_plugins,
    )
    .unwrap_or_else(|e| {
        eprintln!(
            "[zeroship] workflows: failed to open sqlite at '{}': {e}",
            workflow_db_path.display()
        );
        std::process::exit(1);
    });
    plugins.push(Arc::new(workflow_binding));
    eprintln!(
        "[zeroship] workflows binding registered (sqlite; path={})",
        workflow_db_path.display()
    );

    zeroship_runtime::serve::start_server(
        modules,
        zeroship_runtime::serve::ServerOptions {
            port,
            workers,
            cpu_limit,
            wall_timeout,
            heap_limit_bytes,
            dev_entry_loader,
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
    // Before ANY flag is read, so a typo is reported as a typo rather than as
    // the downstream symptom of its default. Mirrors cmd_serve.
    if let Err(e) = check_unknown_deploy_flags(args) {
        eprintln!("zeroship deploy: {e}");
        std::process::exit(1);
    }
    let (config, resolved, app, target, control_url, input) =
        deploy_target(args).unwrap_or_else(|e| {
            eprintln!("zeroship deploy: {e}");
            std::process::exit(1);
        });
    let token = resolve_bearer_token(args).unwrap_or_else(|e| {
        eprintln!("zeroship deploy: {e}");
        std::process::exit(1);
    });
    let auto_create = deploy_auto_create(args);
    let declared_secrets = resolved
        .as_ref()
        .and_then(|cfg| cfg.get("secrets"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();

    project_config::print_provenance("deploy", &[("app", &app), ("control", &control_url)]);
    let app_source = app.source.clone();
    let (app, control_url) = (app.value, control_url.value);

    let input_path = input;
    let body = std::fs::read(&input_path).unwrap_or_else(|e| {
        eprintln!("Failed to read {}: {e}", input_path.display());
        eprintln!("Run `vite build` (with @zeroship/vite-plugin) to produce a .zship archive.");
        std::process::exit(1);
    });

    // BEFORE the upload, not after it. A reminder printed after a successful
    // deploy names a step the deploy has already made it too late to take in
    // order; control now REFUSES a deploy whose migrations have not been
    // applied, so this line is what a creator reads on the way to that 409
    // rather than a footnote under a green result.
    print_migrate_reminder(&app, &control_url, resolved.as_ref());

    eprintln!(
        "Deploying {} ({:.1}KB) to {control_url}/api/apps/{app}/deploy...",
        input_path.display(),
        body.len() as f64 / 1024.0,
    );

    let mut client = CurlControlClient;
    match deploy_archive(
        &mut client,
        &control_url,
        &target,
        &token,
        &body,
        &declared_secrets,
        auto_create,
    ) {
        Ok(outcome) => {
            if let Some(created) = outcome.created_app {
                eprintln!("created app {} ({})", created.name, created.id.as_str());
                record_created_app(
                    config.as_ref(),
                    flag_str(args, "--env=").as_deref(),
                    &app_source,
                    &created.id,
                );
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

type DeployTarget = (
    Option<project_config::ProjectConfig>,
    Option<project_config::Resolved>,
    project_config::Sourced,
    AppTarget,
    project_config::Sourced,
    PathBuf,
);

/// Resolve the deploy target and the artifact to upload.
///
/// The positional `.zship` path becomes OPTIONAL here: with a config file the
/// packer's own output path is already written down, so `zeroship deploy` with
/// zero arguments is the whole point. Without one, nothing changes.
fn deploy_target(args: &[String]) -> Result<DeployTarget, String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot read the working directory: {e}"))?;
    let file = project_config::locate(args, &cwd)?;
    let config = file
        .as_deref()
        .map(project_config::ProjectConfig::load)
        .transpose()?;
    let resolved = match (&config, flag_str(args, "--env=")) {
        (Some(cfg), env) => Some(cfg.resolve(env.as_deref())?),
        (None, Some(env)) => {
            return Err(format!(
                "--env={env} needs a {} in this directory to read the environment from",
                project_config::CONFIG_FILENAME
            ))
        }
        (None, None) => None,
    };

    let (app, target) = resolve_deploy_app(args, resolved.as_ref())?;
    let control_url = project_config::resolve_control(args, resolved.as_ref())?;

    let input = match args.get(2).filter(|a| !a.starts_with("--")) {
        Some(p) => PathBuf::from(p),
        None => match resolved.as_ref() {
            Some(cfg) => cfg.require_path("build.output")?,
            None => {
                return Err(
                    "Usage: zeroship deploy <path-to-.zship> --app=<id> \
                     [--app-name=<name>] [--control=<url>] [--token=<token>] [--no-create]\n\
                     With a zeroship.jsonc the path, app and control all come from the file \
                     and `zeroship deploy` takes no arguments."
                        .to_string(),
                )
            }
        },
    };
    Ok((config, resolved, app, target, control_url, input))
}

/// Decide WHAT the deploy was pointed at, from which input carried the value.
///
/// Four inputs, two kinds, and no inspection of the value in choosing between
/// them:
///
/// - `--app-name=<name>` - an explicit routing label. This is the flag the
///   deleted `is_uuid` guess used to synthesise from a name-shaped string.
/// - `--app=<id>` or the file's `app` member - an identity, refused unless it
///   parses as one ([`app_id_or_refuse`]).
/// - the file's `name` member, as the LAST resort and only when the file is
///   present. That fallback is not a guess: `name` is a different config member
///   with its own [`project_config::Source`], and it is what makes a brand-new
///   project deployable before it has an id to write down.
///
/// The `name` fallback's original note is kept verbatim, because it records why
/// this is a `deploy`-only affordance:
///
/// > `app` FALLS BACK TO `name` HERE AND NOWHERE ELSE, and only when the file
/// > is present. A brand-new project has no app id: `app` is
/// > deliberately not a required key, `deploy` already resolves-or-creates by
/// > name, and the id it mints is reported for the file. Doing this in
/// > `migrate` would let a typo'd name migrate a fresh empty app while the
/// > real one stayed broken. Secret and var commands require an app identity.
fn resolve_deploy_app(
    args: &[String],
    resolved: Option<&project_config::Resolved>,
) -> Result<(project_config::Sourced, AppTarget), String> {
    if let Some(name) = parse_flag(args, "--app-name") {
        if parse_flag(args, "--app").is_some() {
            return Err(
                "--app and --app-name both name a target; pass one. --app takes the \
                 app's ID (the identity), --app-name its routing label."
                    .to_string(),
            );
        }
        return Ok((
            project_config::Sourced {
                value: name.clone(),
                source: project_config::Source::Flag("--app-name"),
            },
            AppTarget::Name(name),
        ));
    }

    match project_config::resolve_value(args, "--app", None, None, resolved, "app", None) {
        Ok(sourced) => {
            let id = app_id_or_refuse(&sourced.value)?;
            Ok((sourced, AppTarget::Id(id)))
        }
        Err(e) => match resolved.and_then(|r| r.str("name")) {
            Some(name) if deploy_auto_create(args) => Ok((
                project_config::Sourced {
                    value: name.to_string(),
                    source: project_config::Source::FileMember("name"),
                },
                AppTarget::Name(name.to_string()),
            )),
            _ => Err(e),
        },
    }
}

/// Report where to put an id created from the file's `name` fallback.
///
/// WRITEBACK IS DELIBERATELY TINY: one field and one code path.
/// Not `control` - a `--control=` typo becoming permanent is worse than typing
/// it twice. An existing config `app` or an explicit `--app` is never a
/// writeback target, even if that target is auto-created; only a missing `app`
/// that fell back to `name` reaches the file.
///
/// A missing root member is appended through the JSONC CST. Existing values
/// still use their original source span, but the source guard below means that
/// replacement is not reachable from an auto-create writeback.
fn record_created_app(
    config: Option<&project_config::ProjectConfig>,
    environment: Option<&str>,
    app_source: &project_config::Source,
    id: &AppId,
) {
    let Some(config) = config else {
        eprintln!("  record it with --app={} on the next command, or in a {} (see docs/reference/project-config.md)",
            id.as_str(),
            project_config::CONFIG_FILENAME);
        return;
    };
    // An `--env=` deploy's app id belongs to THAT environment, and the splice
    // only ever touches the top-level member. Writing it at the root would put
    // staging's id where every un-flagged command reads production's - which is
    // the cross-targeting the non-inheritable rule exists to prevent, arriving
    // through the writeback door.
    if let Some(env) = environment {
        eprintln!(
            "  add this under environments.{env} in {}:\n    \"app\": \"{}\",",
            config.path.display(),
            id.as_str()
        );
        return;
    }
    if app_source != &project_config::Source::FileMember("name") {
        return;
    }
    match config.write_app(id) {
        Ok(()) => {
            eprintln!("  wrote app id into {}", config.path.display());
        }
        Err(e) => eprintln!("  could not record the app id ({e}); add it by hand: \"app\": \"{}\",", id.as_str()),
    }
}

/// BEFORE the upload, name the migrate step when the selected project has
/// migrations to apply.
///
/// DELIBERATELY A CLIENT-SIDE HINT, and a weak one. It fires on the presence of
/// the build's own artifact beside the selected project config, including when
/// deploy was invoked from another directory. It does not know whether the
/// app's migrations are already applied. It cannot tell you that you FORGOT;
/// only that there is something to run.
///
/// THE EXPENSIVE HALF NOW EXISTS, which is why this prints first. The deploy
/// handler compares the artifact's `runtime_descriptor.hash` against the
/// descriptor recorded on the app's newest applied migration and answers 409
/// `schema_not_applied` when they disagree
/// (`Registry::set_deploy_with_manifest`). This line is the warning on the way
/// in; that refusal is the guarantee. Printing it after a 200, as this used to,
/// named a step the deploy had already made it too late to take in order.
fn print_migrate_reminder(
    app: &str,
    control_url: &str,
    resolved: Option<&project_config::Resolved>,
) {
    // The reminder now reads the SAME `migrations.out` the build wrote to.
    // Before this it read a hardcoded const, so a project that moved its
    // generated dir got silence from the one hint it had.
    let Some(out) = resolved.and_then(|r| r.resolve_path("migrations.out")) else {
        return;
    };
    let ir = out.join(migrate::IR_FILENAME);
    if !ir.is_file() {
        return;
    }
    eprintln!();
    eprintln!("This app has committed migrations. Deploy does NOT apply them:");
    eprintln!("  zeroship migrate --app={app} --control={control_url}");
    eprintln!("Until you do, this deploy is REFUSED with 409 schema_not_applied.");
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ControlResponse {
    pub(crate) status: u16,
    pub(crate) body: String,
}

trait ControlClient {
    fn deploy_zship(
        &mut self,
        control_url: &str,
        app: &AppId,
        token: &str,
        body: &[u8],
    ) -> Result<ControlResponse, String>;

    fn list_apps(&mut self, control_url: &str, token: &str) -> Result<ControlResponse, String>;

    fn list_secrets(
        &mut self,
        control_url: &str,
        app: &AppId,
        token: &str,
    ) -> Result<ControlResponse, String>;

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
        app: &AppId,
        token: &str,
        body: &[u8],
    ) -> Result<ControlResponse, String> {
        let deploy_url = format!("{control_url}/api/apps/{}/deploy", app.as_str());
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

    fn list_secrets(
        &mut self,
        control_url: &str,
        app: &AppId,
        token: &str,
    ) -> Result<ControlResponse, String> {
        let url = format!("{control_url}/api/apps/{}/secrets", app.as_str());
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

pub(crate) fn run_curl(
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
    id: AppId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedApp {
    id: AppId,
    created: Option<CreatedApp>,
}

/// Upload `body` to the app `app` names.
///
/// TWO ARMS, ONE PER KIND OF TARGET, and the kind is decided by the caller.
/// Auto-create belongs to the NAME arm alone: creating an app is the answer to
/// "push this code somewhere new", which is a statement about a label. An id
/// that resolves to nothing is a wrong id, and minting an app whose name is
/// that id - which is exactly what the id arm used to do on a 404 - deploys the
/// creator's code to an app they have never heard of and leaves the real one
/// untouched.
fn deploy_archive<C: ControlClient>(
    client: &mut C,
    control_url: &str,
    app: &AppTarget,
    token: &str,
    body: &[u8],
    declared_secrets: &[String],
    auto_create: bool,
) -> Result<DeployOutcome, String> {
    let (id, created) = match app {
        AppTarget::Name(name) => {
            let resolved = resolve_or_create_app(client, control_url, token, name, auto_create)?;
            (resolved.id, resolved.created)
        }
        AppTarget::Id(id) => (id.clone(), None),
    };

    warn_for_missing_declared_secrets(client, control_url, &id, token, declared_secrets);
    let response = client.deploy_zship(control_url, &id, token, body)?;
    if response.status != 200 {
        return Err(deploy_failure_message(app, &response));
    }
    deploy_success(response.body, created)
}

/// The deploy failure a creator reads, with the id arm's 404 spelled out.
///
/// A 404 on an ID is the one failure whose obvious remedy - "create it, then" -
/// is the wrong-target bug, so the message says why the CLI did not take it
/// rather than leaving the creator to reach for `--app-name` by accident.
fn deploy_failure_message(app: &AppTarget, response: &ControlResponse) -> String {
    let base = format!(
        "Deploy failed (HTTP {}): {}",
        response.status, response.body
    );
    match app {
        AppTarget::Id(id) if response.status == 404 => format!(
            "{base}\n  \
             `{}` is an app ID, and an app is never created from one: that \
             would mint a new app whose NAME is the id and deploy there, \
             leaving the app you meant untouched. Check the id, or address the \
             app by its routing label with --app-name=<name>.",
            id.as_str()
        ),
        _ => base,
    }
}

fn warn_for_missing_declared_secrets<C: ControlClient>(
    client: &mut C,
    control_url: &str,
    app: &AppId,
    token: &str,
    declared: &[String],
) {
    let app_text = app.as_str();
    if declared.is_empty() {
        return;
    }
    let response = match client.list_secrets(control_url, app, token) {
        Ok(response) => response,
        Err(error) => {
            eprintln!(
                "zeroship deploy: warning: could not check declared secrets for app {app_text}: {error}"
            );
            return;
        }
    };
    if response.status != 200 {
        eprintln!(
            "zeroship deploy: warning: could not check declared secrets for app {app_text} \
             (HTTP {}): {}",
            response.status, response.body
        );
        return;
    }
    let Some(present) = parse_secret_names(&response.body) else {
        eprintln!(
            "zeroship deploy: warning: could not parse the secret list for app {app_text}: {}",
            response.body
        );
        return;
    };
    for name in declared {
        if !present.contains(name) {
            eprintln!(
                "zeroship deploy: warning: zeroship.jsonc declares `{name}`, but \
                 `zeroship secret list` does not show it"
            );
        }
    }
}

fn parse_secret_names(body: &str) -> Option<std::collections::HashSet<String>> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    json.get("secrets")?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect()
}

/// Resolve a NAME to an id, creating the app on first deploy.
///
/// `deploy_status` is gone with the id arm's create-on-404 retry: this is now
/// reached only from [`AppTarget::Name`], where no deploy has been attempted
/// yet, so there is never a prior status to report.
fn resolve_or_create_app<C: ControlClient>(
    client: &mut C,
    control_url: &str,
    token: &str,
    name: &str,
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
        return Err(format!(
            "app auto-create failed (HTTP {}): {}",
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
) -> Result<Option<AppId>, String> {
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

fn resolve_dev_app_id(
    env_vars: &mut std::collections::HashMap<String, String>,
) -> Result<AppId, String> {
    match env_vars.get("APP_ID") {
        Some(raw) => AppId::parse(raw).map_err(|error| format!("invalid APP_ID: {error}")),
        None => {
            let id = zeroship_core::app_id::local_dev_app_id();
            env_vars.insert("APP_ID".to_string(), id.as_str().to_string());
            Ok(id)
        }
    }
}

/// A target selected explicitly by stable identity or by routing name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppTarget {
    /// Addressed by identity: deployed to (or migrated) directly, never created.
    Id(AppId),
    /// Addressed by the hostname label: resolved, and for `deploy` created on
    /// first push.
    Name(String),
}

/// Parse the canonical identity accepted by `--app`.
pub(crate) fn app_id_or_refuse(value: &str) -> Result<AppId, String> {
    AppId::parse(value).map_err(|_| {
        format!(
            "--app takes an app ID and `{value}` is not one.\n  \
         An app's NAME is a routing label: it is the hostname subdomain the app \
         is served on, and it can change.\n  \
         An app's ID is the identity every deploy, migration, secret and log \
         lookup keys on. It is spelled `app_<id>`.\n  \
         To address this app by its name instead, pass --app-name={value}."
        )
    })
}

fn find_app_id_by_name(body: &str, name: &str) -> Result<Option<AppId>, String> {
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

pub(crate) fn parse_app_id(body: &str, context: &str) -> Result<AppId, String> {
    let json = serde_json::from_str::<serde_json::Value>(body)
        .map_err(|e| format!("parse {context}: {e}"))?;
    parse_app_id_value(&json, context)
}

fn parse_app_id_value(json: &serde_json::Value, context: &str) -> Result<AppId, String> {
    let raw = json
        .get("id")
        .and_then(|id| id.as_str())
        .ok_or_else(|| format!("parse {context}: missing string id"))?;
    AppId::parse(raw).map_err(|error| format!("parse {context}: invalid app id: {error}"))
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
    eprintln!("  zeroship deploy   [<path-to-.zship>] [--app=<id>] [--app-name=<name>] [--control=URL] [--token=TOKEN] [--no-create] [--config=PATH] [--env=NAME]");
    eprintln!("                   Upload a pre-built .zship to the control plane.");
    eprintln!("                   --app takes the app's ID; --app-name its routing label.");
    eprintln!("                   Token source: --token, ZEROSHIP_TOKEN, or zeroship login.");
    eprintln!("  zeroship migrate  [<path-to-migrations.ir.json>] [--app=<id>] [--app-name=<name>] [--control=URL] [--token=TOKEN] [--config=PATH] [--env=NAME] [--yes]");
    eprintln!("                   Apply the app's committed migrations to its DEPLOYED database.");
    eprintln!("                   Without a path, reads <migrations.out>/migrations.ir.json from");
    eprintln!("                   zeroship.jsonc; without either, the command errors.");
    eprintln!("                   An app that uses env.db needs this after deploy,");
    eprintln!("                   or its first database call fails with a missing-role error.");
    eprintln!("  zeroship config   show [--config=PATH] [--env=NAME]");
    eprintln!("  zeroship config   path [--config=PATH]");
    eprintln!("                   Show the resolved project config or its selected path.");
    eprintln!("  zeroship login    [--control=URL] [--provider=platform|supabase] [--config=PATH] [--env=NAME]");
    eprintln!("                   Sign in with the platform device flow.");
    eprintln!("  zeroship whoami");
    eprintln!("                   Show the signed-in account.");
    eprintln!("  zeroship logout");
    eprintln!(
        "                   Delete local CLI credentials; no server-side token revocation is performed."
    );
    eprintln!("  zeroship dev init [--secrets-dir=PATH] [--env-file=PATH]");
    eprintln!("                   Provision stable, strong local platform secrets.");
    eprintln!("  zeroship organization create|list|show|use|members|invite|revoke|join|role|remove|transfer|projects");
    eprintln!("                   The organization owns your projects and is the billed party.");
    eprintln!("                   `use <org_...>` records which one, so the other subcommands");
    eprintln!("                   need --organization= only to override it.");
    eprintln!("  zeroship secret   set|list|rm|expose|unexpose|expose-list  --app=<id> [--control=URL] [--token=TOKEN]");
    eprintln!("                   Encrypted at rest. Always readable as env.KEY; reaches");
    eprintln!("                   process.env (where any npm dependency can read it) only");
    eprintln!("                   via `secret set KEY=v --expose` or `secret expose KEY`.");
    eprintln!("  zeroship var      set|list|rm  --app=<id> [--control=URL] [--token=TOKEN]");
    eprintln!("                   PLAINTEXT config, always in both env and process.env.");
    eprintln!("                   Never put a credential in a var; use a secret.");
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
    "--dev-entry-loader",
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

/// Known flags accepted by `zeroship deploy` (bare names, no `=`).
///
/// This is the set `cmd_deploy` actually CONSUMES, read off the call sites
/// rather than off the usage string: `--app=` and `--control=` via `flag_str`,
/// `--token=` via `resolve_bearer_token`, `--no-create` via
/// `deploy_auto_create`. Keeping it derived from behaviour matters because the
/// usage string is a second expression of the same contract, and the gate that
/// checks shipped deploy instructions (`tests/deploy_instructions_gate.sh`)
/// already reads THAT - so if this list ever drifts, it should drift against
/// the parser, not against the prose.
const DEPLOY_KNOWN_FLAGS: &[&str] = &[
    "--app",
    "--app-name",
    "--control",
    "--token",
    "--no-create",
    "--config",
    "--env",
];

/// Return `Err` if any `--flag` argument in `args[3..]` is not a known `deploy`
/// flag. Positional args (no leading `--`) are left unchecked.
///
/// NOT a copy of `check_unknown_serve_flags`: that one consumes the token after
/// a bare `--flag` as its value, because serve accepts the space form
/// (`--workers 4`). Deploy has no space-form flag - `--app`, `--control` and
/// `--token` are equals-only and `--no-create` takes no value - so skipping a
/// token after a bare flag would swallow whatever followed `--no-create`.
pub(crate) fn check_unknown_deploy_flags(args: &[String]) -> Result<(), String> {
    // args[0] = binary, args[1] = "deploy", args[2] = the OPTIONAL <path>.
    // Scanning from 2 rather than 3 is what makes `zeroship deploy --app=x`
    // (no positional, path from the config file) still get its typo check;
    // positional args are skipped by the `--` test below either way.
    for arg in args.iter().skip(2) {
        if !arg.starts_with("--") {
            continue;
        }
        let flag_name = match arg.find('=') {
            Some(idx) => &arg[..idx],
            None => arg.as_str(),
        };
        if !DEPLOY_KNOWN_FLAGS.contains(&flag_name) {
            return Err(format!(
                "unknown flag `{flag_name}`; a typo here is silent - \
                 `--control` falling back to its default would deploy to \
                 http://localhost:9090 instead of the control plane you named. \
                 Usage: zeroship deploy [<path-to-.zship>] [--app=<id>] [--app-name=<name>] \
                 [--control=<url>] [--token=<token>] [--no-create] \
                 [--config=<path>] [--env=<name>]"
            ));
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
    "no API token found; run `zeroship login`, pass `--token=<token>`, or set ZEROSHIP_TOKEN";

pub(crate) fn resolve_bearer_token(args: &[String]) -> Result<String, String> {
    resolve_bearer_token_from(
        args,
        || zeroship_core::declared_env!(cli, "ZEROSHIP_TOKEN", crate::ZeroshipCliConsumer),
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

    const APP_ID: &str = "app_034klb07lrb9jgma6imvmx000";

    fn app_id(raw: &str) -> AppId {
        AppId::parse(raw).expect("test app id must be canonical")
    }

    #[test]
    fn app_id_accepts_only_the_canonical_typed_id() {
        assert_eq!(app_id_or_refuse(APP_ID), Ok(app_id(APP_ID)));
        for raw_uuid in [
            "0197f8a1-2b3c-7d4e-8f90-1a2b3c4d5e6f",
            "0197f8a12b3c7d4e8f901a2b3c4d5e6f",
            "{0197f8a1-2b3c-7d4e-8f90-1a2b3c4d5e6f}",
            "urn:uuid:0197f8a1-2b3c-7d4e-8f90-1a2b3c4d5e6f",
        ] {
            app_id_or_refuse(raw_uuid).expect_err("raw UUID app ids must be rejected");
        }
    }

    /// The converse, and the behaviour change: a NAME is refused, not guessed
    /// at. The refusal must name both concepts, because the creator's next move
    /// depends on which one they meant.
    #[test]
    fn app_id_refuses_a_name_and_says_which_is_which() {
        for name in [
            "my-app",
            "",
            "not-a-uuid",
            "0197f8a1-2b3c-7d4e-8f90-1a2b3c4d5e6",
            // `app_` alone is not enough: the body must be a real id body.
            "app_not-an-id",
        ] {
            let err = app_id_or_refuse(name).expect_err("{name:?} is not an app id");
            assert!(
                err.contains("routing label") && err.contains("identity"),
                "the refusal must distinguish the name from the id: {err}"
            );
            assert!(
                err.contains("--app-name"),
                "and must name the flag that addresses an app by name: {err}"
            );
        }
    }

    #[test]
    fn control_response_requires_a_canonical_app_id() {
        let parsed = parse_app_id(
            r#"{"id":"app_034klb07lrb9jgma6imvmx000"}"#,
            "create app response",
        )
        .expect("canonical app id");
        assert_eq!(parsed, app_id(APP_ID));

        let error = parse_app_id(
            r#"{"id":"0197f8a1-2b3c-7d4e-8f90-1a2b3c4d5e6f"}"#,
            "create app response",
        )
        .expect_err("raw UUID response must not enter an app route");
        assert!(error.contains("invalid app id"), "{error}");
    }

    #[test]
    fn dev_app_id_defaults_to_the_shared_typed_identity() {
        let mut env = std::collections::HashMap::new();
        let app = resolve_dev_app_id(&mut env).expect("default dev app id");
        assert_eq!(app, zeroship_core::app_id::local_dev_app_id());
        assert_eq!(env.get("APP_ID").map(String::as_str), Some(app.as_str()));
    }

    #[test]
    fn dev_app_id_rejects_raw_uuid_input() {
        let mut env = std::collections::HashMap::from([(
            "APP_ID".to_string(),
            "0197f8a1-2b3c-7d4e-8f90-1a2b3c4d5e6f".to_string(),
        )]);
        resolve_dev_app_id(&mut env).expect_err("raw UUID must not scope local app data");
    }

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
            "--dev-entry-loader=createDevEntryLoader",
        ]);
        assert!(check_unknown_serve_flags(&args).is_ok());
    }

    /// The same guarantee for `deploy`, which did not have it. MEASURED against
    /// the release binary on 2026-08-11, before this check existed:
    ///
    ///   zeroship serve  <file> --prot=3000
    ///     -> "unknown flag `--prot`; run `zeroship serve --help` ..."
    ///   zeroship deploy <file> --app=a --token=t --contrl=http://my-control
    ///     -> "Deploying ... to http://localhost:9090/api/apps/a/deploy..."
    ///
    /// The typo'd `--contrl` was ignored and `--control` fell back to its
    /// default, so the deploy went to whatever listens on localhost:9090 -
    /// a working control plane on any machine running the dev stack. That is a
    /// deploy landing on the WRONG TARGET silently, not just a poor message.
    #[test]
    fn unknown_deploy_flag_is_rejected() {
        // Typo: `--contrl` instead of `--control`. This is the one with a
        // wrong-target consequence rather than a confusing-error one.
        let args = s(&[
            "zeroship",
            "deploy",
            "app.zship",
            "--app=myapp",
            "--contrl=http://my-control",
        ]);
        let result = check_unknown_deploy_flags(&args);
        assert!(result.is_err(), "typo'd flag should be rejected");
        let msg = result.unwrap_err();
        assert!(msg.contains("--contrl"), "error should name the unknown flag: {msg}");

        // Every flag the deploy path actually reads is accepted. This list is
        // the one `cmd_deploy` consumes: --app=/--control= via flag_str,
        // --token= via resolve_bearer_token, --no-create via deploy_auto_create.
        let args = s(&[
            "zeroship",
            "deploy",
            "app.zship",
            "--app=myapp",
            "--control=http://localhost:9090",
            "--token=pat",
            "--no-create",
        ]);
        assert!(check_unknown_deploy_flags(&args).is_ok());

        // The positional path argument must not be mistaken for a flag.
        let args = s(&["zeroship", "deploy", "./dist/app.zship", "--app=myapp"]);
        assert!(check_unknown_deploy_flags(&args).is_ok());
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum FakeCall {
        Deploy(String),
        List,
        ListSecrets(String),
        Create(String),
    }

    #[derive(Default)]
    struct FakeControlClient {
        calls: Vec<FakeCall>,
        deploys: VecDeque<ControlResponse>,
        lists: VecDeque<ControlResponse>,
        creates: VecDeque<ControlResponse>,
        secret_lists: VecDeque<ControlResponse>,
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
            app: &AppId,
            _token: &str,
            _body: &[u8],
        ) -> Result<ControlResponse, String> {
            self.calls.push(FakeCall::Deploy(app.as_str().to_string()));
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

        fn list_secrets(
            &mut self,
            _control_url: &str,
            app: &AppId,
            _token: &str,
        ) -> Result<ControlResponse, String> {
            self.calls
                .push(FakeCall::ListSecrets(app.as_str().to_string()));
            self.secret_lists
                .pop_front()
                .ok_or_else(|| "unexpected secret list call".to_string())
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

    /// A missing identity must never trigger name-based app creation.
    #[test]
    fn a_missing_app_id_is_refused_rather_than_created() {
        let missing_app = APP_ID;
        let mut client = FakeControlClient::default()
            .with_deploy(404, r#"{"error":"app not found"}"#)
            .with_list(200, "[]")
            .with_create(
                201,
                r#"{"id":"app_034klb07lrb9jgma6imvmx001","name":"missing"}"#,
            )
            .with_deploy(200, r#"{"deploy_hash":"sha256:abc"}"#);

        let err = deploy_archive(
            &mut client,
            "http://control.test",
            &AppTarget::Id(app_id(missing_app)),
            "token",
            b"zship",
            &[],
            true,
        )
        .expect_err("a 404 on an id must not be answered by creating an app");

        assert!(err.contains("HTTP 404"), "{err}");
        assert!(
            err.contains("never created from one"),
            "the refusal must say why creating was not the answer: {err}"
        );
        assert_eq!(
            client.calls,
            vec![FakeCall::Deploy(missing_app.to_string())],
            "nothing may be listed or created after a 404 on an id",
        );
    }

    /// A name target resolves or creates an app without replacing configured identity.
    #[test]
    fn deploy_flag_auto_create_preserves_configured_app() {
        let temp = tempfile::tempdir().expect("create temp project");
        let config_path = temp.path().join(project_config::CONFIG_FILENAME);
        let original = r#"{
  "name": "production-app",
  "app": "app_034klb07lrb9jgma6imvmx000",
  "control": "http://control.test",
  "runtime_date": "2026-08-14",
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "migrations": { "dir": "migrations", "out": "generated/zeroship" },
  "secrets": []
}
"#;
        std::fs::write(&config_path, original).expect("write project config");
        let config = project_config::ProjectConfig::load(&config_path).expect("load config");
        let resolved = config.resolve(None).expect("resolve config");
        let args = s(&[
            "zeroship",
            "deploy",
            "dist/app.zship",
            "--app-name=scratch-test",
        ]);
        let (app, target) =
            resolve_deploy_app(&args, Some(&resolved)).expect("resolve the by-name flag");
        assert_eq!(app.source, project_config::Source::Flag("--app-name"));
        assert_eq!(
            target,
            AppTarget::Name("scratch-test".to_string()),
            "--app-name must win over the file's `app` id, and must be a NAME",
        );

        let mut client = FakeControlClient::default()
            .with_list(200, "[]")
            .with_create(
                201,
                r#"{"id":"app_034klb07lrb9jgma6imvmx001","name":"scratch-test"}"#,
            )
            .with_deploy(200, r#"{"deploy_hash":"sha256:def"}"#);

        let outcome = deploy_archive(
            &mut client,
            "http://control.test",
            &target,
            "token",
            b"zship",
            &[],
            true,
        )
        .expect("name deploy should create and retry by id");

        let created = outcome.created_app.expect("scratch app was created");
        record_created_app(Some(&config), None, &app.source, &created.id);

        assert_eq!(outcome.deploy_hash.as_deref(), Some("sha256:def"));
        assert_eq!(
            client.calls,
            vec![
                FakeCall::List,
                FakeCall::Create("scratch-test".to_string()),
                FakeCall::Deploy("app_034klb07lrb9jgma6imvmx001".to_string()),
            ]
        );
        assert_eq!(
            std::fs::read_to_string(&config_path).expect("read project config"),
            original,
            "an auto-created --app-name target must not replace the committed app",
        );
    }

    #[test]
    fn deploy_file_app_auto_create_preserves_configured_app() {
        let temp = tempfile::tempdir().expect("create temp project");
        let config_path = temp.path().join(project_config::CONFIG_FILENAME);
        // The `app` member holds an ID. It read `configured-name` until the
        // discriminator was deleted; nothing in this test depended on that
        // value being a name, and leaving one there would document a config
        // `resolve_deploy_app` now refuses.
        let original = r#"{
  "name": "production-app",
  "app": "app_034klb07lrb9jgma6imvmx000",
  "control": "http://control.test",
  "runtime_date": "2026-08-14",
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "migrations": { "dir": "migrations", "out": "generated/zeroship" },
  "secrets": []
}
"#;
        std::fs::write(&config_path, original).expect("write project config");
        let config = project_config::ProjectConfig::load(&config_path).expect("load config");
        let resolved = config.resolve(None).expect("resolve config");
        let app = project_config::resolve_value(
            &s(&["zeroship", "deploy"]),
            "--app",
            None,
            None,
            Some(&resolved),
            "app",
            None,
        )
        .expect("resolve file app");
        assert_eq!(app.source, project_config::Source::File);

        record_created_app(
            Some(&config),
            None,
            &app.source,
            &app_id("app_034klb07lrb9jgma6imvmx001"),
        );

        assert_eq!(
            std::fs::read_to_string(&config_path).expect("read project config"),
            original,
            "an auto-created app resolved from the file must not rewrite it",
        );
    }

    #[test]
    fn deploy_environment_auto_create_never_writes_the_root() {
        let temp = tempfile::tempdir().expect("create temp project");
        let config_path = temp.path().join(project_config::CONFIG_FILENAME);
        let original = r#"{
  "name": "production-app",
  "control": "http://control.test",
  "runtime_date": "2026-08-14",
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "migrations": { "dir": "migrations", "out": "generated/zeroship" },
  "secrets": [],
  "environments": {
    "staging": {
      "app": "staging-app",
      "control": "http://staging-control.test"
    }
  }
}
"#;
        std::fs::write(&config_path, original).expect("write project config");
        let config = project_config::ProjectConfig::load(&config_path).expect("load config");

        record_created_app(
            Some(&config),
            Some("staging"),
            &project_config::Source::FileMember("name"),
            &app_id("app_034klb07lrb9jgma6imvmx001"),
        );

        assert_eq!(
            std::fs::read_to_string(&config_path).expect("read project config"),
            original,
            "an environment app id must never be written at the root",
        );
    }

    #[test]
    fn deploy_no_create_suppresses_missing_app_provisioning() {
        let missing_app = APP_ID;
        let args = s(&[
            "zeroship",
            "deploy",
            "dist/app.zship",
            "--app=app_034klb07lrb9jgma6imvmx000",
            "--no-create",
        ]);
        assert!(!deploy_auto_create(&args));

        let mut client = FakeControlClient::default()
            .with_deploy(404, r#"{"error":"app not found"}"#);

        let err = deploy_archive(
            &mut client,
            "http://control.test",
            &AppTarget::Id(app_id(missing_app)),
            "token",
            b"zship",
            &[],
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
            &AppTarget::Id(app_id(APP_ID)),
            "token",
            b"zship",
            &[],
            true,
        )
        .expect("existing app deploy");

        assert_eq!(outcome.deploy_hash.as_deref(), Some("sha256:existing"));
        assert_eq!(outcome.created_app, None);
        assert_eq!(client.calls, vec![FakeCall::Deploy(APP_ID.to_string())]);
    }

    /// A typed app id is an IDENTITY: deploy addresses it and creates nothing.
    ///
    /// The client is stocked so that BOTH outcomes are available - a list, a
    /// create and a deploy - because a fake that can only answer the expected
    /// call proves the call order by starving the alternative rather than by
    /// measuring it. Whichever path `deploy_archive` takes, it gets served.
    #[test]
    fn a_typed_app_id_deploys_to_that_app_and_creates_nothing() {
        let id = "app_034klb07lrb9jgma6imvmx000";
        let mut client = FakeControlClient::default()
            .with_list(200, "[]")
            .with_create(
                201,
                r#"{"id":"app_034klb07lrb9jgma6imvmx001","name":"app_034klb07lrb9jgma6imvmx000"}"#,
            )
            .with_deploy(200, r#"{"deploy_hash":"sha256:typed"}"#);

        let outcome = deploy_archive(
            &mut client,
            "http://control.test",
            &AppTarget::Id(app_id(id)),
            "token",
            b"zship",
            &[],
            true,
        )
        .expect("a typed app id deploys");

        assert_eq!(
            client.calls,
            vec![FakeCall::Deploy(id.to_string())],
            "an app id must be deployed to directly; a list+create here is the \
             CLI guessing the id was a name and minting an app named after it",
        );
        assert_eq!(
            outcome.created_app, None,
            "addressing an app by its id must never create an app",
        );
        assert_eq!(outcome.deploy_hash.as_deref(), Some("sha256:typed"));
    }

    /// The refusal is WIRED, not merely defined.
    ///
    /// `app_id_or_refuse` having the right opinion proves nothing if the deploy
    /// target is resolved around it, and a predicate whose call site stops
    /// being reached leaves every predicate test green. Both halves go through
    /// `resolve_deploy_app`, one variable apart: the same value, the other flag.
    #[test]
    fn the_id_flag_refuses_a_name_and_the_name_flag_takes_it() {
        let by_id = s(&["zeroship", "deploy", "dist/app.zship", "--app=my-app"]);
        let err = resolve_deploy_app(&by_id, None).expect_err("--app=<name> must be refused");
        assert!(
            err.contains("--app-name"),
            "the refusal must point at the flag that does take a name: {err}"
        );

        let by_name = s(&["zeroship", "deploy", "dist/app.zship", "--app-name=my-app"]);
        let (sourced, target) =
            resolve_deploy_app(&by_name, None).expect("--app-name takes a name");
        assert_eq!(target, AppTarget::Name("my-app".to_string()));
        assert_eq!(sourced.source, project_config::Source::Flag("--app-name"));
    }

    /// Naming a target twice is a refusal, not a precedence rule. Silently
    /// preferring one is how a deploy lands somewhere the creator did not read
    /// on the command line.
    #[test]
    fn the_two_app_flags_cannot_both_be_given() {
        let args = s(&[
            "zeroship",
            "deploy",
            "dist/app.zship",
            "--app=app_034klb07lrb9jgma6imvmx000",
            "--app-name=my-app",
        ]);
        let err = resolve_deploy_app(&args, None).expect_err("two targets must be refused");
        assert!(err.contains("--app") && err.contains("--app-name"), "{err}");
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
                    provider: "platform".to_string(),
                    control_url: None,
                    token_endpoint: None,
                    anon_key: None,
                    userinfo_url: None,
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
        assert!(err.contains("--token=<token>"), "{err}");
    }
}
