//! zeroship CLI — build, serve, deploy, and inspect apps.
//!
//! Commands:
//!   zeroship build   <dir-or-file> [--outdir=dist] [--minify]
//!   zeroship inspect <file.appbundle>
//!   zeroship serve   <file-or-dir> [--port=3000] [--workers=0]
//!   zeroship deploy  <dir-or-file> --app=<id> [--control=URL] [--key=KEY]

use std::path::PathBuf;
use std::sync::Arc;

use zeroship_bundle::{AppBundle, ModuleType, ModuleEntry};
use zeroship_runtime::NativePlugin;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "build" => cmd_build(&args),
        "inspect" => cmd_inspect(&args),
        "serve" => cmd_serve(&args),
        "deploy" => cmd_deploy(&args),
        _ => print_usage(),
    }
}

// ---------------------------------------------------------------------------
// build
// ---------------------------------------------------------------------------

fn cmd_build(args: &[String]) {
    let input = args
        .get(2)
        .expect("Usage: zeroship build <dir-or-file> [--outdir=dist] [--minify]");
    let outdir = flag_str(args, "--outdir=").unwrap_or_else(|| "dist".into());
    let minify = args.iter().any(|a| a == "--minify");

    let input_path = PathBuf::from(input);

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

    // Try dual bundle (server + client split) for directories
    if input_path.is_dir() {
        use zeroship_compiler::bundler::{dual_bundle, BundleOptions as BundleOpts};
        let opts = BundleOpts {
            entry: String::new(),
            minify,
            sourcemap: !minify,
            ..Default::default()
        };
        match dual_bundle(&input_path, &opts) {
            Ok(result) => {
                let source = &result.server.js;
                let bundle = AppBundle::new(
                    "index.js",
                    vec![("index.js".to_string(), ModuleType::EsModule, source.clone())],
                );
                let bytes = bundle.to_bytes();

                std::fs::create_dir_all(&outdir).expect("Failed to create output directory");

                // Write server .appbundle
                let bundle_path = PathBuf::from(&outdir).join("app.appbundle");
                std::fs::write(&bundle_path, &bytes).expect("Failed to write .appbundle");

                // Write client bundle
                if let Some(ref client) = result.client {
                    let pub_dir = PathBuf::from(&outdir).join("public");
                    std::fs::create_dir_all(&pub_dir).expect("Failed to create public dir");
                    std::fs::write(pub_dir.join("app.js"), &client.js)
                        .expect("Failed to write app.js");
                    eprintln!("  client: {}/public/app.js ({:.1}KB)", outdir, client.size_bytes as f64 / 1024.0);

                    // Generate index.html
                    if let Some(ref component) = result.entry_component {
                        let html = generate_client_html_external(component);
                        std::fs::write(pub_dir.join("index.html"), &html)
                            .expect("Failed to write index.html");
                        eprintln!("  html:   {}/public/index.html", outdir);
                    }
                }

                // Write source map
                let source_map_file = if let Some(ref map) = result.server.source_map {
                    let map_path = PathBuf::from(&outdir).join("app.appbundle.map");
                    std::fs::write(&map_path, map).expect("Failed to write source map");
                    Some("app.appbundle.map".to_string())
                } else {
                    None
                };

                let content_hash = format!(
                    "sha256:{}",
                    bytes[8..40].iter().map(|b| format!("{b:02x}")).collect::<String>()
                );

                let manifest = serde_json::json!({
                    "name": app_name,
                    "version": 1,
                    "entry": "index.js",
                    "modules": bundle.module_names(),
                    "bundle_file": "app.appbundle",
                    "bundle_size": bytes.len(),
                    "source_size": source.len(),
                    "content_hash": content_hash,
                    "source_map": source_map_file,
                    "server_functions": result.server_functions,
                    "entry_component": result.entry_component,
                    "has_client": result.client.is_some(),
                    "compiler": format!("zeroship {}", env!("CARGO_PKG_VERSION")),
                    "built_at": utc_now_iso8601(),
                });
                let manifest_path = PathBuf::from(&outdir).join("manifest.json");
                std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest).unwrap())
                    .expect("Failed to write manifest.json");

                eprintln!("Built {app_name} -> {outdir}/");
                eprintln!("  app.appbundle:     {:.1}KB ({:.1}% of {:.1}KB source)",
                    bytes.len() as f64 / 1024.0,
                    bytes.len() as f64 / source.len() as f64 * 100.0,
                    source.len() as f64 / 1024.0,
                );
                eprintln!("  hash:              {content_hash}");
                if !result.server_functions.is_empty() {
                    eprintln!("  server fns:        {}", result.server_functions.join(", "));
                }
                return;
            }
            Err(e) => {
                eprintln!("Dual build failed: {e}");
                std::process::exit(1);
            }
        }
    }

    // Single file path (JSX/TSX or plain JS/TS)
    let (entry_name, source, source_map, client_html) = if input_path.is_file() && {
        let name = input_path.file_name().unwrap().to_string_lossy();
        name.ends_with(".jsx") || name.ends_with(".tsx")
    } {
        let raw = std::fs::read_to_string(&input_path).expect("Failed to read input file");
        let project_root = input_path.parent().unwrap_or(std::path::Path::new("."));
        let result = zeroship_compiler::compile_with_options(
            &raw,
            zeroship_compiler::Target::Rust,
            minify,
            Some(project_root),
        );
        if !result.server.is_empty() {
            let html = result.entry_component.as_ref().map(|component| {
                generate_client_html(&result.client, component)
            });
            ("index.js".to_string(), result.server, None, html)
        } else {
            ("index.js".to_string(), raw, None, None)
        }
    } else {
        let (name, source, map) = compile_input(&input_path, minify);
        (name, source, map, None)
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
    let bytes = bundle.to_bytes();

    std::fs::create_dir_all(&outdir).expect("Failed to create output directory");

    let bundle_path = PathBuf::from(&outdir).join("app.appbundle");
    std::fs::write(&bundle_path, &bytes).expect("Failed to write .appbundle");

    if let Some(ref html) = client_html {
        let client_dir = PathBuf::from(&outdir).join("public");
        std::fs::create_dir_all(&client_dir).expect("Failed to create public directory");
        std::fs::write(client_dir.join("index.html"), html).expect("Failed to write index.html");
        eprintln!("  client: {}/public/index.html", outdir);
    }

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
        "compiler": format!("zeroship {}", env!("CARGO_PKG_VERSION")),
        "built_at": utc_now_iso8601(),
    });
    let manifest_path = PathBuf::from(&outdir).join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .expect("Failed to write manifest.json");

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

