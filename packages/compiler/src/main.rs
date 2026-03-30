use appbase_compiler::{compile, Target};
use std::fs;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let input = args.get(1).expect("Usage: appbase-compile <file> [--target=rust|node] [--outdir=.dist] [--json]");
    let target = args.iter()
        .find(|a| a.starts_with("--target="))
        .and_then(|a| a.strip_prefix("--target="))
        .unwrap_or("rust");
    let outdir = args.iter()
        .find(|a| a.starts_with("--outdir="))
        .and_then(|a| a.strip_prefix("--outdir="))
        .unwrap_or(".dist");
    let json_mode = args.iter().any(|a| a == "--json");

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

    let result = compile(&source, target);

    if json_mode {
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
        return;
    }

    // Write output files
    let out = Path::new(outdir);
    fs::create_dir_all(out).unwrap();

    // Server code
    if !result.server.is_empty() {
        fs::write(out.join("server.js"), &result.server).unwrap();
        eprintln!("[compile] Wrote {}/server.js", outdir);
    }

    // Client HTML (self-contained with esm.sh import maps)
    let entry = result.entry_component.as_deref().unwrap_or("App");
    let html = generate_html(&result.client, entry);
    fs::write(out.join("index.html"), &html).unwrap();
    eprintln!("[compile] Wrote {}/index.html", outdir);

    // Metadata
    let meta = serde_json::json!({
        "entry_component": result.entry_component,
        "server_functions": result.server_functions,
    });
    fs::write(out.join("meta.json"), serde_json::to_string_pretty(&meta).unwrap()).unwrap();
    eprintln!("[compile] Wrote {}/meta.json", outdir);

    eprintln!("[compile] Done. {} server functions: {}",
        result.server_functions.len(),
        result.server_functions.join(", "));
}

fn generate_html(client_js: &str, entry: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>appbase</title>
  <script type="importmap">
  {{
    "imports": {{
      "react": "https://esm.sh/react@19",
      "react/": "https://esm.sh/react@19/",
      "react-dom": "https://esm.sh/react-dom@19",
      "react-dom/": "https://esm.sh/react-dom@19/"
    }}
  }}
  </script>
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
