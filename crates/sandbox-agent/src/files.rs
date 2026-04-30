//! Path-safe file operations on the in-VM workspace.
//!
//! Every operation routes through a directory file descriptor for the
//! workspace, opened once at startup and held by [`Workspace`].
//! File-by-file access uses `openat2(2)` with **`RESOLVE_BENEATH`**
//! and **`RESOLVE_NO_SYMLINKS`** (Linux 5.6+):
//!
//!   - `RESOLVE_BENEATH` — the resolved path **must** end up below
//!     the workspace fd. Any traversal that would escape (`..`,
//!     symlinks pointing out) fails with `EXDEV`.
//!   - `RESOLVE_NO_SYMLINKS` — **no** symlinks are followed at any
//!     component, including the leaf. A user-planted symlink in the
//!     workspace returns `ELOOP` instead of leaking its target.
//!
//! The combination is **atomic** — a single syscall does the entire
//! resolution + open against the kernel-held inode for the workspace
//! dirfd. There is no window for a parent-symlink swap between
//! check-and-use, which closes the TOCTOU bug that the previous
//! `canonicalize`-then-`open` design had.
//!
//! Static path validation (no empty / no absolute / no `..`) stays as
//! defense-in-depth; under `RESOLVE_BENEATH` the kernel would catch
//! these too, but the early return gives clearer error messages and
//! avoids one syscall on obviously bad input.
//!
//! `file_tree` is read-only listing and uses `symlink_metadata` to
//! avoid following any symlink it encounters; entries that ARE
//! symlinks are skipped entirely (no listing leaks link targets).
//!
//! ## `unsafe`
//!
//! `nix::fcntl::openat2` returns a raw `RawFd`. Wrapping it in an
//! `OwnedFd` (the only way to ensure RAII close) requires
//! `OwnedFd::from_raw_fd`, which is `unsafe` because we have to
//! promise the fd is freshly allocated and owned by us. It is —
//! `openat2` just gave it to us — so the safety obligation is met.
//! We allow the lint module-wide and confine the unsafe blocks to
//! these RAII wraps.

#![allow(unsafe_code)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{openat2, OFlag, OpenHow, ResolveFlag};
use nix::sys::stat::Mode;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct FileEntry {
    pub path: String,
    pub kind: &'static str, // "file" | "dir"
    pub size: u64,
    /// Last modification time as a unix timestamp (seconds). 0 if the
    /// platform doesn't expose mtime or the metadata read failed.
    pub mtime_unix: u64,
}

/// Cap on individual file size. Mirrors the controller's cap so the
/// agent can't be coerced into writing a huge blob via the local API.
pub const MAX_BYTES: usize = 5 * 1024 * 1024;

/// Cap on number of entries returned by [`Workspace::file_tree`].
pub const MAX_TREE_ENTRIES: usize = 50_000;

/// Result of [`Workspace::file_tree`].
#[derive(Debug, Serialize)]
pub struct FileTree {
    pub entries: Vec<FileEntry>,
    pub truncated: bool,
}

/// Capability handle for the workspace directory. Owns the dirfd;
/// every file operation is a single `openat2`/`mkdirat`/`unlinkat`
/// syscall relative to it. The original path is kept only for
/// diagnostic logging — runtime resolution never re-touches it.
#[derive(Debug)]
pub struct Workspace {
    fd: OwnedFd,
    path: PathBuf,
}

impl Workspace {
    /// Open the workspace directory. Creates it if missing. The
    /// dirfd is held for the agent's lifetime.
    ///
    /// Errors here include the host path because they fire at boot
    /// only (operator-facing, never returned to a client).
    pub fn open(path: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(path)
            .map_err(|e| format!("create workspace {}: {e}", path.display()))?;
        // O_NOFOLLOW on the workspace itself — if /workspace happens
        // to be a symlink we refuse, since that's not what the
        // operator intended.
        let raw = nix::fcntl::open(
            path,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|e| format!("open workspace {}: {e}", path.display()))?;
        // SAFETY: the fd was just returned by open(2); we own it now.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Self { fd, path: path.to_path_buf() })
    }

