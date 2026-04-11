//! appbase CLI — the unified entry point for the appbase platform.
//!
//! Commands:
//!   appbase serve <server.js> [--static=index.html] [--port=3000] [--db=appbase.db]
//!   appbase dev <entrypoint> [--port=3000] [--compiler=appbase-compile]
//!   appbase compile <entrypoint> [--target=rust|node] [--outdir=.dist] [--minify]

use appbase_platform::control::AppRegistry;
use appbase_platform::core::config::{AppbaseConfig, IsolateConfig, ServerConfig};
use appbase_platform::core::plugin::{Plugin, PluginFactory};
use appbase_platform::server::router;
use appbase_runtime::bundle::{AppBundle, ModuleType};
use appbase_runtime::modules::ModuleEntry;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "build" => cmd_build(&args),
        "inspect" => cmd_inspect(&args),
        "serve" => cmd_serve(&args),
        "deploy" => cmd_deploy(&args),
        "platform" => cmd_platform(&args),
        "dev" => cmd_dev(&args),
        _ => print_usage(),
    }
}

fn cmd_build(args: &[String]) {
    let input = args
        .get(2)
        .expect("Usage: appbase build <dir-or-file> [--outdir=dist] [--minify]");
    let outdir = flag_str(args, "--outdir=").unwrap_or_else(|| "dist".into());
    let minify = args.iter().any(|a| a == "--minify");

    let input_path = PathBuf::from(input);

    // Determine app name
    let app_name = if input_path.is_file() {
        input_path
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string()
    } else {
        read_package_name(&input_path)
            .unwrap_or_else(|| input_path.file_name().unwrap().to_string_lossy().to_string())
    };

    let (entry_name, source, source_map) = if input_path.is_file() {
        // Single file -- read directly, skip esbuild (no source map)
        let source =
            std::fs::read_to_string(&input_path).expect("Failed to read input file");
        let name = input_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        (name, source, None)
    } else if input_path.is_dir() {
        // Directory -- bundle with esbuild via the compiler crate
        use appbase_compiler::bundler::{bundle, BundleOptions};

        let options = BundleOptions {
            entry: String::new(),
            minify,
            sourcemap: true,
            ..Default::default()
        };

        let result = bundle(&input_path, &options).unwrap_or_else(|e| {
            eprintln!("Build failed: {e}");
            std::process::exit(1);
        });

        ("index.js".to_string(), result.js, result.source_map)
    } else {
        eprintln!("Input path does not exist: {}", input_path.display());
        std::process::exit(1);
    };

    // Determine module type from extension
    let module_type = if entry_name.ends_with(".json") {
        ModuleType::Json
    } else {
        ModuleType::EsModule
    };

    // Create .appbundle
    let bundle = AppBundle::new(
        &entry_name,
        vec![(entry_name.clone(), module_type, source.clone())],
    );
    let bytes = bundle.to_bytes();

    // Create output directory
    std::fs::create_dir_all(&outdir).expect("Failed to create output directory");

    // Write .appbundle
    let bundle_path = PathBuf::from(&outdir).join("app.appbundle");
    std::fs::write(&bundle_path, &bytes).expect("Failed to write .appbundle");

    // Write source map (if available)
    let source_map_file = if let Some(ref map) = source_map {
        let map_path = PathBuf::from(&outdir).join("app.appbundle.map");
        std::fs::write(&map_path, map).expect("Failed to write source map");
        Some("app.appbundle.map".to_string())
    } else {
        None
    };

    // Content hash from appbundle header (bytes 8..40)
    let content_hash = format!(
        "sha256:{}",
        bytes[8..40]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );

    // Write manifest.json
    let manifest = serde_json::json!({
        "name": app_name,
        "version": 1,
        "entry": entry_name,
        "modules": bundle.module_names(),
        "bundle_file": "app.appbundle",
        "bundle_size": bytes.len(),
        "source_size": source.len(),
        "content_hash": content_hash,
        "source_map": source_map_file,
        "compiler": format!("appbase {}", env!("CARGO_PKG_VERSION")),
        "built_at": utc_now_iso8601(),
    });
    let manifest_path = PathBuf::from(&outdir).join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .expect("Failed to write manifest.json");

    // Summary
    let source_len = source.len();
    let bundle_len = bytes.len();
    eprintln!("Built {app_name} -> {outdir}/");
    eprintln!(
        "  app.appbundle:     {:.1}KB ({:.1}% of {:.1}KB source)",
        bundle_len as f64 / 1024.0,
        bundle_len as f64 / source_len as f64 * 100.0,
        source_len as f64 / 1024.0
    );
    if source_map_file.is_some() {
        eprintln!(
            "  app.appbundle.map: {:.1}KB",
            source_map.as_ref().unwrap().len() as f64 / 1024.0
        );
    }
    eprintln!("  manifest.json:     metadata");
    eprintln!("  hash:              {content_hash}");
}

