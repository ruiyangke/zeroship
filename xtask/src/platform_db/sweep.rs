//! The decisions the sweeper makes, separated from the server it makes them
//! against.
//!
//! They are here because they are the part that can be WRONG SILENTLY. The
//! caller's other half either connects or does not; these functions produce a
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
//!
//! AND WHY AN EMPTY SCAN IS NOT AN ANSWER BY ITSELF. Everything above describes
//! how the scan LOOKS. It says nothing about the runs where it could not look,
//! and those produced identical output: no holders found, exit 0, drop. See
//! [`Evidence`], [`gaps`] and [`verdict`] at the bottom of this file for the
//! sorting that separates a pass which saw nothing from one that saw nothing
//! BECAUSE it was blind, and [`super::sweep_command`] for the caller that
//! assembles the other half of the evidence.

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
    FAMILIES
        .iter()
        .copied()
        .find(|family| name == *family || name.starts_with(&format!("{family}_")))
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

/// How few `/proc` entries a pass may rule on before the pass itself is the
/// thing in doubt.
///
/// A FLOOR, NOT A TARGET, in the sense `tests/lib/gate_arms.sh` uses the word:
/// it separates "this box is quiet" from "this scan did not look". Measured
/// 2026-08-20 on the machine the sweeper runs on: 495 entries, of which 109
/// were ours. Any Linux kernel contributes more than sixteen threads before
/// userspace starts, so a pass under this saw a process table that is not a
/// Linux machine's -- a PID namespace, most likely, where the peer agent this
/// scan exists to protect is by construction invisible.
///
/// WHAT IT DOES NOT CATCH: a namespace large enough to clear it. `proc_hidden`
/// is the companion probe and catches `hidepid=2`; neither catches a container
/// with a plausible number of its own processes in it.
pub const PROC_PIDS_FLOOR: usize = 16;

/// What a single `/proc` pass found, AND what it could not see.
///
/// The second half is the point. The previous shape carried one `unreadable`
/// counter that lumped four different situations together, printed it in a
/// summary line, and let the sweep proceed on it; 406 of the 495 entries
/// counted into it on an ordinary run, so the number could never have
/// discriminated anything.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Holders {
    /// `<name> <pid>` pairs, in pattern-file order within each pid.
    pub held_by: Vec<(String, i32)>,
    /// Entries this pass ruled on: it read at least the command line.
    pub examined: usize,
    /// Entries whose process had exited -- the directory was gone, or the
    /// process was a zombie waiting to be reaped.
    ///
    /// NOT a gap. A process that has exited is not a process that is alive, so
    /// skipping it is provably right rather than assumed right.
    pub vanished: usize,
    /// Entries whose `cmdline` the kernel gave us and whose `environ` it did
    /// not.
    ///
    /// NOT a gap by itself, because the argv half IS read -- which is new:
    /// requiring both files threw away the command line of all 390 foreign
    /// processes on this machine (measured 2026-08-20). Two causes seen there,
    /// both per-process rather than machine-wide: another uid, and a process
    /// the kernel marks unreadable (`ssh-agent`, whose `/proc/<pid>/environ` is
    /// root-owned mode 400; a containerised `postgres`, in another user
    /// namespace). What makes this survivable is [`Holders::peer_env_read`]
    /// below, which proves the environment channel works at all.
    pub env_denied: usize,
    /// Entries whose `environ` was read for a process that is NOT one of ours.
    ///
    /// THE ANTI-VACUITY PROBE FOR LAYER 2. The environment half is the entire
    /// reason this scan exists -- a peer's suite sitting in cargo holds no
    /// backend and appears only in `PG_TEST_URL` -- and a kernel that refused
    /// every peer's `environ` would leave that half silently dead while the
    /// argv half kept the scan looking healthy. Measured 2026-08-20 on this
    /// machine: 77 readable, 14 refused. Zero is the gap.
    pub peer_env_read: usize,
    /// Entries that are alive and whose `cmdline` could not be read at all.
    ///
    /// A GAP. Exiting explains a short read and so does a refused environment;
    /// neither explains this, and every one of them could be the peer agent
    /// whose database is about to be dropped. Measured 2026-08-20: 0 of 495.
    pub unexplained: Vec<i32>,
    /// `/proc` itself could not be listed. Total blindness, and indistinguish-
    /// able from "nothing holds any of these names" until it is carried out.
    pub proc_unlistable: bool,
    /// The listing did not contain pid 1, so the kernel is hiding processes
    /// from us (`hidepid=2`, `subset=pid`). Peer agents would be hidden too.
    pub proc_hidden: bool,
}