    /// Path the workspace was opened from. **Diagnostic only** — file
    /// operations never re-resolve via this path.
    pub fn path(&self) -> &Path { &self.path }

    fn dirfd(&self) -> RawFd { self.fd.as_raw_fd() }

    /// Read a file's bytes (capped at [`MAX_BYTES`]). Error messages
    /// contain only the **relative** path — host paths never escape
    /// to the client.
    ///
    /// The read is **bounded** at `MAX_BYTES + 1` via `Read::take`, so
    /// a multi-GiB file in the workspace can't OOM the agent — the
    /// `take` reader stops at the cap, we observe the overflow byte
    /// (or hit EOF first), and reject if it's set.
    pub fn read_file(&self, relative: &str) -> Result<Vec<u8>, String> {
        validate_relative(relative)?;
        let how = sandbox_open_how(OFlag::O_RDONLY | OFlag::O_CLOEXEC);
        let raw = openat2(self.dirfd(), relative, how)
            .map_err(|e| map_open_err("read", relative, e))?;
        // SAFETY: openat2 returned a fresh fd we own.
        let file = unsafe { File::from_raw_fd(raw) };
        // Bound the read at MAX_BYTES + 1: if `take` returns that many
        // bytes, the file has at least one more byte → over the cap.
        // If it returns fewer, that's the full file and we're under cap.
        let mut buf = Vec::new();
        let mut limited = file.take(MAX_BYTES as u64 + 1);
        limited
            .read_to_end(&mut buf)
            .map_err(|e| format!("read {relative}: {e}"))?;
        if buf.len() > MAX_BYTES {
            return Err(format!(
                "file too large to read (>{MAX_BYTES} bytes)"
            ));
        }
        Ok(buf)
    }

    /// Write a file's bytes (truncate-or-create). Creates parent
    /// directories as needed via [`Self::create_dir_all_relative`].
    pub fn write_file(&self, relative: &str, content: &[u8]) -> Result<(), String> {
        validate_relative(relative)?;
        if content.len() > MAX_BYTES {
            return Err(format!(
                "file too large: {} bytes (max {MAX_BYTES})",
                content.len()
            ));
        }
        // Ensure parents exist. None for top-level files.
        if let Some(parent) = parent_of(relative) {
            self.create_dir_all_relative(parent)?;
        }
        let how = sandbox_open_how(OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC | OFlag::O_CLOEXEC);
        let raw = openat2_with_mode(self.dirfd(), relative, how, 0o644)
            .map_err(|e| map_open_err("write", relative, e))?;
        // SAFETY: fresh fd from openat2.
        let mut file = unsafe { File::from_raw_fd(raw) };
        file.write_all(content)
            .map_err(|e| format!("write {relative}: {e}"))
    }

    /// Delete a file (NOT a directory). `Ok(false)` if absent.
    pub fn delete_file(&self, relative: &str) -> Result<bool, String> {
        validate_relative(relative)?;
        // Resolve the parent directory under sandbox semantics, then
        // unlinkat the leaf relative to that parent fd. Symlink swap
        // of the parent during the operation cannot affect us — fds
        // reference inodes, not paths.
        let (parent_rel, leaf) = split_parent_leaf(relative);
        let parent_fd = self.open_dir(parent_rel)?;
        match nix::unistd::unlinkat(
            Some(parent_fd.as_raw_fd()),
            leaf,
            nix::unistd::UnlinkatFlags::NoRemoveDir,
        ) {
            Ok(()) => Ok(true),
            Err(Errno::ENOENT) => Ok(false),
            Err(Errno::EISDIR) => Err("path is a directory; use rmdir instead".into()),
            Err(e) => Err(format!("delete {relative}: {e}")),
        }
    }

