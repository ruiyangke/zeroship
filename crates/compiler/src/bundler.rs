//! esbuild-based bundler for server-side JS/TS projects.
//!
//! Handles: TS/TSX/JSX → JS, npm resolution, tree-shaking, CJS→ESM, minification.
//! Requires `esbuild` binary on PATH. Shell-out approach — same as wrangler.

use std::path::Path;
use std::process::Command;

/// Options for the esbuild bundler.
#[derive(Debug, Clone)]
pub struct BundleOptions {
    /// Entry point relative to project_dir (e.g. "src/index.ts").
    pub entry: String,
    /// Minify output (default: true for deploy, false for dev).
    pub minify: bool,
    /// Generate external source map (default: true).
    pub sourcemap: bool,
    /// ES target version (default: "es2022").
    pub target: String,
    /// Packages to exclude from bundle (e.g. platform-provided APIs).
    pub external: Vec<String>,
}

impl Default for BundleOptions {
    fn default() -> Self {
        Self {
            entry: String::new(),
            minify: false, // No minification by default — readable output, clean stack traces.
                           // Tree-shaking handles dead code. V8 bytecode cache makes parse time irrelevant.
            sourcemap: false,
            target: "es2022".to_string(),
            external: Vec::new(),
        }
    }
}

/// Result of a successful bundle.
#[derive(Debug, Clone)]
pub struct BundleResult {
    /// Bundled ESM JavaScript.
    pub js: String,
    /// External source map (if sourcemap=true).
    pub source_map: Option<String>,
    /// Size of the bundled JS in bytes.
    pub size_bytes: usize,
}

/// Errors from the bundling process.
#[derive(Debug)]
pub enum BundleError {
    /// esbuild binary not found on PATH.
    EsbuildNotFound(std::io::Error),
    /// esbuild reported compilation errors.
    CompileError(String),
    /// Entry point not found or not specified.
    EntryNotFound(String),
    /// Filesystem error.
    Io(std::io::Error),
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EsbuildNotFound(e) => write!(f, "esbuild not found on PATH: {e}"),
            Self::CompileError(msg) => write!(f, "esbuild error: {msg}"),
            Self::EntryNotFound(msg) => write!(f, "entry point not found: {msg}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for BundleError {}

/// Check if esbuild is available on PATH.
pub fn esbuild_available() -> bool {
    Command::new("esbuild")
        .arg("--version")
        .output()
        .is_ok()
}

/// Get esbuild version string.
pub fn esbuild_version() -> Option<String> {
    Command::new("esbuild")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}

/// Detect the entry point for a project directory.
///
/// Checks (in order):
/// 1. package.json "module" or "main" field
/// 2. src/index.ts → src/index.js → index.ts → index.js
/// 3. src/server.ts → src/server.js → server.ts → server.js
/// 4. Single .ts/.js file in root
pub fn detect_entry(project_dir: &Path) -> Option<String> {
    // 1. package.json
    let pkg_json = project_dir.join("package.json");
    if pkg_json.exists() {
        if let Ok(content) = std::fs::read_to_string(&pkg_json) {
            if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content) {
                // Prefer "module" (ESM) over "main" (CJS)
                for field in ["module", "main"] {
                    if let Some(entry) = pkg.get(field).and_then(|v| v.as_str()) {
                        let entry_path = project_dir.join(entry);
                        if entry_path.exists() {
                            return Some(entry.to_string());
                        }
                    }
                }
            }
        }
    }

    // 2-3. Well-known entry points
    let candidates = [
        "src/index.ts",
        "src/index.tsx",
        "src/index.js",
        "src/index.jsx",
        "index.ts",
        "index.tsx",
        "index.js",
        "index.jsx",
        "src/server.ts",
        "src/server.tsx",
        "src/server.js",
        "src/server.jsx",
        "server.ts",
        "server.tsx",
        "server.js",
        "server.jsx",
        "src/main.ts",
        "src/main.js",
        "main.ts",
        "main.js",
    ];

    for candidate in candidates {
        if project_dir.join(candidate).exists() {
            return Some(candidate.to_string());
        }
    }

    // 4. Single .ts/.js file in root
    let root_files: Vec<_> = std::fs::read_dir(project_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            (name.ends_with(".ts")
                || name.ends_with(".tsx")
                || name.ends_with(".js")
                || name.ends_with(".jsx"))
                && !name.starts_with('.')
        })
        .collect();

