//! File operations on the bind-mounted workspace.
//!
//! The agent's `read_file` / `write_file` / `list_files` tools all
//! land here. We operate on the host-side bind-mount path directly
//! (no `docker exec`) — much faster, no shell-escape concerns, and
//! the workspace is the same on both sides of the mount.
//!
//! All paths are validated to stay inside the per-session workspace
//! root; symlinks that escape the workspace are rejected.

use std::path::{Path, PathBuf};

#[derive(Debug, serde::Serialize)]
pub struct FileEntry {
    pub path: String,
    pub kind: &'static str, // "file" | "dir"
    pub size: u64,
}

/// Resolve a request-supplied relative path against the workspace root.
/// Rejects any input that resolves outside the root, contains `..`, is
/// absolute, or follows a symlink that escapes.
pub fn resolve(workspace: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative.is_empty() {
        return Err("path is empty".to_string());
    }
    let p = Path::new(relative);
    if p.is_absolute() {
        return Err("absolute paths not allowed".to_string());
    }
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => return Err("'..' segments not allowed".to_string()),
            std::path::Component::Prefix(_) => return Err("Windows paths not allowed".to_string()),
            std::path::Component::RootDir => return Err("absolute paths not allowed".to_string()),
            _ => {}
        }
    }

    let joined = workspace.join(p);

    // canonicalize the parent to follow symlinks that exist, but the
    // file itself may not yet exist (write-new case).
    let parent = joined.parent().unwrap_or(workspace);
    if parent.exists() {
        let canon = std::fs::canonicalize(parent)
            .map_err(|e| format!("canonicalize parent: {e}"))?;
        let canon_root = std::fs::canonicalize(workspace)
            .map_err(|e| format!("canonicalize workspace: {e}"))?;
        if !canon.starts_with(&canon_root) {
            return Err("path escapes workspace via symlink".to_string());
        }
    }

    Ok(joined)
}

/// Read a file's bytes.
pub fn read_file(workspace: &Path, relative: &str) -> Result<Vec<u8>, String> {
    let p = resolve(workspace, relative)?;
    std::fs::read(&p).map_err(|e| format!("read {}: {e}", p.display()))
}

/// Write a file's bytes (creating parents as needed). Caps file size
/// to limit DoS via huge agent-generated bodies.
pub fn write_file(workspace: &Path, relative: &str, content: &[u8]) -> Result<(), String> {
    const MAX_BYTES: usize = 5 * 1024 * 1024;
    if content.len() > MAX_BYTES {
        return Err(format!("file too large: {} bytes (max {MAX_BYTES})", content.len()));
    }
    let p = resolve(workspace, relative)?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::write(&p, content).map_err(|e| format!("write {}: {e}", p.display()))
}

/// Delete a file (NOT a directory). Returns Ok(false) if the file
/// didn't exist; Err on real failures.
pub fn delete_file(workspace: &Path, relative: &str) -> Result<bool, String> {
    let p = resolve(workspace, relative)?;
    if !p.exists() {
        return Ok(false);
    }
    if p.is_dir() {
        return Err("path is a directory; use rmdir instead".to_string());
    }
    std::fs::remove_file(&p).map_err(|e| format!("remove {}: {e}", p.display()))?;
    Ok(true)
}

/// Walk the workspace and list every file/dir entry. Skips
/// `node_modules`, `.git`, `dist`, `.next` — the agent doesn't need
/// them and they'd make the response huge.
pub fn file_tree(workspace: &Path) -> Result<Vec<FileEntry>, String> {
    if !workspace.exists() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    let mut stack = vec![(workspace.to_path_buf(), String::new())];

    while let Some((dir, prefix)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => return Err(format!("read_dir {}: {e}", dir.display())),
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy().to_string();

            // Skip noise — never useful to the agent.
            if matches!(
                name_str.as_str(),
                "node_modules" | ".git" | "dist" | ".next" | ".turbo" | ".cache",
            ) {
                continue;
            }

            let rel = if prefix.is_empty() {
                name_str.clone()
            } else {
                format!("{prefix}/{name_str}")
            };

            let ft = match entry.file_type() {
                Ok(f) => f,
                Err(_) => continue,
            };

            if ft.is_dir() {
                out.push(FileEntry { path: rel.clone(), kind: "dir", size: 0 });
                stack.push((entry.path(), rel));
            } else if ft.is_file() {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                out.push(FileEntry { path: rel, kind: "file", size });
            }
            // Symlinks: ignored to avoid escapes.
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zeroship-sandbox-test-{}",
            uuid::Uuid::new_v4().simple(),
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_then_read() {
        let ws = tmp();
        write_file(&ws, "hello.txt", b"world").unwrap();
        assert_eq!(read_file(&ws, "hello.txt").unwrap(), b"world");
    }

    #[test]
    fn write_creates_parents() {
        let ws = tmp();
        write_file(&ws, "src/server.ts", b"x").unwrap();
        assert!(ws.join("src/server.ts").exists());
    }

    #[test]
    fn rejects_parent_dir() {
        let ws = tmp();
        assert!(write_file(&ws, "../boom", b"x").is_err());
    }

    #[test]
    fn rejects_absolute() {
        let ws = tmp();
        assert!(write_file(&ws, "/etc/passwd", b"x").is_err());
    }

    #[test]
    fn rejects_empty() {
        let ws = tmp();
        assert!(read_file(&ws, "").is_err());
    }

    #[test]
    fn delete_idempotent_for_missing() {
        let ws = tmp();
        assert_eq!(delete_file(&ws, "missing").unwrap(), false);
    }

    #[test]
    fn cap_enforced() {
        let ws = tmp();
        let huge = vec![0u8; 6 * 1024 * 1024];
        assert!(write_file(&ws, "big", &huge).is_err());
    }

    #[test]
    fn file_tree_skips_noise() {
        let ws = tmp();
        write_file(&ws, "src/main.ts", b"a").unwrap();
        write_file(&ws, "node_modules/foo/index.js", b"b").unwrap();
        let entries = file_tree(&ws).unwrap();
        let paths: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"src/main.ts"));
        assert!(!paths.iter().any(|p| p.contains("node_modules")));
    }
}