/// What one `/proc` entry yielded.
#[derive(Debug)]
enum PidRead {
    /// Command line and environment both read.
    Full(Vec<u8>),
    /// Command line read, environment refused.
    ArgvOnly(Vec<u8>),
    /// The process had exited: the directory was gone, or it was a zombie.
    Vanished,
    /// Alive, and its command line could not be read.
    Unexplained,
}

/// Read one `/proc/<pid>`, and say WHY when it yields less than everything.
///
/// THE DEAD CHECK COMES BEFORE EVERY OTHER EXPLANATION, and it is not the
/// obvious "does the directory still exist". A process that has exited but has
/// not been reaped keeps its `/proc` entry, answers `cmdline` with zero bytes
/// and refuses `environ` -- which is character for character what a live
/// process with a protected environment looks like. The first draft of this
/// classified those as unexplained and two of this file's own tests went red on
/// their own `sleep` children, which is how the case was found.
fn read_pid(dir: &Path) -> PidRead {
    let cmdline = std::fs::read(dir.join("cmdline"));
    let environ = std::fs::read(dir.join("environ"));
    match (cmdline, environ) {
        (Ok(cmdline), Ok(environ)) => {
            let mut haystack = cmdline;
            haystack.extend_from_slice(&environ);
            PidRead::Full(haystack)
        }
        (Ok(cmdline), Err(_)) if !has_exited(dir) => PidRead::ArgvOnly(cmdline),
        (Ok(_), Err(_)) => PidRead::Vanished,
        (Err(_), _) if has_exited(dir) => PidRead::Vanished,
        (Err(_), _) => PidRead::Unexplained,
    }
}

