use appbase_compiler::{compile_with_options, Target};
use std::fs;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let input = args.get(1).expect(
        "Usage: appbase-compile <file> [--target=rust|node] [--outdir=.dist] [--cdn=esm.sh|jsdelivr|skypack|none] [--json]"
    );
    let target = args.iter()
        .find(|a| a.starts_with("--target="))
        .and_then(|a| a.strip_prefix("--target="))
        .unwrap_or("rust");
    let outdir = args.iter()
        .find(|a| a.starts_with("--outdir="))
        .and_then(|a| a.strip_prefix("--outdir="))
        .unwrap_or(".dist");
    let cdn = args.iter()
        .find(|a| a.starts_with("--cdn="))
        .and_then(|a| a.strip_prefix("--cdn="))
        .unwrap_or("esm.sh");
    let json_mode = args.iter().any(|a| a == "--json");
    let minify = args.iter().any(|a| a == "--minify");

    let target = match target {
        "rust" => Target::Rust,
        "node" => Target::Node,
        _ => {
            eprintln!("Unknown target: {}. Use 'rust' or 'node'.", target);
            std::process::exit(1);
        }
    };

    let source = fs::read_to_string(input).unwrap_or_else(|e| {
        eprintln!("Failed to read {}: {}", input, e);
        std::process::exit(1);
    });

    let result = compile_with_options(&source, target, minify);

    if json_mode {
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
        return;
    }

    // Write output files
    let out = Path::new(outdir);
    fs::create_dir_all(out).unwrap();

    if !result.server.is_empty() {
        fs::write(out.join("server.js"), &result.server).unwrap();
        eprintln!("[compile] Wrote {}/server.js", outdir);
    }

    let entry = result.entry_component.as_deref().unwrap_or("App");
    let html = generate_html(&result.client, entry, cdn);
    fs::write(out.join("index.html"), &html).unwrap();
    eprintln!("[compile] Wrote {}/index.html (cdn: {})", outdir, cdn);

    let meta = serde_json::json!({
        "entry_component": result.entry_component,
        "server_functions": result.server_functions,
        "cdn": cdn,
    });
    fs::write(out.join("meta.json"), serde_json::to_string_pretty(&meta).unwrap()).unwrap();
    eprintln!("[compile] Wrote {}/meta.json", outdir);

    eprintln!("[compile] Done. {} server functions: {}",
        result.server_functions.len(),
        result.server_functions.join(", "));
}

fn cdn_url(cdn: &str, pkg: &str) -> String {
    match cdn {
        "esm.sh" => format!("https://esm.sh/{}", pkg),
        "jsdelivr" => format!("https://esm.run/{}", pkg),
        "skypack" => format!("https://cdn.skypack.dev/{}", pkg),
        _ => String::new(),
    }
}

fn generate_html(client_js: &str, entry: &str, cdn: &str) -> String {
    let import_map = if cdn == "none" {
        String::new()
    } else {
        format!(
            r#"<script type="importmap">
  {{
    "imports": {{
      "react": "{}",
      "react/": "{}/",
      "react-dom": "{}",
      "react-dom/": "{}/"
    }}
  }}
  </script>"#,
            cdn_url(cdn, "react@19"),
            cdn_url(cdn, "react@19"),
            cdn_url(cdn, "react-dom@19"),
            cdn_url(cdn, "react-dom@19"),
        )
    };

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>appbase</title>
  {import_map}
  <style>
    *, *::before, *::after {{ box-sizing: border-box; margin: 0; padding: 0; }}
    body {{ -webkit-font-smoothing: antialiased; }}
  </style>
</head>
<body>
  <div id="root"></div>
  <script type="module">
    import React, {{ useState, useEffect }} from 'react';
    import {{ createRoot }} from 'react-dom/client';

    {client_js}

    const root = createRoot(document.getElementById('root'));
    root.render(React.createElement({entry}));
  </script>
</body>
</html>"#
    )
}