    if root_files.len() == 1 {
        return Some(root_files[0].file_name().to_string_lossy().to_string());
    }

    None
}

/// Bundle a project directory with esbuild.
///
/// Resolves all imports, tree-shakes unused exports, converts TS/CJS to ESM.
/// Outputs a single ESM string ready for V8.
pub fn bundle(project_dir: &Path, options: &BundleOptions) -> Result<BundleResult, BundleError> {
    // Validate entry point
    let entry = if options.entry.is_empty() {
        detect_entry(project_dir).ok_or_else(|| {
            BundleError::EntryNotFound(
                "No entry point specified and auto-detection failed. \
                 Create src/index.ts or specify --entry"
                    .to_string(),
            )
        })?
    } else {
        let entry_path = project_dir.join(&options.entry);
        if !entry_path.exists() {
            return Err(BundleError::EntryNotFound(format!(
                "{} does not exist",
                options.entry
            )));
        }
        options.entry.clone()
    };

    // Build esbuild args
    let mut args = vec![
        entry.clone(),
        "--bundle".to_string(),
        "--format=esm".to_string(),
        "--platform=neutral".to_string(),
        format!("--target={}", options.target),
        "--tree-shaking=true".to_string(),
        // Resolve "main" and "module" fields in package.json (needed for
        // packages like lodash-es that use "main" with --platform=neutral).
        "--main-fields=module,main".to_string(),
        // npm conditional exports — prefer serverless/worker builds
        "--conditions=workerd,worker,browser".to_string(),
        "--log-level=warning".to_string(),
    ];

    if options.minify {
        // Full minify: renames identifiers — smaller but unreadable stack traces
        args.push("--minify".to_string());
    } else {
        // Default: strip whitespace + simplify syntax, but keep identifier names.
        // Readable stack traces with ~33% size reduction.
        args.push("--minify-whitespace".to_string());
        args.push("--minify-syntax".to_string());
    }

    // When sourcemap is requested, use external mode with a temp outfile so
    // esbuild writes both bundled.js and bundled.js.map as separate files.
    let tmp_dir = if options.sourcemap {
        let dir = std::env::temp_dir().join(format!("zeroship-build-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| {
            BundleError::CompileError(format!("Failed to create temp dir: {e}"))
        })?;
        let out_file = dir.join("bundled.js");
        args.push("--sourcemap=external".to_string());
        args.push(format!("--outfile={}", out_file.display()));
        Some(dir)
    } else {
        None
    };

    for ext in &options.external {
        args.push(format!("--external:{ext}"));
    }

    // Run esbuild — output to stdout (or to temp file when sourcemap is enabled)
    let output = Command::new("esbuild")
        .args(&args)
        .current_dir(project_dir)
        .output()
        .map_err(BundleError::EsbuildNotFound)?;

    // Check for errors
    if !output.status.success() {
        if let Some(ref dir) = tmp_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BundleError::CompileError(stderr.trim().to_string()));
    }

    // Capture warnings
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        eprintln!("[bundler] {}", stderr.trim());
    }

    let (js, source_map) = if let Some(ref dir) = tmp_dir {
        // Read from temp files
        let js = std::fs::read_to_string(dir.join("bundled.js")).map_err(|e| {
            BundleError::CompileError(format!("Failed to read esbuild output: {e}"))
        })?;
        let map_path = dir.join("bundled.js.map");
        let source_map = if map_path.exists() {
            Some(std::fs::read_to_string(&map_path).map_err(|e| {
                BundleError::CompileError(format!("Failed to read source map: {e}"))
            })?)
        } else {
            None
        };
        let _ = std::fs::remove_dir_all(dir);
        (js, source_map)
    } else {
        let js = String::from_utf8(output.stdout).map_err(|e| {
            BundleError::CompileError(format!("esbuild output is not valid UTF-8: {e}"))
        })?;
        (js, None)
    };

    let size_bytes = js.len();

    Ok(BundleResult {
        js,
        source_map,
        size_bytes,
    })
}