/// Has the process behind `/proc/<pid>` stopped running?
///
/// True when the entry is gone, and true for the `Z` (zombie) and `X` (dead)
/// states, which keep the entry alive after the process is not.
///
/// State is the first field after the `comm` parenthesis, counted from the LAST
/// `)` for the same reason [`parent_of`] counts from there: `comm` may contain
/// both spaces and parentheses.
fn has_exited(dir: &Path) -> bool {
    let Ok(stat) = std::fs::read_to_string(dir.join("stat")) else {
        return true;
    };
    let Some(tail) = stat.rsplit_once(") ") else {
        return true;
    };
    matches!(tail.1.split(' ').next(), Some("Z") | Some("X"))
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
///
/// WHAT IT STILL CANNOT SEE, stated because a scan that names its own limits is
/// the only kind whose empty answer means anything: a run that never spells its
/// database name in argv or in its environment is invisible here. That is a
/// property of the suite database exporting `TEST_DB`/`PG_TEST_URL`, not
/// of this function, and nothing in this file can detect it changing. The
/// selftest's holder case is what pinned it.
pub fn scan_holders(names: &[String], self_pid: i32) -> Holders {
    let mut out = Holders::default();
    let mut candidates: Vec<i32> = Vec::new();

    let Ok(entries) = std::fs::read_dir("/proc") else {
        out.proc_unlistable = true;
        return out;
    };
    let mut saw_init = false;
    for entry in entries.flatten() {
        let file = entry.file_name();
        let Some(pid) = file.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        if pid == 1 {
            saw_init = true;
        }
        let haystack = match read_pid(&entry.path()) {
            PidRead::Full(bytes) => {
                out.examined += 1;
                // The ancestry walk runs only on entries whose environment we
                // actually got, so the probe counts the thing it claims to:
                // environments of processes that are not this run's.
                if !pid_is_ours(pid, self_pid) {
                    out.peer_env_read += 1;
                }
                bytes
            }
            PidRead::ArgvOnly(bytes) => {
                out.examined += 1;
                out.env_denied += 1;
                bytes
            }
            PidRead::Vanished => {
                out.vanished += 1;
                continue;
            }
            PidRead::Unexplained => {
                out.unexplained.push(pid);
                continue;
            }
        };
        if names.iter().any(|n| contains(&haystack, n.as_bytes())) {
            candidates.push(pid);
        }
    }
    out.proc_hidden = !saw_init;

    candidates.sort_unstable();
    for pid in candidates {
        if pid_is_ours(pid, self_pid) {
            continue;
        }
        let dir = Path::new("/proc").join(pid.to_string());
        let haystack = match read_pid(&dir) {
            PidRead::Full(bytes) | PidRead::ArgvOnly(bytes) => bytes,
            // Stage 1 proved this process names one of these databases, so a
            // stage 2 that cannot confirm it is throwing away the strongest
            // evidence the pass has. Exiting explains it; being alive and
            // unreadable does not, and that becomes a gap.
            PidRead::Vanished => continue,
            PidRead::Unexplained => {
                out.unexplained.push(pid);
                continue;
            }
        };
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

// ---------------------------------------------------------------------------
// Was the evidence complete?
//
// THE DEFECT THIS ANSWERS. Every check the sweeper makes can come back empty
// for two unrelated reasons: nothing is alive, or the check could not look.
// Those produced the same output and the same exit code, so "no evidence of
// life" was read as "evidence of death" -- and the consequence is an
// irreversible DROP of a peer agent's run or of the one artifact somebody is
// debugging a failed suite from.
//
// WHY REFUSING IS AFFORDABLE HERE. The obvious objection to refusing on doubt
// is that a tool which refuses always is a tool nobody runs, and then the leak
// returns. That objection is answered by SORTING the doubt rather than by
// tolerating it. Three of the four ways a `/proc` pass comes back short have
// proofs attached -- the process exited, or it belongs to a uid that is not
// ours -- and on the machine the sweeper runs on those account for 406 of the
// 406 entries the old counter lumped together (measured 2026-08-20). What is
// left, `unexplained`, is empty on a healthy run, so the refusal costs nothing
// until something is genuinely wrong.
//
// WHY A GAP DOES NOT REFUSE WHEN NOTHING IS DOOMED. Every gap below can only
// make a database look MORE dead: an unread process is a holder not found, an
// unread session count is a session not seen, an unfingerprinted worktree is a
// schema not reachable. So closing a gap can only SHRINK the drop list, and a
// drop list that is already empty is a conclusion no additional evidence could
// overturn. That is what keeps the control case -- a clean cluster with nothing
// to reclaim -- exiting 0 instead of crying wolf.
// ---------------------------------------------------------------------------

/// One thing this sweep could not establish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    /// A stable token, so a caller can test for one gap without matching prose.
    pub id: &'static str,
    /// What could not be established, and what it would have ruled on.
    pub detail: String,
}

/// Everything a sweep run learned about its own inputs.
///
/// ASSEMBLED BY THE CALLER, not gathered here. The `/proc` half comes from
/// [`scan_holders`]; the session count, the git enumeration and the worktree
/// fingerprints are the shell script's, because they are questions about a
/// server and a repository rather than about this process. Passing them in as
/// arguments is what keeps this function pure enough to have a discrimination
/// test per gap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evidence {
    /// The `/proc` scan subcommand did not succeed at all.
    pub proc_scan_failed: bool,
    /// [`Holders::proc_unlistable`].
    pub proc_unlistable: bool,
    /// [`Holders::proc_hidden`].
    pub proc_hidden: bool,
    /// [`Holders::examined`].
    pub proc_examined: usize,
    /// [`Holders::peer_env_read`].
    pub proc_peer_env_read: usize,
    /// [`Holders::unexplained`].
    pub proc_unexplained: Vec<i32>,
    /// Did the `pg_stat_activity` query return?
    pub sessions_query_ok: bool,
    /// Did `git worktree list` and `git for-each-ref` both return?
    pub git_listing_ok: bool,
    /// Working trees `git worktree list` reported.
    pub worktrees_seen: usize,
    /// Working trees whose migration set could be hashed.
    pub worktrees_fingerprinted: usize,
    /// How many databases this run would drop.
    pub doomed: usize,
}

/// Everything the run could not establish, in report order.
///
/// Empty is the ordinary answer. Each entry names the check and what it was
/// standing between the sweeper and.
#[must_use]
pub fn gaps(evidence: &Evidence) -> Vec<Gap> {
    let mut gaps = Vec::new();
    let mut add = |id, detail: String| gaps.push(Gap { id, detail });

    if evidence.proc_scan_failed {
        add(
            "proc_scan",
            "the /proc liveness scan did not run. Its empty result is the \
             absence of a scan, not the absence of holders."
                .to_owned(),
        );
    } else if evidence.proc_unlistable {
        add(
            "proc_unlistable",
            "/proc could not be listed, so no local run could be seen holding \
             any of these names."
                .to_owned(),
        );
    } else {
        if evidence.proc_hidden {
            add(
                "proc_hidden",
                "/proc did not list pid 1, so the kernel is hiding processes \
                 from this scan (hidepid=2, subset=pid, or a PID namespace). A \
                 peer agent's run would be hidden the same way."
                    .to_owned(),
            );
        }
        if evidence.proc_examined < PROC_PIDS_FLOOR {
            add(
                "proc_floor",
                format!(
                    "the /proc pass ruled on {} entries, under the floor of \
                     {PROC_PIDS_FLOOR}. A process table that small is not this \
                     machine's, so its silence says nothing.",
                    evidence.proc_examined
                ),
            );
        }
        if evidence.proc_peer_env_read == 0 {
            add(
                "env_blind",
                "no process outside this run had its environment read, so the \
                 half of the scan that sees a peer's suite sitting in cargo \
                 with no backend attached saw nothing at all. That is the case \
                 the /proc scan exists for; its argv half alone does not cover \
                 it."
                .to_owned(),
            );
        }
        if !evidence.proc_unexplained.is_empty() {
            add(
                "proc_unexplained",
                format!(
                    "{} /proc entries are alive and their command lines could \
                     not be read: {}. A process that exited and one whose \
                     environment is protected are both accounted for \
                     elsewhere; these are neither, and any of them could be \
                     the run that owns a database below.",
                    evidence.proc_unexplained.len(),
                    join_pids(&evidence.proc_unexplained),
                ),
            );
        }
    }

    if !evidence.sessions_query_ok {
        add(
            "sessions",
            "the pg_stat_activity query did not return, so every database \
             below was counted as having zero sessions attached."
                .to_owned(),
        );
    }

    if !evidence.git_listing_ok {
        add(
            "git_listing",
            "git could not enumerate the worktrees or the refs, so the set of \
             reachable schemas is not this repository's."
                .to_owned(),
        );
    } else if evidence.worktrees_fingerprinted < evidence.worktrees_seen {
        add(
            "worktrees",
            format!(
                "{} of {} working trees produced no schema fingerprint. An \
                 agent midway through authoring a migration has committed \
                 nothing, so its database's hash exists only in its working \
                 tree -- exactly the database this would call unreachable.",
                evidence.worktrees_seen - evidence.worktrees_fingerprinted,
                evidence.worktrees_seen,
            ),
        );
    }

    gaps
}

/// What the run may do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The evidence is complete. Whatever is on the drop list may be dropped.
    Proceed,
    /// Incomplete evidence, but nothing is on the drop list. Every gap above
    /// can only ADD to that list, so an empty one is safe. Say so and carry on.
    ProceedWithGaps(Vec<Gap>),
    /// Incomplete evidence and something to destroy. Stop.
    Refuse(Vec<Gap>),
}

