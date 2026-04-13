//! Filesystem storage for app bundles.
//!
//! Layout:
//! ```text
//! {base_dir}/
//!   source/
//!     server.ts            ← entrypoint
//!     utils.ts             ← additional modules
//!     types.ts             ← type definitions
//!   builds/
//!     v1/
//!       server.js          ← bundled output
//!       server.js.cache    ← V8 bytecode cache
//!       client/
//!         index.html
//!       meta.json
//!     v2/ ...
//!   current → builds/v2   ← symlink to active version
//! ```

use std::path::PathBuf;

/// Errors from storage operations.
#[derive(Debug)]
pub enum StorageError {
    Io(std::io::Error),
    NotFound(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "storage I/O error: {e}"),
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
        }
    }
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Filesystem storage for a single app's bundles.
pub struct AppStorage {
    base_dir: PathBuf,
}

impl AppStorage {
    /// Create a new storage handle. Does NOT create directories yet.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Deploy a new version. Returns the version number.
    ///
    /// - `server_js`: compiled server bundle
    /// - `client_html`: optional client HTML bundle
    ///
    /// Creates a new build directory, writes artifacts, updates the `current` symlink.
    /// Deletes any stale bytecode cache to force recompilation.
    pub fn deploy(
        &self,
        server_js: &str,
        client_html: Option<&[u8]>,
    ) -> Result<u64, StorageError> {
        let version = self.next_version();
        let build_dir = self.build_dir(version);

        std::fs::create_dir_all(&build_dir)?;

        // Write server bundle
        std::fs::write(build_dir.join("server.js"), server_js)?;

        // Write client assets
        if let Some(html) = client_html {
            let client_dir = build_dir.join("client");
            std::fs::create_dir_all(&client_dir)?;
            std::fs::write(client_dir.join("index.html"), html)?;
        }

        // Write metadata
        let meta = format!(
            r#"{{"version":{},"source_size":{},"timestamp":"{}"}}"#,
            version,
            server_js.len(),
            chrono_now(),
        );
        std::fs::write(build_dir.join("meta.json"), meta)?;

        // Update current symlink
        self.update_current(version)?;

        Ok(version)
    }

    /// Save original source file (for AI editing / viewing).
    pub fn save_source(&self, filename: &str, content: &str) -> Result<(), StorageError> {
        let source_dir = self.base_dir.join("source");
        std::fs::create_dir_all(&source_dir)?;
        std::fs::write(source_dir.join(filename), content)?;
        Ok(())
    }

    /// Path to the active server.js bundle.
    pub fn current_server_js(&self) -> PathBuf {
        self.base_dir.join("current").join("server.js")
    }

    /// Path to the bytecode cache for the active version.
    pub fn current_cache_path(&self) -> PathBuf {
        self.base_dir.join("current").join("server.js.cache")
    }

    /// Path to the active client directory.
    pub fn current_client_dir(&self) -> PathBuf {
        self.base_dir.join("current").join("client")
    }

    /// Get the currently active version number.
    pub fn current_version(&self) -> Option<u64> {
        let current = self.base_dir.join("current");
        let target = std::fs::read_link(&current).ok()?;
        let dir_name = target.file_name()?.to_str()?;
        dir_name.strip_prefix('v')?.parse().ok()
    }

