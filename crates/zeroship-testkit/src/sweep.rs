//! The decisions `tests/sweep_test_databases.sh` makes, separated from the
//! server it makes them against.
//!
//! They are here because they are the part that can be WRONG SILENTLY. The
//! script's other half either connects or does not; these functions produce a
//! VERDICT, and a wrong verdict is a dropped database.
//!
//! THE SWEEPER MUST NEVER DROP `WITH (FORCE)`, and that asymmetry against
//! `tests/lib/scratch_db.sh` -- which uses it, correctly -- is the whole safety
//! model. `WITH (FORCE)` terminates every other backend on the database before
//! dropping, so the drop cannot fail on a live connection. scratch_db drops a
//! database THIS RUN created, where the only connections left are its own
//! stragglers, and a plain DROP would leak the database on them. The sweeper
//! drops databases OTHER runs created; a live connection there is a peer agent
//! mid-suite, and `WITH (FORCE)` would kill it. A plain `DROP DATABASE` failing
//! with "is being accessed by other users" is not an inconvenience for the
//! sweeper -- it is the answer, and the last line of defence behind the
//! liveness scan below.
//!
//! WHY A /proc SCAN AND NOT `pg_stat_activity`. A suite run exports
//! `PG_TEST_URL=postgres://.../<db>`, so the name sits in `/proc/<pid>/environ`
//! from the first line of the script to the last -- INCLUDING the minutes it
//! spends in cargo with no backend attached at all. `pg_stat_activity` sees
//! nothing during those minutes, and that is exactly when a sweeper would
//! decide the database was dead.

use std::path::Path;

/// The database families the sweeper owns.
///
/// Deliberately a short explicit list rather than a `zeroship%` wildcard:
/// `zeroship` itself is the dev platform database on the shared cluster and
/// carries real rows.
pub const FAMILIES: &[&str] = &["zeroship_auth_test", "zeroship_billing_test"];

/// The family a database name belongs to, or `None`.
///
/// A name matches a family only if it IS the family or the family followed by
/// `_`. The prefix test has to be anchored that way: `zeroship` is a prefix of
/// every name here, and a sweeper that treated prefixes loosely would put the
/// platform's own database on the list.
pub fn family_of(name: &str) -> Option<&'static str> {
    FAMILIES.iter().copied().find(|family| {
        name == *family || (name.len() > family.len() + 1 && name.starts_with(&format!("{family}_")))
    })
}

/// Is `pid` `self_pid` or one of its descendants?
///
/// Our own subshells and child processes inherit our command line and our
/// environment, so a `/proc` scan for a name we are examining will otherwise
/// find US and report every candidate live -- a sweeper that silently reclaims
/// nothing, which is the failure mode that looks like success.
///
/// Walks the ppid chain rather than comparing command lines, which is what
/// makes it independent of what the caller was invoked with.
pub fn pid_is_ours(pid: i32, self_pid: i32) -> bool {
    let mut pid = pid;
    for _ in 0..32 {
        // The ROOT test comes FIRST. Every ancestry chain ends at 1 and then 0,
        // so if `self_pid` were ever 0 or 1 the match below would fire on the
        // last hop of every walk and declare the entire process table ours.
        // Checking the terminals first makes that unreachable.
        if pid == 1 || pid == 0 {
            return false;
        }
        if pid == self_pid {
            return true;
        }
        match parent_of(pid) {
            Some(ppid) => pid = ppid,
            None => return false,
        }
    }
    false
}

/// Field 4 of `/proc/<pid>/stat`.
///
/// Field 2 (`comm`) can contain spaces AND parentheses -- `(Web Content)`,
/// `(sh -c foo)` -- so the count starts from the LAST `)` rather than from the
/// start of the line.
fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat.rsplit_once(") ")?.1;
    tail.split(' ').nth(1)?.parse().ok()
}