fn cmd_inspect(args: &[String]) {
    let file = args
        .get(2)
        .expect("Usage: appbase inspect <file.appbundle>");
    let bytes = std::fs::read(file).expect("Failed to read file");

    // Parse (validates integrity)
    let bundle = AppBundle::from_bytes(&bytes).unwrap_or_else(|e| {
        eprintln!("Invalid .appbundle: {e}");
        std::process::exit(1);
    });

    println!(".appbundle v1");
    println!("Modules: {}", bundle.module_count());

    for (i, info) in bundle.all_module_info().iter().enumerate() {
        println!(
            "  [{i}] {} ({}, {:.1}KB compressed, {:.1}KB original)",
            info.specifier,
            info.module_type,
            info.compressed_size as f64 / 1024.0,
            info.original_size as f64 / 1024.0,
        );
    }

    println!("Entry: {}", bundle.entry());
    println!("Bundle size: {:.1}KB", bytes.len() as f64 / 1024.0);

    // SHA-256 from header bytes 8..40
    let hash_hex: String = bytes[8..40].iter().map(|b| format!("{b:02x}")).collect();
    println!("SHA-256: {hash_hex}");
}

fn cmd_serve(args: &[String]) {
    let input = args.get(2).expect(
        "Usage: appbase serve <file-or-dir> [--port=3000] [--workers=0] [--cpu-limit=MS] [--wall-timeout=MS]",
    );
    let port = flag_u16(args, "--port=").unwrap_or(3000);

    let workers: usize = flag_str(args, "--workers=")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let cpu_limit: Option<Duration> = flag_str(args, "--cpu-limit=")
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis);

    let wall_timeout: Option<Duration> = flag_str(args, "--wall-timeout=")
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis);

    let input_path = PathBuf::from(input);

    let modules = if input.ends_with(".appbundle") {
        // Mode 1: load pre-built .appbundle
        load_from_appbundle(&input_path)
    } else if input_path.is_dir() {
        // Mode 2: build from directory (esbuild)
        build_and_load_dir(&input_path)
    } else {
        // Mode 2: single JS/TS file
        build_and_load_file(&input_path)
    };

    eprintln!("[appbase] Starting server on port {port}");

    appbase_runtime::serve::start_server(modules, appbase_runtime::serve::ServerOptions {
        port,
        workers,
        cpu_limit,
        wall_timeout,
    });
}

fn load_from_appbundle(path: &PathBuf) -> Vec<ModuleEntry> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("Failed to read {}: {e}", path.display());
        std::process::exit(1);
    });
    let mut bundle = AppBundle::from_bytes(&bytes).unwrap_or_else(|e| {
        eprintln!("Invalid .appbundle: {e}");
        std::process::exit(1);
    });

    let info = bundle.all_module_info();
    eprintln!(
        "[appbase] Loaded {} ({} modules, {:.1}KB)",
        path.display(),
        info.len(),
        bytes.len() as f64 / 1024.0
    );

    bundle.to_module_entries()
}

