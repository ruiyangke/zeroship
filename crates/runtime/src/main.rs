use appbase_runtime::{dev, plugins, server};
use appbase_runtime::v8::Plugin;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "serve" => {
            let script = args.get(2).expect("Usage: appbase-rt serve <server.js> [--static=index.html] [--port=3000] [--db=appbase.db]");
            let port = parse_flag(&args, "--port=").unwrap_or(3000);
            let db_path = parse_flag_str(&args, "--db=").unwrap_or("appbase.db".into());
            let static_file = parse_flag_str(&args, "--static=");

            // Build plugins from CLI flags
            let plugin_list: Vec<Box<dyn Plugin>> = vec![
                Box::new(plugins::db::DbPlugin::new(&db_path)),
            ];

            let rt = tokio::runtime::Runtime::new().unwrap();
            if let Err(e) = rt.block_on(server::serve(script, port, static_file.as_deref(), plugin_list)) {
                eprintln!("[appbase-rt] Error: {e}");
                std::process::exit(1);
            }
        }
        "dev" => {
            let entry = args.get(2).expect("Usage: appbase-rt dev <app.jsx> [--port=3000] [--compiler=appbase-compile]");
            let port = parse_flag(&args, "--port=").unwrap_or(3000);
            let compiler = parse_flag_str(&args, "--compiler=").unwrap_or("appbase-compile".into());
            let minify = args.iter().any(|a| a == "--minify");

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            if let Err(e) = rt.block_on(dev::dev(entry, port, &compiler, minify)) {
                eprintln!("[appbase-rt] Error: {e}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("Usage:");
            eprintln!("  appbase-rt serve <server.js> [--static=index.html] [--port=3000] [--db=appbase.db]");
            eprintln!("  appbase-rt dev <app.jsx> [--port=3000] [--compiler=appbase-compile]");
        }
    }
}

fn parse_flag(args: &[String], prefix: &str) -> Option<u16> {
    args.iter().find(|a| a.starts_with(prefix))
        .and_then(|a| a.strip_prefix(prefix))
        .and_then(|s| s.parse().ok())
}

fn parse_flag_str(args: &[String], prefix: &str) -> Option<String> {
    args.iter().find(|a| a.starts_with(prefix))
        .and_then(|a| a.strip_prefix(prefix))
        .map(|s| s.to_string())
}
