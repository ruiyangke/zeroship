use appbase_runtime::{dev, server};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "serve" => {
            // Production: serve pre-compiled files
            let script = args.get(2).expect("Usage: appbase-rt serve <server.js> [--static=index.html] [--port=3000] [--db=appbase.db]");
            let port = parse_flag(&args, "--port=").unwrap_or(3000);
            let db_path = parse_flag_str(&args, "--db=").unwrap_or("appbase.db".into());
            let static_file = parse_flag_str(&args, "--static=");

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            if let Err(e) = rt.block_on(server::serve(script, &db_path, port, static_file.as_deref())) {
                eprintln!("[appbase-rt] Error: {}", e);
                std::process::exit(1);
            }
        }
        "dev" => {
            // Dev mode: compile + serve + watch + WS reload
            let entry = args.get(2).expect("Usage: appbase-rt dev <app.jsx> [--port=3000] [--compiler=appbase-compile]");
            let port = parse_flag(&args, "--port=").unwrap_or(3000);
            let compiler = parse_flag_str(&args, "--compiler=").unwrap_or("appbase-compile".into());
            let minify = args.iter().any(|a| a == "--minify");

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            if let Err(e) = rt.block_on(dev::dev(entry, port, &compiler, minify)) {
                eprintln!("[appbase-rt] Error: {}", e);
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
