//! Virtual filesystem for .appbundle storage.

use std::fmt;
use std::fs;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during VFS operations.
#[derive(Debug)]
pub enum VfsError {
    /// The requested bundle was not found.
    NotFound(String),
    /// An underlying I/O error occurred.
    Io(std::io::Error),
    /// A storage-level error occurred.
    Storage(String),
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VfsError::NotFound(id) => write!(f, "bundle not found: {id}"),
            VfsError::Io(e) => write!(f, "I/O error: {e}"),
            VfsError::Storage(msg) => write!(f, "storage error: {msg}"),
        }
    }
}

impl std::error::Error for VfsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VfsError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for VfsError {
    fn from(e: std::io::Error) -> Self {
        VfsError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Result alias
// ---------------------------------------------------------------------------

/// Result type for VFS operations.
pub type VfsResult<T> = Result<T, VfsError>;

// ---------------------------------------------------------------------------
// BundleStore trait
// ---------------------------------------------------------------------------

/// Abstraction over storage backends for `.appbundle` files.
///
/// All methods are synchronous; implementations are expected to use blocking I/O.
pub trait BundleStore: Send + Sync {
    /// Store a bundle for `app_id`, overwriting any previous data.
    fn put(&self, app_id: &str, data: &[u8]) -> VfsResult<()>;

    /// Retrieve the bundle for `app_id`.
    ///
    /// Returns [`VfsError::NotFound`] if the bundle does not exist.
    fn get(&self, app_id: &str) -> VfsResult<Vec<u8>>;

    /// Delete the bundle for `app_id`.
    ///
    /// Returns [`VfsError::NotFound`] if the bundle does not exist.
    fn delete(&self, app_id: &str) -> VfsResult<()>;

    /// Return `true` if a bundle exists for `app_id`, `false` otherwise.
    fn exists(&self, app_id: &str) -> VfsResult<bool>;

    /// Store a static asset for `app_id` at the given `path`.
    fn put_asset(&self, app_id: &str, path: &str, data: &[u8]) -> VfsResult<()>;

    /// Retrieve a static asset for `app_id` at the given `path`.
    ///
    /// Returns [`VfsError::NotFound`] if the asset does not exist.
    fn get_asset(&self, app_id: &str, path: &str) -> VfsResult<Vec<u8>>;
}

// ---------------------------------------------------------------------------
// LocalFs implementation
// ---------------------------------------------------------------------------

/// A [`BundleStore`] backed by the local filesystem.
///
/// Layout: `{base_dir}/{app_id}/bundle.appbundle`
#[derive(Debug)]
pub struct LocalFs {
    base_dir: PathBuf,
}

impl LocalFs {
    /// Create a new `LocalFs` rooted at `base_dir`.
    ///
    /// The directory is created (including all parents) if it does not exist.
    pub fn new(base_dir: impl Into<PathBuf>) -> VfsResult<Self> {
        let base_dir = base_dir.into();
        fs::create_dir_all(&base_dir)?;
        Ok(Self { base_dir })
    }

    /// Return the path to the bundle file for `app_id`.
    fn bundle_path(&self, app_id: &str) -> PathBuf {
        self.base_dir.join(app_id).join("bundle.appbundle")
    }

    /// Return the path to a static asset for `app_id`.
    ///
    /// Layout: `{base_dir}/{app_id}/assets/{path}`
    fn asset_path(&self, app_id: &str, path: &str) -> PathBuf {
        self.base_dir.join(app_id).join("assets").join(path)
    }
}

impl BundleStore for LocalFs {
    fn put(&self, app_id: &str, data: &[u8]) -> VfsResult<()> {
        let path = self.bundle_path(app_id);
        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, data)?;
        Ok(())
    }

    fn get(&self, app_id: &str) -> VfsResult<Vec<u8>> {
        let path = self.bundle_path(app_id);
        match fs::read(&path) {
            Ok(data) => Ok(data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(VfsError::NotFound(app_id.to_string()))
            }
            Err(e) => Err(VfsError::Io(e)),
        }
    }

    fn delete(&self, app_id: &str) -> VfsResult<()> {
        let app_dir = self.base_dir.join(app_id);
        match fs::remove_dir_all(&app_dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(VfsError::NotFound(app_id.to_string()))
            }
            Err(e) => Err(VfsError::Io(e)),
        }
    }

    fn exists(&self, app_id: &str) -> VfsResult<bool> {
        Ok(self.bundle_path(app_id).exists())
    }

    fn put_asset(&self, app_id: &str, path: &str, data: &[u8]) -> VfsResult<()> {
        let full = self.asset_path(app_id, path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&full, data)?;
        Ok(())
    }

    fn get_asset(&self, app_id: &str, path: &str) -> VfsResult<Vec<u8>> {
        let full = self.asset_path(app_id, path);
        match fs::read(&full) {
            Ok(data) => Ok(data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(VfsError::NotFound(format!("{app_id}/assets/{path}")))
            }
            Err(e) => Err(VfsError::Io(e)),
        }
    }
}