// ---------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------

fn cmd_inspect(args: &[String]) {
    let file = args
        .get(2)
        .expect("Usage: zeroship inspect <file.appbundle>");
    let bytes = std::fs::read(file).expect("Failed to read file");

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

    let hash_hex: String = bytes[8..40].iter().map(|b| format!("{b:02x}")).collect();
    println!("SHA-256: {hash_hex}");
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

fn cmd_serve(args: &[String]) {
    let input = args.get(2).expect(
        "Usage: zeroship serve <file-or-dir> [--port=3000] [--workers=0] [--cpu-limit=MS] [--wall-timeout=MS]",
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

    let input_path = PathBuf::from(input);

    let modules = if input.ends_with(".appbundle") {
        load_from_appbundle(&input_path)
    } else if input_path.is_dir() {
        build_and_load_dir(&input_path)
    } else {
        let (mods, _client_html) = build_and_load_file(&input_path);
        // Client HTML is written to dist/public/ by build — gateway serves it.
        mods
    };

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

    zeroship_runtime::serve::start_server(
        modules,
        zeroship_runtime::serve::ServerOptions {
            port,
            workers,
            cpu_limit,
            wall_timeout,
            plugins,
        },
    );
}

// ---------------------------------------------------------------------------
// deploy
// ---------------------------------------------------------------------------

fn cmd_deploy(args: &[String]) {
    let input = args.get(2).expect(
        "Usage: zeroship deploy <dir-or-file> --app=<name-or-id> [--control=http://localhost:9090] [--key=<master-key>]",
    );
    let app = flag_str(args, "--app=").expect("--app=<name-or-id> is required");
    let control_url = flag_str(args, "--control=")
        .or_else(|| std::env::var("ZEROSHIP_CONTROL_URL").ok())
        .unwrap_or_else(|| "http://localhost:9090".into());
    let master_key = flag_str(args, "--key=")
        .or_else(|| std::env::var("ZEROSHIP_MASTER_KEY").ok())
        .unwrap_or_default();

    let input_path = PathBuf::from(input);

    // Build .appbundle
    let (entry_name, source, _) = compile_input(&input_path, false);

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

    // Upload to control plane via curl
    let deploy_url = format!("{control_url}/api/apps/{app}/deploy");
    eprintln!("Deploying to {deploy_url}...");

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
            "Content-Type: application/octet-stream",
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

    // Deploy static assets from public/ directory (if it exists)
    let public_dir = if input_path.is_dir() {
        input_path.join("public")
    } else {
        input_path
            .parent()
            .map(|p| p.join("public"))
            .unwrap_or_else(|| PathBuf::from("public"))
    };

    if public_dir.is_dir() {
        deploy_assets(&public_dir, &app, &control_url, &master_key);
    }
}

/// Walk a directory and upload all files as static assets.
fn deploy_assets(public_dir: &PathBuf, app: &str, control_url: &str, master_key: &str) {
    let files = walk_dir(public_dir);
    if files.is_empty() {
        return;
    }

    eprintln!("Deploying {} static assets from {}...", files.len(), public_dir.display());

    let mut uploaded = 0;
    let mut total_bytes = 0usize;

    for (rel_path, full_path) in &files {
        let data = match std::fs::read(full_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("  WARN: skipping {rel_path}: {e}");
                continue;
            }
        };
        total_bytes += data.len();

        let url = format!("{control_url}/api/apps/{app}/assets/{rel_path}");

        let result = std::process::Command::new("curl")
            .args([
                "-s",
                "-w", "\n%{http_code}",
                "-X", "PUT",
                &url,
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
                    stdin.write_all(&data).ok();
                }
                child.wait_with_output()
            });

        match result {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let status: u16 = stdout
                    .trim()
                    .rsplit('\n')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if status == 200 {
                    uploaded += 1;
                } else {
                    eprintln!("  WARN: {rel_path} upload failed (HTTP {status})");
                }
            }
            Err(e) => {
                eprintln!("  WARN: {rel_path} upload failed: {e}");
            }
        }
    }

    eprintln!(
        "Uploaded {uploaded}/{} assets ({:.1}KB total)",
        files.len(),
        total_bytes as f64 / 1024.0,
    );
}