fn build_and_load_dir(dir: &PathBuf) -> Vec<ModuleEntry> {
    use appbase_compiler::bundler::{bundle, BundleOptions};

    let options = BundleOptions {
        entry: String::new(),
        minify: false,
        sourcemap: false,
        ..Default::default()
    };

    let result = bundle(dir, &options).unwrap_or_else(|e| {
        eprintln!("Build failed: {e}");
        std::process::exit(1);
    });

    eprintln!(
        "[appbase] Built {:.1}KB from {}",
        result.js.len() as f64 / 1024.0,
        dir.display()
    );

    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: result.js,
    }]
}

fn build_and_load_file(path: &PathBuf) -> Vec<ModuleEntry> {
    let source = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("Failed to read {}: {e}", path.display());
        std::process::exit(1);
    });
    let name = path.file_name().unwrap().to_string_lossy().to_string();

    eprintln!(
        "[appbase] Loaded {} ({:.1}KB)",
        path.display(),
        source.len() as f64 / 1024.0
    );

    vec![ModuleEntry {
        specifier: name,
        source,
    }]
}

fn cmd_deploy(args: &[String]) {
    let input = args.get(2).expect(
        "Usage: appbase deploy <dir-or-file> --app=<name-or-id> [--control=http://localhost:9090] [--key=<master-key>]",
    );
    let app = flag_str(args, "--app=").expect("--app=<name-or-id> is required");
    let control_url = flag_str(args, "--control=")
        .or_else(|| std::env::var("APPBASE_CONTROL_URL").ok())
        .unwrap_or_else(|| "http://localhost:9090".into());
    let master_key = flag_str(args, "--key=")
        .or_else(|| std::env::var("APPBASE_MASTER_KEY").ok())
        .unwrap_or_else(|| "".into());

    let input_path = PathBuf::from(input);

    // Step 1: Build .appbundle (same logic as cmd_build)
    let (entry_name, source) = if input_path.is_file() {
        let source = std::fs::read_to_string(&input_path).expect("Failed to read input file");
        let name = input_path.file_name().unwrap().to_string_lossy().to_string();
        (name, source)
    } else if input_path.is_dir() {
        use appbase_compiler::bundler::{bundle, BundleOptions};
        let options = BundleOptions {
            entry: String::new(),
            minify: false,
            sourcemap: false,
            ..Default::default()
        };
        let result = bundle(&input_path, &options).unwrap_or_else(|e| {
            eprintln!("Build failed: {e}");
            std::process::exit(1);
        });
        ("index.js".to_string(), result.js)
    } else {
        eprintln!("Input path does not exist: {}", input_path.display());
        std::process::exit(1);
    };

    let module_type = if entry_name.ends_with(".json") {
        ModuleType::Json
    } else {
        ModuleType::EsModule
    };

    let bundle = AppBundle::new(
        &entry_name,
        vec![(entry_name.clone(), module_type, source.clone())],
    );
    let bundle_bytes = bundle.to_bytes();

    eprintln!(
        "Built .appbundle: {:.1}KB ({:.0}% of {:.1}KB source)",
        bundle_bytes.len() as f64 / 1024.0,
        bundle_bytes.len() as f64 / source.len() as f64 * 100.0,
        source.len() as f64 / 1024.0,
    );

    // Step 2: Upload to control plane
    // Use a simple blocking HTTP request (no async needed for CLI)
    let deploy_url = format!("{control_url}/api/apps/{app}/deploy");
    eprintln!("Deploying to {deploy_url}...");

    let response = std::process::Command::new("curl")
        .args([
            "-s", "-w", "\n%{http_code}",
            "-X", "POST",
            &deploy_url,
            "-H", &format!("Authorization: Bearer {master_key}"),
            "-H", "Content-Type: application/octet-stream",
            "--data-binary", "@-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(&bundle_bytes).ok();
            }
            child.wait_with_output()
        });

    match response {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let lines: Vec<&str> = stdout.trim().rsplitn(2, '\n').collect();
            let (status_str, body) = if lines.len() == 2 {
                (lines[0], lines[1])
            } else {
                (lines[0], "")
            };

            let status: u16 = status_str.parse().unwrap_or(0);
            if status == 200 {
                eprintln!("Deployed successfully!");
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
                    if let Some(hash) = json.get("deploy_hash").and_then(|h| h.as_str()) {
                        eprintln!("  deploy_hash: {hash}");
                    }
                }
            } else {
                eprintln!("Deploy failed (HTTP {status}): {body}");
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

fn cmd_platform(args: &[String]) {
    let script = args
        .get(2)
        .expect("Usage: appbase platform <server.js> [--static=index.html] [--port=3000] [--db=appbase.db] [--config=appbase.toml]");
    let port = flag_u16(args, "--port=").unwrap_or(3000);
    let static_file = flag_str(args, "--static=");
    let data_dir = PathBuf::from("data");

    // Load config if appbase.toml exists
    let config_path = flag_str(args, "--config=").unwrap_or_else(|| "appbase.toml".into());
    let metering_config = if std::path::Path::new(&config_path).exists() {
        eprintln!("[appbase] Loading config from {config_path}");
        Some(
            appbase_platform::metering::config::MeteringConfig::load(&config_path).unwrap_or_else(|e| {
                eprintln!("[appbase] Config error: {e}");
                std::process::exit(1);
            }),
        )
    } else {
        None
    };

    let default_plan = metering_config
        .as_ref()
        .map(|c| c.plan_for_app("default"));

    let server_js = std::fs::read_to_string(script)
        .unwrap_or_else(|e| panic!("Failed to read {script}: {e}"));
    let client_html = static_file.map(|path| {
        std::fs::read(&path).unwrap_or_else(|e| panic!("Failed to read {path}: {e}"))
    });

    // Plugin factory: no legacy deno_core plugins — the V8 runtime has built-in
    // KV (kv.rs), env (env.rs), crypto, fetch, URL, timers.
    let plugin_factory: PluginFactory = Arc::new(move |_app_id| -> Vec<Box<dyn Plugin>> {
        vec![]
    });

    // Load isolate config from TOML if available, otherwise use defaults
    let isolate_config = if std::path::Path::new(&config_path).exists() {
        let toml_str = std::fs::read_to_string(&config_path).unwrap_or_default();
        toml::from_str::<AppbaseConfig>(&toml_str)
            .map(|c| c.isolates)
            .unwrap_or_default()
    } else {
        IsolateConfig::default()
    };

    let config = AppbaseConfig {
        server: ServerConfig {
            port,
            host: "0.0.0.0".into(),
        },
        isolates: isolate_config,
        plugins: Default::default(),
    };

    // Ensure data directory exists and compute store path before data_dir is moved
    std::fs::create_dir_all(&data_dir).unwrap_or_else(|e| {
        eprintln!("[appbase] Warning: could not create data dir: {e}");
    });
    let store_path = data_dir.join("metering.db");

    // Master key: from env or generate a default for development
    let master_key = std::env::var("APPBASE_MASTER_KEY")
        .unwrap_or_else(|_| "dev-master-key".to_string());

    if master_key == "dev-master-key" {
        eprintln!("\u{26a0}\u{fe0f}  WARNING: Using default master key. Set APPBASE_MASTER_KEY for production.");
        eprintln!("\u{26a0}\u{fe0f}  Admin API is accessible with key: dev-master-key");
    }

    let rt = tokio::runtime::Runtime::new().unwrap();
    if let Err(e) = rt.block_on(async {
        // Create the control plane registry (SQLite-backed via sqlx)
        let registry_db_path = data_dir.join("apps.db");
        let registry: Arc<dyn AppRegistry> = Arc::new(
            appbase_platform::control::sqlx_registry::SqlxRegistry::new(
                &format!("sqlite://{}?mode=rwc", registry_db_path.display()),
                master_key.clone(),
            )
            .await
            .unwrap_or_else(|e| {
                eprintln!("[appbase] Failed to open registry DB: {e}");
                std::process::exit(1);
            }),
        );

        // Backward compat: ensure "default" app exists and deploy the server_js to it
        if registry.get_app("default").await.unwrap_or(None).is_none() {
            registry.create_app("default", "free").await.unwrap_or_else(|e| {
                eprintln!("[appbase] Failed to create default app: {e}");
                std::process::exit(1);
            });
        }
        registry
            .deploy("default", &server_js, client_html.as_deref())
            .await
            .unwrap_or_else(|e| {
                eprintln!("[appbase] Failed to deploy default app: {e}");
                std::process::exit(1);
            });

        // Cold-tier event log + background writer
        let event_log: std::sync::Arc<dyn appbase_platform::core::event_log::EventLog> =
            std::sync::Arc::new(appbase_platform::metering::event_logger::InMemoryEventLog::new());
        let (event_sender, event_writer_handle) =
            appbase_platform::metering::event_channel::spawn_event_writer(event_log);

        let state = router::single_app_state(
            server_js,
            client_html,
            &config,
            data_dir,
            plugin_factory,
            default_plan,
            event_sender,
            registry,
            master_key.clone(),
        );

        // Warm-tier store for metering persistence (prefer SQLite, fallback to in-memory)
        let store: Arc<dyn appbase_platform::core::meter_store::MeterStore> =
            match appbase_platform::metering::store::sqlite::SqliteMeterStore::new(
                &format!("sqlite://{}?mode=rwc", store_path.display()),
            )
            .await
            {
                Ok(s) => {
                    eprintln!(
                        "[appbase] Using SQLite meter store at {}",
                        store_path.display()
                    );
                    Arc::new(s)
                }
                Err(e) => {
                    eprintln!(
                        "[appbase] SQLite meter store failed ({e}), falling back to in-memory"
                    );
                    Arc::new(appbase_platform::metering::store::memory::InMemoryStore::new())
                }
            };

        // Crash recovery: reload counters from warm tier (spec §4.6)
        // With SQLite, this recovers counters from the last flush,
        // preventing quota bypass after restart.
        state
            .meters
            .recover_from_store(store.as_ref(), &["default".to_string()])
            .await;

        // Clone store for final shutdown flush before it is moved into background tasks
        let store_for_shutdown = store.clone();

        // Spawn background metering services
        let flusher_handle = appbase_platform::metering::flusher::spawn_flusher(
            state.meters.clone(),
            store.clone(),
            Duration::from_secs(5),
        );
        let roller_handle = appbase_platform::metering::rollover::spawn_period_roller(
            state.meters.clone(),
            store,
            appbase_platform::metering::rollover::RolloverConfig::default(),
        );

        // Spawn spending reconciler with a default spending limit.
        // TODO: load per-app spending limits from TOML config ([apps.X.spending]).
        let pricing = Arc::new(appbase_platform::billing::pricing::PricingTable::cloudflare_comparable());
        let mut limits = HashMap::new();
        limits.insert(
            "default".to_string(),
            appbase_platform::billing::reconciler::SpendingLimit {
                limit_millicents: Some(500_000), // $5.00 default for free tier
                ..Default::default()
            },
        );
        let reconciler_config = appbase_platform::billing::reconciler::ReconcilerConfig {
            interval: Duration::from_secs(10),
            pricing,
            limits,
        };
        let reconciler_handle = appbase_platform::billing::reconciler::spawn_reconciler(
            reconciler_config,
            state.meters.clone(),
            state.meters.clone(),
        );
        // Idle isolate eviction (every 30s)
        let pool_for_eviction = state.pool.clone();
        let eviction_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                pool_for_eviction.evict_idle();
            }
        });

        // Version polling for hot reload (every 5s)
        let registry_for_poll = state.registry.clone();
        let pool_for_poll = state.pool.clone();
        let bundles_for_poll = state.bundles.clone();
        let versions_for_poll = state.bundle_versions.clone();
        let hot_reload_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let versions = versions_for_poll.read().unwrap().clone();
                for (app_id, cached_version) in &versions {
                    if let Ok(Some(db_version)) = registry_for_poll.get_version(app_id).await {
                        if db_version > *cached_version {
                            // Evict stale cache + isolate
                            bundles_for_poll.lock().unwrap().remove(app_id);
                            pool_for_poll.evict_app(app_id);
                            versions_for_poll.write().unwrap().remove(app_id);
                            eprintln!("[hot-reload] App '{app_id}' updated: v{cached_version} → v{db_version}");
                        }
                    }
                }
            }
        });

        eprintln!(
            "[appbase] Background services started (flusher=5s, roller=60s, reconciler=10s, eviction=30s, hot-reload=5s)"
        );

        let meters_for_shutdown = state.meters.clone();

        let result = router::serve(state, &config.server.host, config.server.port).await;

        // Final flush before aborting background tasks to avoid losing recent increments
        eprintln!("[appbase] Shutting down — final flush...");
        appbase_platform::metering::flusher::flush_all(&meters_for_shutdown, store_for_shutdown.as_ref()).await;

        // Abort background tasks on shutdown
        flusher_handle.abort();
        roller_handle.abort();
        reconciler_handle.abort();
        event_writer_handle.abort();
        eviction_handle.abort();
        hot_reload_handle.abort();

        result
    }) {
        eprintln!("[appbase] Error: {e}");
        std::process::exit(1);
    }
}