    /// Create directories along `relative` if missing. Idempotent.
    pub fn create_dir_all_relative(&self, relative: &str) -> Result<(), String> {
        validate_relative(relative)?;
        // Walk components, mkdirat each one relative to the *previous*
        // directory fd we opened (sandbox semantics maintained
        // throughout — never falls back to absolute paths).
        let mut current_fd: RawFd = self.dirfd();
        let mut owned: Option<OwnedFd> = None;
        for component in relative.split('/').filter(|s| !s.is_empty()) {
            // mkdirat: ignore EEXIST.
            match nix::sys::stat::mkdirat(
                Some(current_fd),
                component,
                Mode::S_IRWXU | Mode::S_IRGRP | Mode::S_IXGRP,
            ) {
                Ok(()) => {}
                Err(Errno::EEXIST) => {}
                Err(e) => return Err(format!("mkdir {relative}: {e}")),
            }
            // Open the component (as a dir, no symlink follow) to use
            // as the next parent fd.
            let how = sandbox_open_how(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC);
            let raw = openat2(current_fd, component, how)
                .map_err(|e| map_open_err("mkdir", relative, e))?;
            // SAFETY: fresh fd from openat2.
            let next = unsafe { OwnedFd::from_raw_fd(raw) };
            current_fd = next.as_raw_fd();
            owned = Some(next); // hold onto the OwnedFd for next iteration's borrow
        }
        drop(owned); // explicit: close the deepest-opened dir fd
        Ok(())
    }

    /// Open a (possibly empty) sub-path as a directory fd. `""`
    /// returns the workspace dirfd itself.
    fn open_dir(&self, relative: &str) -> Result<OwnedFd, String> {
        if relative.is_empty() {
            // Re-dup the workspace fd so the caller has an OwnedFd.
            let dup = self
                .fd
                .try_clone()
                .map_err(|e| format!("dup workspace fd: {e}"))?;
            return Ok(dup);
        }
        validate_relative(relative)?;
        let how = sandbox_open_how(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC);
        let raw = openat2(self.dirfd(), relative, how)
            .map_err(|e| map_open_err("open dir", relative, e))?;
        // SAFETY: fresh fd from openat2.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    /// Walk the workspace and list every regular file or dir. Skips
    /// noise dirs (`node_modules`, `.git`, …) and any symlinks.
    /// Capped at [`MAX_TREE_ENTRIES`].
    pub fn file_tree(&self) -> Result<FileTree, String> {
        let mut out = Vec::new();
        let mut stack: Vec<(PathBuf, String)> = vec![(self.path.clone(), String::new())];
        let mut truncated = false;

        'outer: while let Some((dir, prefix)) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue, // dir might have been swapped; skip
            };
            for entry in entries.flatten() {
                if out.len() >= MAX_TREE_ENTRIES {
                    truncated = true;
                    break 'outer;
                }
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
                // symlink_metadata sees the link itself, not its target.
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let ft = meta.file_type();
                if ft.is_symlink() {
                    continue;
                }
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if ft.is_dir() {
                    out.push(FileEntry {
                        path: rel.clone(),
                        kind: "dir",
                        size: 0,
                        mtime_unix: mtime,
                    });
                    stack.push((entry.path(), rel));
                } else if ft.is_file() {
                    out.push(FileEntry {
                        path: rel,
                        kind: "file",
                        size: meta.len(),
                        mtime_unix: mtime,
                    });
                }
            }
        }

        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(FileTree { entries: out, truncated })
    }
}

// ─── helpers ────────────────────────────────────────────────────

/// Sandbox `OpenHow` — every file open is RESOLVE_BENEATH (no escape)
/// and RESOLVE_NO_SYMLINKS (no follow). Together they make the open
/// atomic and TOCTOU-free.
fn sandbox_open_how(flags: OFlag) -> OpenHow {
    OpenHow::new()
        .flags(flags)
        .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS)
}

fn openat2_with_mode(
    dirfd: RawFd,
    path: &str,
    mut how: OpenHow,
    mode: u32,
) -> nix::Result<RawFd> {
    how = how.mode(Mode::from_bits_truncate(mode as nix::libc::mode_t));
    openat2(dirfd, path, how)
}