    /// List all available versions.
    pub fn versions(&self) -> Vec<u64> {
        let builds_dir = self.base_dir.join("builds");
        let mut versions = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&builds_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if let Some(v) = name.strip_prefix('v') {
                        if let Ok(n) = v.parse::<u64>() {
                            versions.push(n);
                        }
                    }
                }
            }
        }
        versions.sort();
        versions
    }

    /// Roll back to a previous version.
    pub fn rollback(&self, version: u64) -> Result<(), StorageError> {
        let build_dir = self.build_dir(version);
        if !build_dir.exists() {
            return Err(StorageError::NotFound(format!("version {version}")));
        }
        // Delete bytecode cache to force recompile with the old code
        let _ = std::fs::remove_file(build_dir.join("server.js.cache"));
        self.update_current(version)
    }

    /// Check if any version has been deployed.
    pub fn has_builds(&self) -> bool {
        self.base_dir.join("current").join("server.js").exists()
    }

    /// Path to source directory.
    pub fn source_dir(&self) -> PathBuf {
        self.base_dir.join("source")
    }

    // --- internal ---

    fn build_dir(&self, version: u64) -> PathBuf {
        self.base_dir.join("builds").join(format!("v{version}"))
    }

    fn next_version(&self) -> u64 {
        self.versions().last().map(|v| v + 1).unwrap_or(1)
    }

    fn update_current(&self, version: u64) -> Result<(), StorageError> {
        let current = self.base_dir.join("current");
        let _ = std::fs::remove_file(&current);
        #[cfg(unix)]
        {
            let target = self.build_dir(version);
            std::os::unix::fs::symlink(&target, &current)?;
        }
        #[cfg(not(unix))]
        {
            // Windows fallback: write version number to a file
            std::fs::write(&current, format!("v{version}"))?;
        }
        Ok(())
    }
}

fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", now.as_secs())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("zeroship-storage-test-{id}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn deploy_creates_build() {
        let dir = temp_dir();
        let store = AppStorage::new(&dir);
        let v = store.deploy("var x = 1;", None).unwrap();
        assert_eq!(v, 1);
        assert!(store.current_server_js().exists());
        assert_eq!(
            std::fs::read_to_string(store.current_server_js()).unwrap(),
            "var x = 1;"
        );
    }

    #[test]
    fn deploy_increments_version() {
        let dir = temp_dir();
        let store = AppStorage::new(&dir);
        assert_eq!(store.deploy("v1", None).unwrap(), 1);
        assert_eq!(store.deploy("v2", None).unwrap(), 2);
        assert_eq!(store.deploy("v3", None).unwrap(), 3);
        assert_eq!(store.current_version(), Some(3));
        assert_eq!(
            std::fs::read_to_string(store.current_server_js()).unwrap(),
            "v3"
        );
    }

    #[test]
    fn versions_lists_all() {
        let dir = temp_dir();
        let store = AppStorage::new(&dir);
        store.deploy("a", None).unwrap();
        store.deploy("b", None).unwrap();
        store.deploy("c", None).unwrap();
        assert_eq!(store.versions(), vec![1, 2, 3]);
    }

    #[test]
    fn rollback_switches_version() {
        let dir = temp_dir();
        let store = AppStorage::new(&dir);
        store.deploy("v1-code", None).unwrap();
        store.deploy("v2-code", None).unwrap();
        assert_eq!(store.current_version(), Some(2));

        store.rollback(1).unwrap();
        assert_eq!(store.current_version(), Some(1));
        assert_eq!(
            std::fs::read_to_string(store.current_server_js()).unwrap(),
            "v1-code"
        );
    }

    #[test]
    fn rollback_nonexistent_fails() {
        let dir = temp_dir();
        let store = AppStorage::new(&dir);
        assert!(store.rollback(99).is_err());
    }

    #[test]
    fn deploy_with_client_html() {
        let dir = temp_dir();
        let store = AppStorage::new(&dir);
        store.deploy("server", Some(b"<html>hi</html>")).unwrap();
        let html = std::fs::read_to_string(
            store.current_client_dir().join("index.html"),
        ).unwrap();
        assert_eq!(html, "<html>hi</html>");
    }

    #[test]
    fn save_source() {
        let dir = temp_dir();
        let store = AppStorage::new(&dir);
        store.save_source("app.tsx", "export default function App() {}").unwrap();
        let content = std::fs::read_to_string(store.source_dir().join("app.tsx")).unwrap();
        assert!(content.contains("App"));
    }

    #[test]
    fn has_builds() {
        let dir = temp_dir();
        let store = AppStorage::new(&dir);
        assert!(!store.has_builds());
        store.deploy("code", None).unwrap();
        assert!(store.has_builds());
    }
}
