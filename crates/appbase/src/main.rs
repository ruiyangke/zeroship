//! appbase CLI — the unified entry point for the appbase platform.
//!
//! Commands:
//!   appbase serve <server.js> [--static=index.html] [--port=3000] [--db=appbase.db]
//!   appbase dev <app.jsx> [--port=3000] [--compiler=appbase-compile]
//!   appbase compile <app.jsx> [--target=rust|node] [--outdir=.dist] [--minify]

use appbase_core::config::{AppbaseConfig, IsolateConfig, ServerConfig};
use appbase_core::plugin::{Plugin, PluginFactory};
use appbase_server::router;
use std::path::PathBuf;
use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "serve" => cmd_serve(&args),
        "dev" => cmd_dev(&args),
        _ => print_usage(),
    }
}

fn cmd_serve(args: &[String]) {
    let script = args
        .get(2)
        .expect("Usage: appbase serve <server.js> [--static=index.html] [--port=3000] [--db=appbase.db]");
    let port = flag_u16(args, "--port=").unwrap_or(3000);
    let db_path = flag_str(args, "--db=").unwrap_or_else(|| "appbase.db".into());
    let static_file = flag_str(args, "--static=");
    let data_dir = PathBuf::from("data");

    let server_js = std::fs::read_to_string(script)
        .unwrap_or_else(|e| panic!("Failed to read {script}: {e}"));
    let client_html = static_file.map(|path| {
        std::fs::read(&path).unwrap_or_else(|e| panic!("Failed to read {path}: {e}"))
    });

    // Plugin factory: creates fresh plugins for each isolate
    let plugin_factory: PluginFactory = Arc::new(move |_app_id| -> Vec<Box<dyn Plugin>> {
        vec![
            Box::new(appbase_plugins::db::DbPlugin::with_path(&db_path)),
            Box::new(appbase_plugins::kv::KvPlugin::new()),
            Box::new(appbase_plugins::env::EnvPlugin::from_system("APPBASE_")),
        ]
    });

    let config = AppbaseConfig {
        server: ServerConfig {
            port,
            host: "0.0.0.0".into(),
        },
        isolates: IsolateConfig::default(),
        plugins: Default::default(),
    };

    let rt = tokio::runtime::Runtime::new().unwrap();
    if let Err(e) = rt.block_on(async {
        let state = router::single_app_state(
            server_js,
            client_html,
            &config,
            data_dir,
            plugin_factory,
        );
        router::serve(state, &config.server.host, config.server.port).await
    }) {
        eprintln!("[appbase] Error: {e}");
        std::process::exit(1);
    }
}

fn cmd_dev(args: &[String]) {
    let entry = args
        .get(2)
        .expect("Usage: appbase dev <app.jsx> [--port=3000] [--compiler=appbase-compile]");
    let port = flag_u16(args, "--port=").unwrap_or(3000);
    let compiler = flag_str(args, "--compiler=").unwrap_or_else(|| "appbase-compile".into());
    let minify = args.iter().any(|a| a == "--minify");

    eprintln!("[appbase] Dev mode not yet migrated to new architecture.");
    eprintln!("[appbase] Use the old runtime: appbase-rt dev {entry} --port={port} --compiler={compiler}");
    if minify {
        eprintln!("[appbase] --minify flag noted");
    }
    // TODO: migrate dev mode to use appbase-server with dev middleware
    std::process::exit(1);
}

fn print_usage() {
    eprintln!("appbase — AI-native full-stack app platform");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  appbase serve <server.js> [--static=index.html] [--port=3000] [--db=appbase.db]");
    eprintln!("  appbase dev <app.jsx> [--port=3000] [--compiler=appbase-compile]");
    eprintln!();
    eprintln!("Compile with appbase-compile:");
    eprintln!("  appbase-compile <app.jsx> [--target=rust|node] [--outdir=.dist] [--minify]");
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