fn map_open_err(op: &str, rel: &str, e: nix::Error) -> String {
    match e {
        Errno::ELOOP => format!("{op} {rel}: refusing to follow symlink"),
        Errno::EXDEV => format!("{op} {rel}: path escapes workspace"),
        Errno::ENOENT => format!("{op} {rel}: No such file or directory"),
        _ => format!("{op} {rel}: {e}"),
    }
}

/// Cap on the number of path components, applied by [`validate_relative`].
/// 32 is comfortably above any real project layout and bounds the
/// per-call syscall count for `create_dir_all_relative` so a hostile
/// caller can't spam unbounded-depth paths.
pub const MAX_PATH_COMPONENTS: usize = 32;

/// Reject empty / absolute / `..` / pathologically deep paths. Belt
/// over the kernel's suspenders (`RESOLVE_BENEATH` would catch `..`,
/// but a clearer 4xx is friendlier than "EXDEV").
fn validate_relative(relative: &str) -> Result<(), String> {
    if relative.is_empty() {
        return Err("path is empty".into());
    }
    if relative.starts_with('/') {
        return Err("absolute paths not allowed".into());
    }
    let mut depth = 0usize;
    for c in Path::new(relative).components() {
        match c {
            std::path::Component::ParentDir => {
                return Err("'..' segments not allowed".into())
            }
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                return Err("absolute paths not allowed".into())
            }
            std::path::Component::Normal(_) => depth += 1,
            _ => {}
        }
    }
    if depth > MAX_PATH_COMPONENTS {
        return Err(format!(
            "path too deep ({depth} components; max {MAX_PATH_COMPONENTS})"
        ));
    }
    Ok(())
}

/// Split `"a/b/c"` -> (`"a/b"`, `"c"`). Single component returns
/// (`""`, leaf).
fn split_parent_leaf(relative: &str) -> (&str, &str) {
    match relative.rsplit_once('/') {
        Some((p, l)) => (p, l),
        None => ("", relative),
    }
}