/// Rule on a run's evidence.
#[must_use]
pub fn verdict(evidence: &Evidence) -> Verdict {
    let gaps = gaps(evidence);
    if gaps.is_empty() {
        Verdict::Proceed
    } else if evidence.doomed == 0 {
        Verdict::ProceedWithGaps(gaps)
    } else {
        Verdict::Refuse(gaps)
    }
}

/// The refusal a caller prints, naming every gap and what it is guarding.
#[must_use]
pub fn refusal_text(gaps: &[Gap], doomed: usize) -> String {
    let mut out = format!(
        "REFUSED: {} database(s) are on the drop list and this run could not \
         establish that they are dead.\n",
        doomed
    );
    for gap in gaps {
        out.push_str(&format!("  - [{}] {}\n", gap.id, gap.detail));
    }
    out.push_str(
        "  A check that could not look and a check that found nothing print the \
         same empty result. Dropping on the first is irreversible, so this \
         stops instead. Fix the check, or name the databases to drop yourself.\n",
    );
    out
}

/// The warning a caller prints when the evidence is short but the drop list is
/// empty, so nothing was at risk.
#[must_use]
pub fn gap_warning_text(gaps: &[Gap]) -> String {
    let mut out = String::from(
        "WARNING: this run's evidence was incomplete, and nothing was on the \
         drop list.\n",
    );
    for gap in gaps {
        out.push_str(&format!("  - [{}] {}\n", gap.id, gap.detail));
    }
    out.push_str(
        "  Each gap above can only ADD to the drop list, never remove from it, \
         so an empty list stands. It would not have stood with anything on it.\n",
    );
    out
}