/// What a single `/proc` pass found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Holders {
    /// `<name> <pid>` pairs, in pattern-file order within each pid.
    pub held_by: Vec<(String, i32)>,
    /// How many `/proc` entries could not be read -- another user's process, or
    /// one that exited between the directory listing and the open.
    ///
    /// Reported rather than swallowed: neither can be a run of OURS holding one
    /// of these names, so skipping is correct, but the blindness is visible
    /// instead of assumed.
    pub unreadable: usize,
}

/// Scan `/proc` ONCE for every name in `names`.
///
/// ONE PASS, not one per candidate. The obvious shape -- loop candidates, loop
/// `/proc` -- was 62 x ~1000 x 2 greps and did not finish in two minutes.
///
/// TWO STAGES, because substring and token are different questions.
/// `zeroship_auth_test` is a substring of `zeroship_auth_test_s46`, so a
/// substring test alone would report the short name held whenever the long one
/// is. Stage 1 uses the cheap substring test only to REJECT the processes that
/// mention nothing; stage 2 splits the survivors into identifier tokens and
/// matches exactly.
pub fn scan_holders(names: &[String], self_pid: i32) -> Holders {
    let mut out = Holders::default();
    let mut candidates: Vec<i32> = Vec::new();

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let file = entry.file_name();
        let Some(pid) = file.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let dir = entry.path();
        let (Ok(environ), Ok(cmdline)) = (
            std::fs::read(dir.join("environ")),
            std::fs::read(dir.join("cmdline")),
        ) else {
            out.unreadable += 1;
            continue;
        };
        let mut haystack = cmdline;
        haystack.extend_from_slice(&environ);
        if names.iter().any(|n| contains(&haystack, n.as_bytes())) {
            candidates.push(pid);
        }
    }

    candidates.sort_unstable();
    for pid in candidates {
        if pid_is_ours(pid, self_pid) {
            continue;
        }
        let dir = Path::new("/proc").join(pid.to_string());
        let (Ok(environ), Ok(cmdline)) = (
            std::fs::read(dir.join("environ")),
            std::fs::read(dir.join("cmdline")),
        ) else {
            continue;
        };
        let mut haystack = cmdline;
        haystack.extend_from_slice(&environ);
        let tokens = identifier_tokens(&haystack);
        for name in names {
            if tokens.contains(&name.as_bytes()) {
                out.held_by.push((name.clone(), pid));
            }
        }
    }
    out
}

/// The pids holding `name`, in scan order.
pub fn holders_of(held: &Holders, name: &str) -> Vec<i32> {
    held.held_by
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, pid)| *pid)
        .collect()
}