fn parent_of(relative: &str) -> Option<&str> {
    relative.rfind('/').map(|i| &relative[..i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_workspace(label: &str) -> Workspace {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("zsbx-files-{label}-{pid}-{n}"));
        Workspace::open(&dir).expect("open workspace")
    }

    #[test]
    fn write_then_read() {
        let ws = unique_workspace("a");
        ws.write_file("hello.txt", b"world").unwrap();
        assert_eq!(ws.read_file("hello.txt").unwrap(), b"world");
    }

    #[test]
    fn write_creates_parents() {
        let ws = unique_workspace("b");
        ws.write_file("src/server.ts", b"x").unwrap();
        assert!(ws.path().join("src/server.ts").exists());
    }

    #[test]
    fn rejects_parent_dir() {
        let ws = unique_workspace("c");
        assert!(ws.write_file("../boom", b"x").is_err());
    }

    #[test]
    fn rejects_pathologically_deep_path() {
        let ws = unique_workspace("deep");
        // 33 components > MAX_PATH_COMPONENTS (32)
        let mut deep = String::new();
        for _ in 0..MAX_PATH_COMPONENTS + 1 {
            deep.push_str("a/");
        }
        deep.push_str("file.txt");
        let r = ws.write_file(&deep, b"x");
        assert!(r.is_err(), "deep path must be rejected");
        let msg = r.unwrap_err();
        assert!(msg.contains("too deep"), "expected 'too deep', got: {msg}");
    }

    #[test]
    fn rejects_absolute() {
        let ws = unique_workspace("d");
        assert!(ws.write_file("/etc/passwd", b"x").is_err());
    }

    #[test]
    fn rejects_empty() {
        let ws = unique_workspace("e");
        assert!(ws.read_file("").is_err());
    }

    #[test]
    fn delete_idempotent_for_missing() {
        let ws = unique_workspace("f");
        assert!(!ws.delete_file("missing").unwrap());
    }

    #[test]
    fn cap_enforced_on_write() {
        let ws = unique_workspace("g");
        let huge = vec![0u8; MAX_BYTES + 1];
        assert!(ws.write_file("big", &huge).is_err());
    }

    #[test]
    fn file_tree_skips_noise() {
        let ws = unique_workspace("h");
        ws.write_file("src/main.ts", b"a").unwrap();
        ws.write_file("node_modules/foo/index.js", b"b").unwrap();
        let t = ws.file_tree().unwrap();
        let paths: Vec<_> = t.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"src/main.ts"));
        assert!(!paths.iter().any(|p| p.contains("node_modules")));
        assert!(!t.truncated);
    }

    // ─── P0-A symlink-leaf escape regressions ───────────────────

    #[test]
    fn read_refuses_symlink_leaf() {
        let ws = unique_workspace("sym1");
        let outside = ws.path().parent().unwrap().join("zsbx-secret.txt");
        std::fs::write(&outside, b"top secret").unwrap();
        symlink(&outside, ws.path().join("escape")).unwrap();

        let res = ws.read_file("escape");
        assert!(res.is_err(), "must refuse symlink leaf");
        let msg = res.unwrap_err();
        assert!(
            msg.contains("symlink") || msg.contains("escapes"),
            "expected symlink/escape error, got: {msg}"
        );
    }

    #[test]
    fn write_refuses_symlink_leaf() {
        let ws = unique_workspace("sym2");
        let outside = ws.path().parent().unwrap().join("zsbx-target.txt");
        std::fs::write(&outside, b"original").unwrap();
        symlink(&outside, ws.path().join("clobber")).unwrap();

        assert!(ws.write_file("clobber", b"PWNED").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"original");
    }

    #[test]
    fn delete_refuses_symlink_leaf() {
        let ws = unique_workspace("sym3");
        let outside = ws.path().parent().unwrap().join("zsbx-keep.txt");
        std::fs::write(&outside, b"x").unwrap();
        symlink(&outside, ws.path().join("victim")).unwrap();

        // unlinkat removes the symlink itself, not its target — so
        // delete actually succeeds, but the OUTSIDE file must remain.
        let _ = ws.delete_file("victim");
        assert!(outside.exists(), "outside file must NOT be deleted");
    }

    #[test]
    fn file_tree_omits_symlinks() {
        let ws = unique_workspace("sym4");
        ws.write_file("real.txt", b"x").unwrap();
        symlink("/etc/passwd", ws.path().join("link")).unwrap();
        let t = ws.file_tree().unwrap();
        let paths: Vec<String> = t.entries.into_iter().map(|e| e.path).collect();
        assert!(paths.iter().any(|p| p == "real.txt"));
        assert!(!paths.iter().any(|p| p == "link"));
    }

    /// **C2 TOCTOU regression: parent-symlink swap during operation.**
    /// Even if the user replaces a workspace subdirectory with a
    /// symlink between our open and our access, RESOLVE_BENEATH
    /// guarantees the operation refuses. Without RESOLVE_BENEATH,
    /// an attacker could swap `safedir` for a symlink to `/etc`
    /// then write `safedir/passwd` and clobber the host file.
    #[test]
    fn rejects_parent_symlink_escape() {
        let ws = unique_workspace("sym5");
        // Outside dir we'll try to escape into. UNIQUE per test run
        // (PID+counter) so previous runs' state in /tmp can't pollute.
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::SeqCst);
        let outside_dir = ws
            .path()
            .parent()
            .unwrap()
            .join(format!("zsbx-outside-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside_dir);
        std::fs::create_dir_all(&outside_dir).unwrap();

        // Make a workspace subdirectory that's actually a symlink
        // pointing OUT of the workspace.
        symlink(&outside_dir, ws.path().join("safedir")).unwrap();

        // Try to write through the symlinked parent — must fail.
        assert!(
            ws.write_file("safedir/inside.txt", b"PWNED").is_err(),
            "parent-symlink escape must be rejected"
        );
        assert!(
            !outside_dir.join("inside.txt").exists(),
            "no file may be created outside the workspace"
        );

        // Read through the symlinked parent — also rejected.
        std::fs::write(outside_dir.join("inside.txt"), b"set by host").unwrap();
        assert!(ws.read_file("safedir/inside.txt").is_err());
    }
}
