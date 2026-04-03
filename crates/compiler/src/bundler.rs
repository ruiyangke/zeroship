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
            minify: true,
            sourcemap: true,
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
        args.push("--minify".to_string());
    }

    if options.sourcemap {
        args.push("--sourcemap=inline".to_string());
    }

    for ext in &options.external {
        args.push(format!("--external:{ext}"));
    }

    // Run esbuild — output to stdout
    let output = Command::new("esbuild")
        .args(&args)
        .current_dir(project_dir)
        .output()
        .map_err(BundleError::EsbuildNotFound)?;

    // Check for errors
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BundleError::CompileError(stderr.trim().to_string()));
    }

    // Capture warnings
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        eprintln!("[bundler] {}", stderr.trim());
    }

    let js = String::from_utf8(output.stdout).map_err(|e| {
        BundleError::CompileError(format!("esbuild output is not valid UTF-8: {e}"))
    })?;

    let size_bytes = js.len();

    Ok(BundleResult {
        js,
        source_map: None, // inline sourcemap is embedded in js when enabled
        size_bytes,
    })
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
            "appbase-bundler-test-{}-{}",
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
    return new Response(greet("appbase"));
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