/// Recursively walk a directory, returning (relative_path, full_path) pairs.
fn walk_dir(dir: &PathBuf) -> Vec<(String, PathBuf)> {
    let mut result = Vec::new();
    walk_dir_inner(dir, dir, &mut result);
    result
}

fn walk_dir_inner(root: &PathBuf, current: &PathBuf, result: &mut Vec<(String, PathBuf)>) {
    let entries = match std::fs::read_dir(current) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_dir_inner(root, &path, result);
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                result.push((rel_str, path.clone()));
            }
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
    eprintln!("  zeroship build    <dir-or-file> [--outdir=dist] [--minify]");
    eprintln!("                   Compile JS/TS into an .appbundle");
    eprintln!("  zeroship inspect  <file.appbundle>");
    eprintln!("                   Show .appbundle metadata");
    eprintln!("  zeroship serve    <file-or-dir> [--port=3000] [--workers=0]");
    eprintln!("                   Serve a .appbundle, JS file, or project directory");
    eprintln!("  zeroship deploy   <dir-or-file> --app=<id> [--control=URL] [--key=KEY]");
    eprintln!("                   Build and deploy to the control plane");
}

/// Compile input (file or directory) into (entry_name, source, source_map).
fn compile_input(
    input_path: &PathBuf,
    minify: bool,
) -> (String, String, Option<String>) {
    if input_path.is_file() {
        let source =
            std::fs::read_to_string(input_path).expect("Failed to read input file");
        let name = input_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        (name, source, None)
    } else if input_path.is_dir() {
        use zeroship_compiler::bundler::{bundle, BundleOptions};
        let options = BundleOptions {
            entry: String::new(),
            minify,
            sourcemap: !minify,
            ..Default::default()
        };
        let result = bundle(input_path, &options).unwrap_or_else(|e| {
            eprintln!("Build failed: {e}");
            std::process::exit(1);
        });
        ("index.js".to_string(), result.js, result.source_map)
    } else {
        eprintln!("Input path does not exist: {}", input_path.display());
        std::process::exit(1);
    }
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
        "[zeroship] Loaded {} ({} modules, {:.1}KB)",
        path.display(),
        info.len(),
        bytes.len() as f64 / 1024.0
    );
    bundle.to_module_entries()
}