/// `tr -c 'a-zA-Z0-9_' '\n'` -- every maximal run of identifier characters.
fn identifier_tokens(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, b) in bytes.iter().enumerate() {
        let is_word = b.is_ascii_alphanumeric() || *b == b'_';
        match (is_word, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push(&bytes[s..i]);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push(&bytes[s..]);
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_family_test_claims_the_suite_databases_and_nothing_else() {
        for name in [
            "zeroship_auth_test",
            "zeroship_auth_test_s46",
            "zeroship_auth_test_314701e327c4",
            "zeroship_billing_test",
            "zeroship_billing_test_v65v",
        ] {
            assert!(family_of(name).is_some(), "did not own {name}");
        }
        // The ones a loose prefix test would swallow. `zeroship` is the dev
        // platform database and carries real rows; the rest belong to other
        // tools on the same cluster.
        for name in [
            "zeroship",
            "postgres",
            "template0",
            "template1",
            "zeroship_control_test_s8",
            "compio_pg_s8",
            "zeroship_billing_v41",
            "zs_wf_engine_restart_f897474e",
            "zeroship_auth_r9_jwks",
        ] {
            assert!(family_of(name).is_none(), "CLAIMED {name}, which it does not own");
        }
    }

    #[test]
    fn a_family_name_with_a_bare_trailing_underscore_is_not_a_member() {
        // `zeroship_auth_test_` is the family plus a separator and no suffix.
        // Reclaiming it is harmless, but claiming it would mean the anchored
        // prefix test had degenerated into a loose one.
        assert!(family_of("zeroship_auth_test_").is_none());
    }

    #[test]
    fn pid_is_ours_recognises_this_process_and_not_pid_one() {
        let me = std::process::id() as i32;
        assert!(pid_is_ours(me, me));
        assert!(!pid_is_ours(1, me));
        // A self-pid of 0 or 1 must not swallow the whole process table: every
        // ancestry walk ends at 1 and then 0, so a naive order would match on
        // the last hop of EVERY walk.
        assert!(!pid_is_ours(1, 1));
        assert!(!pid_is_ours(1, 0));
        assert!(!pid_is_ours(me, 1));
    }

    #[test]
    fn a_child_is_ours_and_a_stranger_is_not() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let me = std::process::id() as i32;
        let pid = child.id() as i32;
        assert!(pid_is_ours(pid, me), "missed child {pid}");
        // One variable changed: the same live pid, against a self-pid that is
        // in nobody's ancestry. Only the pair tells a working walk apart from
        // one that says yes to everything.
        assert!(!pid_is_ours(pid, 2147483647));
        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn the_scan_finds_a_holder_through_its_environment_alone() {
        // The shape of a suite run sitting in cargo with no database session
        // open -- the case `pg_stat_activity` cannot see. The command line says
        // `sleep 60`; only the environment carries the name.
        let needle = format!("zeroship_auth_test_ffff{}", std::process::id());
        let names = vec![needle.clone()];
        let me = std::process::id() as i32;
        let stranger = 2147483647;

        let before = scan_holders(&names, stranger);
        assert!(holders_of(&before, &needle).is_empty(), "something already held it");

        let mut holder = std::process::Command::new("sleep")
            .arg("60")
            .env("PG_TEST_URL", format!("postgres://u:p@h:5432/{needle}"))
            .spawn()
            .expect("spawn holder");
        let pid = holder.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(300));

        let found = scan_holders(&names, stranger);
        // The holder is a CHILD of this process, so the two calls below differ
        // in exactly one variable: which pid the scan is told is its own.
        let excluded = scan_holders(&names, me);
        holder.kill().ok();
        holder.wait().ok();

        assert!(holders_of(&found, &needle).contains(&pid), "missed the holder {pid}");
        assert!(
            !holders_of(&excluded, &needle).contains(&pid),
            "a descendant of the scanner was reported as a holder"
        );
    }

    #[test]
    fn substring_is_not_membership() {
        // `zeroship_auth_test` is a substring of `zeroship_auth_test_s46`. A
        // holder of the long name must not make the short one look held, or the
        // bare family names could never be reclaimed while anything at all was
        // running.
        let long = format!("zeroship_auth_test_s{}", std::process::id());
        let names = vec!["zeroship_auth_test".to_string(), long.clone()];

        let mut holder = std::process::Command::new("sleep")
            .arg("60")
            .env("PG_TEST_URL", format!("postgres://u:p@h:5432/{long}"))
            .spawn()
            .expect("spawn holder");
        let pid = holder.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(300));
        let found = scan_holders(&names, 2147483647);
        holder.kill().ok();
        holder.wait().ok();

        assert!(holders_of(&found, &long).contains(&pid), "missed the long name");
        assert!(
            !holders_of(&found, "zeroship_auth_test").contains(&pid),
            "the prefix was reported held"
        );
    }

    #[test]
    fn tokens_split_on_every_non_identifier_byte() {
        let got = identifier_tokens(b"postgres://u:p@h:5432/zeroship_auth_test_s46\0X=1");
        let got: Vec<&str> = got.iter().map(|t| std::str::from_utf8(t).unwrap()).collect();
        assert!(got.contains(&"zeroship_auth_test_s46"));
        assert!(!got.contains(&"zeroship_auth_test"));
        assert!(got.contains(&"5432"));
    }
}