fn cmd_dev(_args: &[String]) {
    eprintln!("The `dev` command is not yet implemented.");
    eprintln!("Use `appbase serve <server.js>` instead.");
    std::process::exit(1);
}

fn print_usage() {
    eprintln!("appbase — AI-native full-stack app platform");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  appbase build    <dir-or-file> [--outdir=dist] [--minify]");
    eprintln!("  appbase inspect  <file.appbundle>");
    eprintln!("  appbase serve    <file-or-dir> [--port=3000] [--workers=0]");
    eprintln!("                   Serve a .appbundle, JS file, or project directory");
    eprintln!("  appbase deploy   <dir-or-file> --app=<name-or-id> [--control=URL] [--key=KEY]");
    eprintln!("                   Build and deploy to the control plane");
    eprintln!("  appbase platform <server.js> [--static=index.html] [--port=3000] [--db=appbase.db]");
    eprintln!("                   Start the full platform (legacy monolith)");
    eprintln!("  appbase dev      <entrypoint> [--port=3000]");
}

// --- Build helpers ---

fn read_package_name(dir: &PathBuf) -> Option<String> {
    let pkg = dir.join("package.json");
    let text = std::fs::read_to_string(pkg).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
    parsed.get("name")?.as_str().map(|s| s.to_string())
}

fn utc_now_iso8601() -> String {
    let output = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
        .trim()
        .to_string();
    if output.is_empty() {
        "unknown".to_string()
    } else {
        output
    }
}

// --- CLI helpers ---

fn flag_u16(args: &[String], prefix: &str) -> Option<u16> {
    args.iter()
        .find(|a| a.starts_with(prefix))
        .and_then(|a| a.strip_prefix(prefix))
        .and_then(|s| s.parse().ok())
}

fn flag_str(args: &[String], prefix: &str) -> Option<String> {
    args.iter()
        .find(|a| a.starts_with(prefix))
        .and_then(|a| a.strip_prefix(prefix))
        .map(|s| s.to_string())
}