fn build_and_load_dir(dir: &PathBuf) -> Vec<ModuleEntry> {
    use zeroship_compiler::bundler::{bundle, BundleOptions};
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
        "[zeroship] Built {:.1}KB from {}",
        result.js.len() as f64 / 1024.0,
        dir.display()
    );
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: result.js,
    }]
}

fn build_and_load_file(path: &PathBuf) -> (Vec<ModuleEntry>, Option<String>) {
    let source = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("Failed to read {}: {e}", path.display());
        std::process::exit(1);
    });
    let name = path.file_name().unwrap().to_string_lossy().to_string();

    // For JSX/TSX files, run the SWC compiler to split server + client
    if name.ends_with(".jsx") || name.ends_with(".tsx") {
        let project_root = path.parent().unwrap_or(std::path::Path::new("."));
        let result = zeroship_compiler::compile_with_options(
            &source,
            zeroship_compiler::Target::Rust,
            false,
            Some(project_root),
        );

        if !result.server.is_empty() {
            eprintln!("[zeroship] Compiled {} → server + client", path.display());
            let modules = vec![ModuleEntry {
                specifier: "index.js".into(),
                source: result.server,
            }];

            let html = result.entry_component.as_ref().map(|component| {
                eprintln!("[zeroship] Generated client HTML (entry: {component})");
                generate_client_html(&result.client, component)
            });

            return (modules, html);
        }
    }

    eprintln!(
        "[zeroship] Loaded {} ({:.1}KB)",
        path.display(),
        source.len() as f64 / 1024.0
    );
    (vec![ModuleEntry {
        specifier: name,
        source,
    }], None)
}

/// Generate an HTML page that references an external app.js client bundle.
fn generate_client_html_external(entry_component: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>zeroship app</title>
</head>
<body>
  <div id="root"></div>
  <script src="https://unpkg.com/react@18/umd/react.production.min.js"></script>
  <script src="https://unpkg.com/react-dom@18/umd/react-dom.production.min.js"></script>
  <script src="app.js"></script>
  <script>
    ReactDOM.createRoot(document.getElementById("root")).render(React.createElement({entry_component}));
  </script>
</body>
</html>"#
    )
}

/// Generate an HTML page with inlined client JS (for single-file apps).
fn generate_client_html(client_js: &str, entry_component: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>zeroship app</title>
</head>
<body>
  <div id="root"></div>
  <script src="https://unpkg.com/react@18/umd/react.production.min.js"></script>
  <script src="https://unpkg.com/react-dom@18/umd/react-dom.production.min.js"></script>
  <script>
{client_js}
ReactDOM.createRoot(document.getElementById("root")).render(React.createElement({entry_component}));
  </script>
</body>
</html>"#
    )
}

fn read_package_name(dir: &PathBuf) -> Option<String> {
    let pkg = dir.join("package.json");
    let text = std::fs::read_to_string(pkg).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
    parsed.get("name")?.as_str().map(|s| s.to_string())
}

fn utc_now_iso8601() -> String {
    std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

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
