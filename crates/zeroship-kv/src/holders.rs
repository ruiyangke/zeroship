//! Who is holding this file open.
//!
//! redb takes an exclusive lock for the process lifetime, and its open failure
//! says only `Database already open. Cannot acquire lock.` That is true and
//! undiagnosable: it names the condition and not the cause, so the reader has
//! no next step.
//!
//! The cause is almost always an ORPHANED runtime. `zeroship serve` is a forked
//! child of vite, so a harness that kills its recorded PID kills the parent and
//! leaves the runtime holding the lock. Four such survivors, aged 10 to 34
//! minutes, were found holding `.zeroship/kv.redb` on 2026-08-10 (task #221);
//! the next boot succeeded in 3 seconds once they were killed.
//!
//! This scanner turns that 20-minute `/proc` investigation into a line of error
//! text. It runs ONLY on the open-failure path, so it costs nothing in the
//! normal case.
//!
//! **What it cannot tell you.** A `/proc` walk sees only processes this user can
//! read. An empty result means "no holder I am allowed to see", NEVER "no
//! holder" - so the caller must not phrase an empty result as an all-clear, and
//! [`describe_holders`] deliberately returns `None` rather than a sentence
//! claiming the file is free.

/// A process holding a file open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    /// `/proc/<pid>/comm`, or an empty string if it could not be read.
    pub comm: String,
}

/// Processes holding `path` open, by walking `/proc/*/fd`.
///
/// Returns an empty vec on any non-Linux target and on permission failure. See
/// the module docs: empty is not evidence of absence.
#[cfg(target_os = "linux")]
pub fn holders_of(path: &std::path::Path) -> Vec<Holder> {
    let Ok(target) = path.canonicalize() else {
        return Vec::new();
    };
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for entry in procs.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let fd_dir = entry.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue; // not ours to read, or the process just exited
        };
        for fd in fds.flatten() {
            // read_link on /proc/<pid>/fd/<n> resolves to the open file.
            if std::fs::read_link(fd.path()).is_ok_and(|p| p == target) {
                let comm = std::fs::read_to_string(entry.path().join("comm"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                found.push(Holder { pid, comm });
                break; // one entry per process, however many fds it holds
            }
        }
    }
    found
}

#[cfg(not(target_os = "linux"))]
pub fn holders_of(_path: &std::path::Path) -> Vec<Holder> {
    Vec::new()
}

/// A human-readable holder line, or `None` when nothing readable holds the file.
///
/// `None` deliberately produces no text at all. Saying "no other process holds
/// it" would be a claim this scanner cannot support (see module docs), and a
/// false all-clear next to a lock error is worse than silence.
pub fn describe_holders(path: &std::path::Path) -> Option<String> {
    let holders = holders_of(path);
    if holders.is_empty() {
        return None;
    }
    let list = holders
        .iter()
        .map(|h| {
            if h.comm.is_empty() {
                format!("pid {}", h.pid)
            } else {
                format!("pid {} ({})", h.pid, h.comm)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "still held by {list}. \
         This is usually an orphaned runtime from an earlier run, not a port \
         clash - changing the dev server port will NOT help. Kill the holder(s) \
         and retry"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test process opens the file, so the scanner MUST find our own pid.
    ///
    /// Paired with `finds_nobody_for_an_unheld_file` below, which differs in
    /// exactly one variable: whether anything holds the path. A positive alone
    /// would only prove the walk runs; the pair proves it DISCRIMINATES.
    #[test]
    fn finds_this_process_holding_a_file() {
        let dir = std::env::temp_dir().join(format!("kvholders-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("held.bin");
        let _f = std::fs::File::create(&path).unwrap(); // kept open for the test

        let holders = holders_of(&path);
        assert!(
            holders.iter().any(|h| h.pid == std::process::id()),
            "scanner did not find this process ({}) holding {}; found {:?}",
            std::process::id(),
            path.display(),
            holders
        );
        assert!(describe_holders(&path).is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The one-variable control: same directory, same creation, but the handle
    /// is dropped before scanning. Empty result, and NO text - a false
    /// all-clear next to a lock error is worse than silence.
    #[test]
    fn finds_nobody_for_an_unheld_file() {
        let dir = std::env::temp_dir().join(format!("kvholders-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("unheld.bin");
        drop(std::fs::File::create(&path).unwrap()); // closed immediately

        assert_eq!(holders_of(&path), Vec::new());
        assert_eq!(describe_holders(&path), None);

        std::fs::remove_dir_all(&dir).ok();
    }
}