// ---------------------------------------------------------------------------
// Dual bundle (server + client) for full-stack apps
// ---------------------------------------------------------------------------

/// Result of a dual (server + client) bundle.
#[derive(Debug, Clone)]
pub struct DualBundleResult {
    /// Server bundle — runs in V8 isolate.
    pub server: BundleResult,
    /// Client bundle — runs in browser. None if no client code (API-only app).
    pub client: Option<BundleResult>,
    /// Entry component name (e.g. "App") — the React component to mount.
    pub entry_component: Option<String>,
    /// Server function names (for RPC endpoint registration).
    pub server_functions: Vec<String>,
}

/// Detect client entry point (app.tsx, app.jsx, etc.)
fn detect_client_entry(project_dir: &Path) -> Option<String> {
    let candidates = [
        "src/app.tsx", "src/app.jsx",
        "src/client.tsx", "src/client.jsx",
        "app.tsx", "app.jsx",
    ];
    for candidate in &candidates {
        if project_dir.join(candidate).exists() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Build a full-stack app: server bundle + client bundle.
///
/// 1. Walk all source files (.ts/.tsx/.jsx/.js)
/// 2. SWC splits each file into server + client versions
/// 3. Write split files to temp dirs
/// 4. esbuild bundles server files → server.js
/// 5. esbuild bundles client files → client.js (if client entry exists)
pub fn dual_bundle(
    project_dir: &Path,
    options: &BundleOptions,
) -> Result<DualBundleResult, BundleError> {
    use std::fs;

    // Detect entries
    let server_entry = if options.entry.is_empty() {
        detect_entry(project_dir).ok_or_else(|| {
            BundleError::EntryNotFound("No server entry point found".to_string())
        })?
    } else {
        options.entry.clone()
    };
    let client_entry = detect_client_entry(project_dir);

    // If no client entry, just do a normal server-only bundle
    if client_entry.is_none() {
        let server = bundle(project_dir, options)?;
        return Ok(DualBundleResult {
            server,
            client: None,
            entry_component: None,
            server_functions: Vec::new(),
        });
    }

    let client_entry = client_entry.unwrap();

    // Create temp directories for split output
    let temp_base = std::env::temp_dir().join(format!(
        "zeroship-dual-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    let server_dir = temp_base.join("server");
    let client_dir = temp_base.join("client");
    fs::create_dir_all(&server_dir).map_err(BundleError::Io)?;
    fs::create_dir_all(&client_dir).map_err(BundleError::Io)?;

    // Walk source files and split each with SWC
    let src_dir = project_dir.join("src");
    let source_root = if src_dir.exists() { &src_dir } else { project_dir };
    let mut all_server_fns = Vec::new();
    let mut entry_component = None;

    walk_and_split(source_root, source_root, project_dir, &server_dir, &client_dir,
                   &mut all_server_fns, &mut entry_component)?;

    // Copy node_modules symlink so esbuild can resolve packages
    let nm_src = project_dir.join("node_modules");
    if nm_src.exists() {
        let server_nm = server_dir.join("node_modules");
        let client_nm = client_dir.join("node_modules");
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&nm_src, &server_nm);
            let _ = std::os::unix::fs::symlink(&nm_src, &client_nm);
        }
    }

    // Copy package.json so esbuild respects module resolution
    let pkg = project_dir.join("package.json");
    if pkg.exists() {
        let _ = fs::copy(&pkg, server_dir.join("package.json"));
        let _ = fs::copy(&pkg, client_dir.join("package.json"));
    }

    // Bundle server — keep original extension, esbuild handles TS
    let server_entry_rel = server_entry.replace("src/", "");
    let server_opts = BundleOptions {
        entry: server_entry_rel,
        minify: options.minify,
        sourcemap: options.sourcemap,
        target: options.target.clone(),
        external: options.external.clone(),
    };
    let server = bundle(&server_dir, &server_opts)?;

    // Bundle client (for browser) — keep original extension
    let client_entry_rel = client_entry.replace("src/", "");
    let client_opts = BundleOptions {
        entry: client_entry_rel,
        minify: options.minify,
        sourcemap: false,
        target: options.target.clone(),
        external: vec!["react".to_string(), "react-dom".to_string()],
    };
    let client = match bundle(&client_dir, &client_opts) {
        Ok(result) => Some(result),
        Err(_) => None,
    };

    // Cleanup temp dirs
    let _ = fs::remove_dir_all(&temp_base);

    Ok(DualBundleResult {
        server,
        client,
        entry_component,
        server_functions: all_server_fns,
    })
}

/// Walk source files recursively, split each with SWC, write to server/client dirs.
fn walk_and_split(
    dir: &Path,
    source_root: &Path,
    project_root: &Path,
    server_dir: &Path,
    client_dir: &Path,
    server_fns: &mut Vec<String>,
    entry_component: &mut Option<String>,
) -> Result<(), BundleError> {
    let entries = std::fs::read_dir(dir).map_err(BundleError::Io)?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip node_modules, .git, dist
            let name = path.file_name().unwrap().to_string_lossy();
            if name == "node_modules" || name == ".git" || name == "dist" {
                continue;
            }
            // Create mirror dirs
            let rel = path.strip_prefix(source_root).unwrap();
            std::fs::create_dir_all(server_dir.join(rel)).map_err(BundleError::Io)?;
            std::fs::create_dir_all(client_dir.join(rel)).map_err(BundleError::Io)?;
            walk_and_split(&path, source_root, project_root, server_dir, client_dir,
                          server_fns, entry_component)?;
        } else if path.is_file() {
            let name = path.file_name().unwrap().to_string_lossy();
            if name.ends_with(".ts") || name.ends_with(".tsx")
               || name.ends_with(".js") || name.ends_with(".jsx")
            {
                let source = std::fs::read_to_string(&path).map_err(BundleError::Io)?;
                let rel = path.strip_prefix(source_root).unwrap();

                // Compile with SWC
                let result = crate::compile_with_options(
                    &source,
                    crate::Target::Node,
                    false,
                    Some(project_root),
                );

                // Write original source to both dirs — esbuild handles TS→JS.
                // SWC analysis tells us which functions are server/client,
                // but we let esbuild do the actual bundling + TS stripping.
                // TODO: Write split versions when SWC emits clean JS.
                let ext = rel.extension().unwrap_or_default().to_string_lossy().to_string();
                let server_path = server_dir.join(rel);
                if let Some(parent) = server_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&server_path, &source).map_err(BundleError::Io)?;

                let client_path = client_dir.join(rel);
                if let Some(parent) = client_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&client_path, &source).map_err(BundleError::Io)?;

                let _ = ext; // suppress unused warning

                // Collect metadata
                server_fns.extend(result.server_functions);
                if entry_component.is_none() {
                    *entry_component = result.entry_component;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Unique temp dir per test (avoids cross-test contamination).
    fn temp_dir() -> std::path::PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "zeroship-bundler-test-{}-{}",
            std::process::id(),
            id
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Skip test if esbuild is not on PATH.
    macro_rules! require_esbuild {
        () => {
            if !esbuild_available() {
                eprintln!("SKIPPED: esbuild not on PATH");
                return;
            }
        };
    }

    #[test]
    fn esbuild_check() {
        if esbuild_available() {
            let version = esbuild_version().unwrap();
            eprintln!("esbuild version: {version}");
        } else {
            eprintln!("esbuild not found — bundle tests will be skipped");
        }
    }

    #[test]
    fn bundle_single_file() {
        require_esbuild!();
        let dir = temp_dir();
        fs::write(
            dir.join("index.js"),
            r#"export function hello() { return "world"; }"#,
        )
        .unwrap();

        let result = bundle(
            &dir,
            &BundleOptions {
                entry: "index.js".to_string(),
                minify: false,
                sourcemap: false,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(result.js.contains("hello"));
        assert!(result.js.contains("world"));
        assert!(result.size_bytes > 0);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bundle_multi_file_with_imports() {
        require_esbuild!();
        let dir = temp_dir();
        fs::create_dir_all(dir.join("src")).unwrap();

        fs::write(
            dir.join("src/index.ts"),
            r#"
import { greet } from "./utils";
export function onRequest(req: Request): Response {
    return new Response(greet("zeroship"));
}
"#,
        )
        .unwrap();

        fs::write(
            dir.join("src/utils.ts"),
            r#"
export function greet(name: string): string {
    return `Hello, ${name}!`;
}
export function unused(): string {
    return "this should be tree-shaken";
}
"#,
        )
        .unwrap();

        let result = bundle(
            &dir,
            &BundleOptions {
                entry: "src/index.ts".to_string(),
                minify: false,
                sourcemap: false,
                ..Default::default()
            },
        )
        .unwrap();

        // greet is used → included
        assert!(result.js.contains("greet") || result.js.contains("Hello"));
        // unused is tree-shaken → NOT included
        assert!(
            !result.js.contains("tree-shaken"),
            "unused function should be tree-shaken: {}",
            result.js
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bundle_typescript_stripped() {
        require_esbuild!();
        let dir = temp_dir();
        fs::write(
            dir.join("index.ts"),
            r#"
interface Config {
    name: string;
    port: number;
}
const config: Config = { name: "test", port: 3000 };
export function getConfig(): Config { return config; }
"#,
        )
        .unwrap();

        let result = bundle(
            &dir,
            &BundleOptions {
                entry: "index.ts".to_string(),
                minify: false,
                sourcemap: false,
                ..Default::default()
            },
        )
        .unwrap();

        // TypeScript types stripped
        assert!(!result.js.contains("interface"));
        assert!(!result.js.contains(": Config"));
        // Runtime code preserved
        assert!(result.js.contains("getConfig"));
        assert!(result.js.contains("test"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_entry_src_index() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/index.ts"), "").unwrap();

        assert_eq!(detect_entry(&dir), Some("src/index.ts".to_string()));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_entry_package_json() {
        let dir = temp_dir();
        fs::write(
            dir.join("package.json"),
            r#"{"module": "dist/server.js"}"#,
        )
        .unwrap();
        fs::create_dir_all(dir.join("dist")).unwrap();
        fs::write(dir.join("dist/server.js"), "").unwrap();

        assert_eq!(detect_entry(&dir), Some("dist/server.js".to_string()));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_entry_single_file() {
        let dir = temp_dir();
        fs::write(dir.join("app.ts"), "export function handler() {}").unwrap();

        assert_eq!(detect_entry(&dir), Some("app.ts".to_string()));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bundle_error_missing_entry() {
        let dir = temp_dir();

        let err = bundle(
            &dir,
            &BundleOptions {
                entry: "nonexistent.ts".to_string(),
                ..Default::default()
            },
        )
        .unwrap_err();

        assert!(matches!(err, BundleError::EntryNotFound(_)));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bundle_with_minify() {
        require_esbuild!();
        let dir = temp_dir();
        fs::write(
            dir.join("index.js"),
            r#"
export function longFunctionName() {
    const veryLongVariableName = "hello";
    return veryLongVariableName;
}
"#,
        )
        .unwrap();

        let unminified = bundle(
            &dir,
            &BundleOptions {
                entry: "index.js".to_string(),
                minify: false,
                sourcemap: false,
                ..Default::default()
            },
        )
        .unwrap();

        let minified = bundle(
            &dir,
            &BundleOptions {
                entry: "index.js".to_string(),
                minify: true,
                sourcemap: false,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(
            minified.size_bytes < unminified.size_bytes,
            "minified ({}) should be smaller than unminified ({})",
            minified.size_bytes,
            unminified.size_bytes
        );

        fs::remove_dir_all(&dir).ok();
    }
}
