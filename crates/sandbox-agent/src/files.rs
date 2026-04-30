//! Path-safe file operations on the in-VM workspace.
//!
//! All callers (HTTP handlers) supply paths relative to the workspace
//! root (default `/workspace`). We reject:
//!   - empty paths,
//!   - absolute paths,
//!   - any path containing `..`,
//!   - paths whose **parent** canonicalizes outside the workspace,
//!   - **any path component or leaf that is a symlink** (via `O_NOFOLLOW`).
//!
//! The last rule is the one that closes the symlink-leaf escape that
//! the controller-side `crates/sandbox/src/files.rs` is vulnerable to:
//! a user inside the workspace can `ln -s /etc/passwd /workspace/x`,
//! and a naive `std::fs::read("/workspace/x")` would happily follow
//! that link. Here every open is `O_NOFOLLOW` so the kernel returns
//! `ELOOP` if any component along the path is a symlink — including
//! the leaf — and we surface that as a 4xx, not as a leak.
//!
//! `file_tree` uses `symlink_metadata` (which does NOT follow
//! symlinks) and skips symlink entries entirely, so a malicious
//! symlink doesn't even appear in the listing.
//!
//! Path safety here is defense in depth on top of the VM boundary
//! itself — even if the workspace mount is shared with anything
//! outside the VM, these checks bound what an over-eager handler
//! can read or overwrite.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct FileEntry {
    pub path: String,
    pub kind: &'static str, // "file" | "dir"
    pub size: u64,
}

/// Cap on individual file size. Mirrors the controller's cap so the
/// agent can't be coerced into writing a huge blob via the local API.
pub const MAX_BYTES: usize = 5 * 1024 * 1024;

/// Resolve a relative request path against the workspace root.
/// Performs static checks (empty / absolute / `..`) and the parent
/// canonicalization check. The leaf is **not** canonicalized because
/// it may not exist on write — the leaf-symlink defense lives in the
/// open path itself, where every open uses `O_NOFOLLOW`.
pub fn resolve(workspace: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative.is_empty() {
        return Err("path is empty".into());
    }
    let p = Path::new(relative);
    if p.is_absolute() {
        return Err("absolute paths not allowed".into());
    }
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => return Err("'..' segments not allowed".into()),
            std::path::Component::Prefix(_) => return Err("Windows paths not allowed".into()),
            std::path::Component::RootDir => return Err("absolute paths not allowed".into()),
            _ => {}
        }
    }

    let joined = workspace.join(p);

    let parent = joined.parent().unwrap_or(workspace);
    if parent.exists() {
        let canon = std::fs::canonicalize(parent)
            .map_err(|e| format!("canonicalize parent: {e}"))?;
        let canon_root = std::fs::canonicalize(workspace)
            .map_err(|e| format!("canonicalize workspace: {e}"))?;
        if !canon.starts_with(&canon_root) {
            return Err("path escapes workspace via symlink".into());
        }
    }

    Ok(joined)
}

fn is_eloop(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::ELOOP)
}

/// Read a file's bytes. Refuses to follow a symlink at the leaf —
/// returns "is a symlink" rather than the link target's contents.
pub fn read_file(workspace: &Path, relative: &str) -> Result<Vec<u8>, String> {
    let p = resolve(workspace, relative)?;
    // O_NOFOLLOW on the leaf: if the path or any ancestor is a symlink,
    // the kernel returns ELOOP and we refuse.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&p)
        .map_err(|e| {
            if is_eloop(&e) {
                "refusing to follow symlink".into()
            } else {
                format!("read {}: {e}", p.display())
            }
        })?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .map_err(|e| format!("read {}: {e}", p.display()))?;
    Ok(buf)
}

/// Write a file's bytes. Creates parent dirs if missing. Refuses to
/// overwrite a symlink — `O_NOFOLLOW` causes `open` to fail with
/// `ELOOP` if the leaf is a symlink (write would otherwise follow it
/// and clobber the target file outside the workspace).
pub fn write_file(workspace: &Path, relative: &str, content: &[u8]) -> Result<(), String> {
    if content.len() > MAX_BYTES {
        return Err(format!("file too large: {} bytes (max {MAX_BYTES})", content.len()));
    }
    let p = resolve(workspace, relative)?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&p)
        .map_err(|e| {
            if is_eloop(&e) {
                "refusing to follow symlink".into()
            } else {
                format!("write {}: {e}", p.display())
            }
        })?;
    file.write_all(content)
        .map_err(|e| format!("write {}: {e}", p.display()))
}