fn join_pids(pids: &[i32]) -> String {
    pids.iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(" ")
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
            assert!(
                family_of(name).is_none(),
                "CLAIMED {name}, which it does not own"
            );
        }
    }

    #[test]
    fn a_family_name_with_a_bare_trailing_underscore_is_still_a_member() {
        // The shell's `case "$name" in "${f}_"*)` matches a `*` of zero
        // characters, so `zeroship_auth_test_` is OWNED. This is pinned because
        // the port got it wrong in the first draft: an extra length guard made
        // it unowned, which reads like strictness and is the opposite -- a
        // database the sweeper does not own is one it can never reclaim, so the
        // "stricter" rule leaks. Found by running the old shell function and
        // this one over the same names, not by this test, which was written to
        // agree with the bug.
        assert_eq!(family_of("zeroship_auth_test_"), Some("zeroship_auth_test"));
        // The boundary that must NOT move: no separator at all.
        assert_eq!(family_of("zeroship_auth_testx"), None);
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
        assert!(
            holders_of(&before, &needle).is_empty(),
            "something already held it"
        );

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

        assert!(
            holders_of(&found, &needle).contains(&pid),
            "missed the holder {pid}"
        );
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

        assert!(
            holders_of(&found, &long).contains(&pid),
            "missed the long name"
        );
        assert!(
            !holders_of(&found, "zeroship_auth_test").contains(&pid),
            "the prefix was reported held"
        );
    }

    /// The shape of a run whose every check answered: nothing missing, and a
    /// drop list with something on it. Every case below changes ONE field of
    /// this and nothing else, so what moved the verdict is never in doubt.
    fn complete() -> Evidence {
        Evidence {
            proc_scan_failed: false,
            proc_unlistable: false,
            proc_hidden: false,
            proc_examined: 495,
            proc_peer_env_read: 77,
            proc_unexplained: Vec::new(),
            sessions_query_ok: true,
            git_listing_ok: true,
            worktrees_seen: 10,
            worktrees_fingerprinted: 10,
            doomed: 1,
        }
    }

    #[test]
    fn complete_evidence_proceeds() {
        assert_eq!(gaps(&complete()), Vec::new());
        assert_eq!(verdict(&complete()), Verdict::Proceed);
    }

    #[test]
    fn a_genuinely_empty_population_is_not_a_refusal() {
        // THE CONTROL for every red case below, and the failure that would be
        // worse than the one being fixed: a sweeper that refuses when it
        // successfully found nothing to do is a sweeper nobody runs, and the
        // leak it was built for comes back. One variable against
        // `complete_evidence_proceeds`: the drop list is empty.
        let evidence = Evidence {
            doomed: 0,
            ..complete()
        };
        assert_eq!(verdict(&evidence), Verdict::Proceed);
    }

    #[test]
    fn each_blind_input_refuses_on_its_own_and_names_itself() {
        // One variable per row, against `complete()`. A single case cannot tell
        // a verdict that reads its input from one that refuses on everything,
        // so the row set is paired with `complete_evidence_proceeds` above.
        let cases: Vec<(&str, Evidence)> = vec![
            (
                "proc_scan",
                Evidence {
                    proc_scan_failed: true,
                    ..complete()
                },
            ),
            (
                "proc_unlistable",
                Evidence {
                    proc_unlistable: true,
                    ..complete()
                },
            ),
            (
                "proc_hidden",
                Evidence {
                    proc_hidden: true,
                    ..complete()
                },
            ),
            (
                "proc_floor",
                Evidence {
                    proc_examined: PROC_PIDS_FLOOR - 1,
                    ..complete()
                },
            ),
            (
                "env_blind",
                Evidence {
                    proc_peer_env_read: 0,
                    ..complete()
                },
            ),
            (
                "proc_unexplained",
                Evidence {
                    proc_unexplained: vec![4242],
                    ..complete()
                },
            ),
            (
                "sessions",
                Evidence {
                    sessions_query_ok: false,
                    ..complete()
                },
            ),
            (
                "git_listing",
                Evidence {
                    git_listing_ok: false,
                    ..complete()
                },
            ),
            (
                "worktrees",
                Evidence {
                    worktrees_fingerprinted: 9,
                    ..complete()
                },
            ),
        ];

        for (id, evidence) in cases {
            let Verdict::Refuse(gaps) = verdict(&evidence) else {
                panic!("{id}: did not refuse");
            };
            assert!(
                gaps.iter().any(|gap| gap.id == id),
                "{id}: refused without naming it, got {:?}",
                gaps.iter().map(|gap| gap.id).collect::<Vec<_>>()
            );
            let text = refusal_text(&gaps, evidence.doomed);
            assert!(text.starts_with("REFUSED:"), "{id}: {text}");
            assert!(text.contains(id), "{id}: refusal does not name it: {text}");
        }
    }

    #[test]
    fn a_gap_with_an_empty_drop_list_warns_instead_of_refusing() {
        // Every gap can only ADD to the drop list, so an empty one is a
        // conclusion no further evidence could overturn. It still gets said out
        // loud -- a silent count is the defect, not the exit code alone.
        let evidence = Evidence {
            sessions_query_ok: false,
            doomed: 0,
            ..complete()
        };
        let Verdict::ProceedWithGaps(gaps) = verdict(&evidence) else {
            panic!("refused with nothing to drop");
        };
        assert_eq!(gaps.len(), 1);
        assert!(gap_warning_text(&gaps).contains("sessions"));
    }

    #[test]
    fn an_unlistable_proc_reports_one_gap_and_not_three() {
        // /proc that cannot be listed also has zero examined entries and no
        // pid 1, so a naive rule would emit `proc_floor` and `proc_hidden`
        // beside it and bury the one fact that explains all three.
        let evidence = Evidence {
            proc_unlistable: true,
            proc_hidden: true,
            proc_examined: 0,
            proc_peer_env_read: 0,
            ..complete()
        };
        let ids: Vec<&str> = gaps(&evidence).iter().map(|gap| gap.id).collect();
        assert_eq!(ids, vec!["proc_unlistable"]);
    }

    #[test]
    fn a_real_proc_pass_accounts_for_every_entry_and_leaves_no_gap() {
        // The real process table, which is the only place the accounting can be
        // exercised. The old code lumped 406 of 495 entries into one
        // `unreadable` counter and let the sweep proceed on it; a number that
        // large on a healthy run could never have separated a healthy pass from
        // a blind one. What must hold here is that the healthy run produces NO
        // GAP, because a refusal that fires on an ordinary run is a tool nobody
        // keeps running.
        let held = scan_holders(&[], std::process::id() as i32);
        let evidence = Evidence {
            proc_unlistable: held.proc_unlistable,
            proc_hidden: held.proc_hidden,
            proc_examined: held.examined,
            proc_peer_env_read: held.peer_env_read,
            proc_unexplained: held.unexplained.clone(),
            sessions_query_ok: true,
            git_listing_ok: true,
            worktrees_seen: 1,
            worktrees_fingerprinted: 1,
            proc_scan_failed: false,
            doomed: 1,
        };
        assert_eq!(
            gaps(&evidence),
            Vec::new(),
            "an ordinary /proc pass produced a gap: examined {} peer_env_read \
             {} env_denied {} vanished {} unexplained {:?}",
            held.examined,
            held.peer_env_read,
            held.env_denied,
            held.vanished,
            held.unexplained,
        );
    }

    #[test]
    fn the_scan_finds_a_holder_through_its_command_line_alone() {
        // The partner of `..._through_its_environment_alone`. Requiring BOTH
        // files to be readable threw away the command line of every process
        // this user does not own -- 390 of them on this machine -- so the argv
        // half has to be shown working on its own.
        let needle = format!("zeroship_billing_test_eeee{}", std::process::id());
        let names = vec![needle.clone()];
        let stranger = 2147483647;

        // `sh -c <script> <extra>` rather than `sleep 60 <needle>`: `sleep`
        // rejects the extra argument, exits at once, and leaves a ZOMBIE whose
        // cmdline is empty -- which is what the first draft of this test
        // measured while believing it measured argv matching. The trailing `:`
        // keeps the shell from exec'ing itself away and taking the argv with
        // it.
        let mut holder = std::process::Command::new("sh")
            .args(["-c", "sleep 60; :", &needle])
            .spawn()
            .expect("spawn holder");
        let pid = holder.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(300));
        let found = scan_holders(&names, stranger);
        holder.kill().ok();
        holder.wait().ok();

        assert!(
            holders_of(&found, &needle).contains(&pid),
            "missed the holder {pid} whose only mention of the name is argv"
        );
    }

    #[test]
    fn a_process_that_exited_is_vanished_and_not_unexplained() {
        // A process that has exited is not a process that is alive, so skipping
        // it is provably right. Only a LIVE entry that cannot be read has no
        // explanation, and that is the one that must refuse.
        assert!(matches!(
            read_pid(Path::new("/proc/2147483646")),
            PidRead::Vanished
        ));

        // A ZOMBIE keeps its /proc entry, so "the directory is gone" does not
        // catch it: empty cmdline, refused environ, exactly the shape of a live
        // process with a protected environment. Reaping is deferred until after
        // the read, which is what makes the process a zombie rather than gone.
        let mut dead = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = dead.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(200));
        let verdict = read_pid(&Path::new("/proc").join(pid.to_string()));
        dead.wait().ok();
        assert!(
            matches!(verdict, PidRead::Vanished),
            "a zombie was classified {verdict:?}"
        );

        // One variable changed: an entry with no readable cmdline whose `stat`
        // says the process is RUNNING. That is the only shape with no benign
        // story, and the only one that must stop a sweep.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("stat"), "42 (sh) S 1 42 42 0 -1 0\n").expect("write stat");
        assert!(matches!(read_pid(dir.path()), PidRead::Unexplained));
    }

    #[test]
    fn tokens_split_on_every_non_identifier_byte() {
        let got = identifier_tokens(b"postgres://u:p@h:5432/zeroship_auth_test_s46\0X=1");
        let got: Vec<&str> = got
            .iter()
            .map(|t| std::str::from_utf8(t).unwrap())
            .collect();
        assert!(got.contains(&"zeroship_auth_test_s46"));
        assert!(!got.contains(&"zeroship_auth_test"));
        assert!(got.contains(&"5432"));
    }
}
