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
        "dev" => cmd_dev(&args),
        _ => print_usage(),
    }
}

fn cmd_build(args: &[String]) {
    let input = args
        .get(2)
        .expect("Usage: appbase build <dir-or-file> [--output=app.appbundle] [--minify]");
    let output = flag_str(args, "--output=").unwrap_or_else(|| "app.appbundle".into());
    let minify = args.iter().any(|a| a == "--minify");

    let input_path = PathBuf::from(input);

    let (entry_name, source) = if input_path.is_file() {
        // Single file -- read directly, skip esbuild
        let source =
            std::fs::read_to_string(&input_path).expect("Failed to read input file");
        let name = input_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        (name, source)
    } else if input_path.is_dir() {
        // Directory -- bundle with esbuild via the compiler crate
        use appbase_compiler::bundler::{bundle, BundleOptions};

        let options = BundleOptions {
            // entry left empty => auto-detect
            entry: String::new(),
            minify,
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

    std::fs::write(&output, &bytes).expect("Failed to write .appbundle");

    let source_len = source.len();
    let bundle_len = bytes.len();
    eprintln!("Built {output}:");
    eprintln!(
        "  Source:    {source_len} bytes ({:.1}KB)",
        source_len as f64 / 1024.0
    );
    eprintln!(
        "  Bundle:    {bundle_len} bytes ({:.1}KB)",
        bundle_len as f64 / 1024.0
    );
    eprintln!(
        "  Ratio:     {:.1}%",
        bundle_len as f64 / source_len as f64 * 100.0
    );
    eprintln!("  Entry:     {entry_name}");
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
    let script = args
        .get(2)
        .expect("Usage: appbase serve <server.js> [--static=index.html] [--port=3000] [--db=appbase.db] [--config=appbase.toml]");
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
    eprintln!("  appbase build   <dir-or-file> [--output=app.appbundle] [--minify]");
    eprintln!("  appbase inspect <file.appbundle>");
    eprintln!("  appbase serve   <server.js> [--static=index.html] [--port=3000] [--db=appbase.db]");
    eprintln!("  appbase dev     <entrypoint> [--port=3000] [--compiler=appbase-compile]");
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