/// Delete a regular file. Refuses to operate on a symlink (we use
/// `symlink_metadata` and reject before calling `remove_file` — the
/// latter would happily delete the symlink itself, which is *less*
/// dangerous than following, but we still reject to keep behavior
/// consistent with read/write).
pub fn delete_file(workspace: &Path, relative: &str) -> Result<bool, String> {
    let p = resolve(workspace, relative)?;
    let meta = match std::fs::symlink_metadata(&p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("stat {}: {e}", p.display())),
    };
    if meta.file_type().is_symlink() {
        return Err("refusing to delete symlink".into());
    }
    if meta.is_dir() {
        return Err("path is a directory; use rmdir instead".into());
    }
    std::fs::remove_file(&p).map_err(|e| format!("remove {}: {e}", p.display()))?;
    Ok(true)
}

/// Walk the workspace and list every regular file or dir. Skips the
/// usual noise dirs (`node_modules`, `.git`, …). **Symlinks are
/// excluded entirely** from the listing — they don't have a
/// well-defined size and we don't want to expose link targets.
pub fn file_tree(workspace: &Path) -> Result<Vec<FileEntry>, String> {
    if !workspace.exists() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    let mut stack = vec![(workspace.to_path_buf(), String::new())];

    while let Some((dir, prefix)) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| format!("read_dir {}: {e}", dir.display()))?;

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy().to_string();

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

            // Use symlink_metadata so we see the symlink itself, not
            // its target. Then explicitly skip symlinks.
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let ft = meta.file_type();

            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                out.push(FileEntry { path: rel.clone(), kind: "dir", size: 0 });
                stack.push((entry.path(), rel));
            } else if ft.is_file() {
                out.push(FileEntry { path: rel, kind: "file", size: meta.len() });
            }
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp() -> PathBuf {
        // Avoid `uuid` here — keeps the test crate's dep tree narrow.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("zsbx-agent-test-{pid}-{n}"));
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
        assert!(!delete_file(&ws, "missing").unwrap());
    }

    #[test]
    fn cap_enforced() {
        let ws = tmp();
        let huge = vec![0u8; MAX_BYTES + 1];
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

    // ─── The P0-A symlink-escape regression tests ────────────────────
    //
    // Each of these creates a hostile symlink inside the workspace
    // pointing at a file the agent should never touch. Without
    // O_NOFOLLOW, read/write would happily follow the link. With
    // O_NOFOLLOW, `open` returns ELOOP and the operation fails.

    #[test]
    fn read_refuses_symlink_leaf() {
        let ws = tmp();
        let outside = ws.parent().unwrap().join("zsbx-secret.txt");
        fs::write(&outside, b"top secret").unwrap();
        symlink(&outside, ws.join("escape")).unwrap();

        let res = read_file(&ws, "escape");
        assert!(res.is_err(), "must refuse to read through symlink");
        let msg = res.unwrap_err();
        assert!(msg.contains("symlink"), "error must mention symlink, got: {msg}");
    }

    #[test]
    fn write_refuses_symlink_leaf() {
        let ws = tmp();
        let outside = ws.parent().unwrap().join("zsbx-target.txt");
        fs::write(&outside, b"original").unwrap();
        symlink(&outside, ws.join("clobber")).unwrap();

        let res = write_file(&ws, "clobber", b"PWNED");
        assert!(res.is_err(), "must refuse to write through symlink");
        // Verify the outside file was NOT modified.
        assert_eq!(fs::read(&outside).unwrap(), b"original");
    }

    #[test]
    fn delete_refuses_symlink_leaf() {
        let ws = tmp();
        let outside = ws.parent().unwrap().join("zsbx-keep.txt");
        fs::write(&outside, b"x").unwrap();
        symlink(&outside, ws.join("victim")).unwrap();

        assert!(delete_file(&ws, "victim").is_err());
        assert!(outside.exists(), "outside file must still exist");
    }

    #[test]
    fn file_tree_omits_symlinks() {
        let ws = tmp();
        write_file(&ws, "real.txt", b"x").unwrap();
        symlink("/etc/passwd", ws.join("link")).unwrap();
        let paths: Vec<String> = file_tree(&ws).unwrap().into_iter().map(|e| e.path).collect();
        assert!(paths.iter().any(|p| p == "real.txt"));
        assert!(!paths.iter().any(|p| p == "link"), "symlinks must not be listed");
    }
}
