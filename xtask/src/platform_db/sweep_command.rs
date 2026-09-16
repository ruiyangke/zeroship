//! The operator entrypoint for the sweeper: decide which suite databases no
//! branch can still need, and optionally drop them.
//!
//! THE DECISIONS ARE NOT HERE. `sweep` owns the verdicts and `fingerprint` owns
//! the hashing; this module gathers what they rule on - the databases on the
//! server, the schemas the repository can still produce, and the local
//! processes holding a name - and acts on the answer. That split is what lets
//! the verdicts be tested without a server and without `/proc`.
//!
//! A WRONG VERDICT IS A DROPPED DATABASE. Two asymmetries follow from that and
//! neither is an oversight: a plain `DROP DATABASE` (never `WITH (FORCE)`), and
//! a refusal to drop whenever the evidence is incomplete. See
//! [`super::sweep::verdict`].

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use super::{admin, exit, fingerprint, overlay, sweep};

/// What the caller asked for.
#[derive(Debug, Default)]
pub struct Args {
    /// Actually drop. Without it the plan is printed and nothing is destroyed.
    pub apply: bool,
    /// Also consider pre-hash names, whose reachability cannot be decided.
    pub legacy: bool,
    pub host: Option<String>,
    pub port: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
}

/// Reclaim the test databases no branch can still need.
pub fn run(root: &Path, args: &Args) -> i32 {
    let loaded = match overlay::load(root) {
        Ok(loaded) => loaded,
        Err(refusal) => return refuse(&refusal),
    };
    let mut server = match admin::Server::from_overlay(&loaded) {
        Ok(server) => server,
        Err(refusal) => return refuse(&refusal),
    };
    if let Some(host) = &args.host {
        server.host = host.clone();
    }
    if let Some(port) = &args.port {
        match port.parse() {
            Ok(port) => server.port = port,
            Err(_) => {
                return refuse(&format!(
                    "FATAL: --port '{port}' is not a port number.\n"
                ));
            }
        }
    }
    if let Some(user) = &args.user {
        server.user = user.clone();
    }
    if let Some(password) = &args.password {
        server.pass = password.clone();
    }

    println!("==> Collecting the fingerprints this repository can still produce");
    let mut reachable: BTreeSet<String> = BTreeSet::new();
    let mut git_listing_ok = true;

    let worktrees = git(root, &["worktree", "list", "--porcelain"]);
    let mut worktrees_seen = 0usize;
    let mut worktrees_fingerprinted = 0usize;
    match worktrees {
        Err(why) => {
            git_listing_ok = false;
            eprintln!("    WARNING: git worktree list failed: {why}");
        }
        Ok(listing) => {
            for line in listing.lines() {
                let Some(path) = line.strip_prefix("worktree ") else {
                    continue;
                };
                worktrees_seen += 1;
                match fingerprint::of_dir(Path::new(path)) {
                    Ok(value) if !value.is_empty() => {
                        worktrees_fingerprinted += 1;
                        if reachable.insert(value.clone()) {
                            println!("    {value}  working tree {path}");
                        }
                    }
                    _ => println!("    --            working tree {path} HAS NO FINGERPRINT"),
                }
            }
        }
    }

    match git(root, &["for-each-ref", "--format=%(refname)", "refs/heads", "refs/remotes"]) {
        Err(why) => {
            git_listing_ok = false;
            eprintln!("    WARNING: git for-each-ref failed: {why}");
        }
        Ok(refs) => {
            for reference in refs.lines().filter(|l| !l.is_empty()) {
                let Some(value) = fingerprint::of_ref(root, reference) else {
                    continue;
                };
                if reachable.insert(value.clone()) {
                    println!("    {value}  {reference}");
                }
            }
        }
    }

    if reachable.is_empty() {
        eprintln!("FATAL: no fingerprint could be computed from any branch or worktree.");
        eprintln!("       Every database would look unreachable, so this refuses to sweep.");
        return exit::FATAL;
    }

    // The coordinates the run will actually use, so every message below names
    // the server under test rather than the overlay's - they differ whenever
    // `--host`/`--port` is passed.
    let endpoint = format!("{}:{}", server.host, server.port);
    println!("==> Reading databases from {endpoint}");
    let pg = admin::PgAdmin::new(server);
    let all = match pg.databases() {
        Ok(all) => all,
        Err(why) => {
            eprintln!("FATAL: could not list databases on {endpoint} ({why}).");
            return exit::FATAL;
        }
    };
    println!("    {} databases", all.len());

    let sessions = match pg.session_counts() {
        Ok(sessions) => Some(sessions),
        Err(why) => {
            eprintln!("    WARNING: the pg_stat_activity query did not return ({why}).");
            eprintln!("             Session counts below read '?', not 0.");
            None
        }
    };

    let candidates: Vec<String> = all
        .iter()
        .filter(|name| sweep::family_of(name).is_some())
        .cloned()
        .collect();
    println!(
        "==> {} of the databases on this server belong to a swept family",
        candidates.len()
    );
    if candidates.is_empty() {
        eprintln!("    NOTE: no database on this server names a swept family.");
    }

    println!("==> Scanning /proc for runs holding one of these names");
    let self_pid = std::process::id() as i32;
    let holders = sweep::scan_holders(&candidates, self_pid);

    println!();
    println!("{:<46} {:<10} {}", "DATABASE", "VERDICT", "WHY");
    println!("{}", "-".repeat(81));

    let mut doomed: Vec<String> = Vec::new();
    let mut kept = 0usize;
    let mut keyed_live = 0usize;
    let mut held = 0usize;
    let mut legacy_skipped = 0usize;

    for database in &all {
        let Some(family) = sweep::family_of(database) else {
            kept += 1;
            continue;
        };
        let suffix = database
            .strip_prefix(family)
            .map(|rest| rest.strip_prefix('_').unwrap_or(rest))
            .unwrap_or("");

        let (mut verdict, mut why) = if is_schema_key(suffix) {
            if reachable.contains(suffix) {
                keyed_live += 1;
                ("KEEP", "schema is reachable".to_owned())
            } else {
                ("DOOMED", format!("no branch or worktree produces {suffix}"))
            }
        } else if args.legacy {
            ("DOOMED", "pre-hash name; reachability undecidable".to_owned())
        } else {
            legacy_skipped += 1;
            ("KEEP", "pre-hash name; needs --legacy".to_owned())
        };

        // Liveness is checked for EVERY candidate, including the ones already
        // headed for KEEP: a table that named only the runs it was about to
        // delete would leave a reader unable to see the checks fire at all.
        let sessions_attached = sessions
            .as_ref()
            .and_then(|rows| rows.iter().find(|(name, _)| name == database))
            .map(|(_, count)| *count);
        let local_holders = sweep::holders_of(&holders, database);

        let mut reason = String::new();
        if let Some(count) = sessions_attached.filter(|count| *count != 0) {
            reason = format!("{count} session(s) attached");
        }
        if !local_holders.is_empty() {
            if !reason.is_empty() {
                reason.push_str("; ");
            }
            let pids: Vec<String> = local_holders.iter().map(i32::to_string).collect();
            reason.push_str(&format!("local pid(s) {} hold the name", pids.join(" ")));
        }
        if !reason.is_empty() {
            if verdict == "DOOMED" {
                held += 1;
                verdict = "IN USE";
            }
            why = reason;
        }

        if verdict == "DOOMED" {
            doomed.push(database.clone());
        }
        println!("{database:<46} {verdict:<10} {why}");
    }

    println!();
    println!("    {kept} outside the suite families and never considered");
    println!("    {keyed_live} schema-keyed and reachable");
    println!("    {legacy_skipped} pre-hash, skipped (pass --legacy to consider them)");
    println!("    {held} in use");
    println!("    {} to drop", doomed.len());
    println!();
    println!("    what the liveness scan ruled on, and what it could not:");
    println!(
        "      {worktrees_fingerprinted}/{worktrees_seen} working trees fingerprinted"
    );
    println!("      {} /proc entries examined", holders.examined);
    println!(
        "      {} environments read for processes outside this run",
        holders.peer_env_read
    );
    println!("      {} entries gave their argv and not their environment", holders.env_denied);
    println!("      {} entries belonged to processes that had exited", holders.vanished);
    println!(
        "      {}",
        if holders.unexplained.is_empty() {
            "none live entries with no readable argv".to_owned()
        } else {
            format!(
                "{} live entries with no readable argv",
                holders.unexplained.len()
            )
        }
    );

    let evidence = sweep::Evidence {
        // The scan is a direct call here rather than a subprocess, so it cannot
        // exit without having run; `proc_unlistable` carries the failure that
        // used to arrive as a non-zero exit.
        proc_scan_failed: false,
        proc_unlistable: holders.proc_unlistable,
        proc_hidden: holders.proc_hidden,
        proc_examined: holders.examined,
        proc_peer_env_read: holders.peer_env_read,
        proc_unexplained: holders.unexplained.clone(),
        sessions_query_ok: sessions.is_some(),
        git_listing_ok,
        worktrees_seen,
        worktrees_fingerprinted,
        doomed: doomed.len(),
    };

    println!();
    match sweep::verdict(&evidence) {
        sweep::Verdict::Proceed => {}
        sweep::Verdict::ProceedWithGaps(gaps) => print!("{}", sweep::gap_warning_text(&gaps)),
        sweep::Verdict::Refuse(gaps) => {
            print!("{}", sweep::refusal_text(&gaps, doomed.len()));
            println!("==> nothing was dropped.");
            return exit::BLIND;
        }
    }

    if doomed.is_empty() {
        println!("==> scanned {keyed_live} reachable and {held} in use; nothing dead");
        return exit::OK;
    }

    if !args.apply {
        println!("==> DRY RUN. Re-run with --apply to drop the {} above.", doomed.len());
        return exit::OK;
    }

    let mut dropped = 0usize;
    let mut refused: Vec<String> = Vec::new();
    for database in &doomed {
        match pg.drop_database(database) {
            Ok(()) => {
                println!("    dropped  {database}");
                dropped += 1;
            }
            Err(why) => {
                let first: String = why.lines().take(2).collect::<Vec<_>>().join(" ");
                println!("    REFUSED  {database}: {first}");
                refused.push(database.clone());
            }
        }
    }

    println!("==> dropped {dropped}, refused {}", refused.len());
    if !refused.is_empty() {
        // A refusal is REPORTED BY NAME, never absorbed into a count that reads
        // as success. The server refusing a plain DROP means somebody attached
        // between the liveness scan and this line - the layer working, and the
        // strongest evidence available that the scan was wrong about it.
        eprintln!("    the server refused these, so a session was attached to each:");
        for database in &refused {
            eprintln!("      {database}");
        }
        eprintln!("    Each one was on the drop list, so the liveness scan called it");
        eprintln!("    dead and the server disagreed. Nothing was forced; nothing lost.");
        return exit::NO;
    }
    exit::OK
}

/// A schema-keyed name: the 12 hex characters of a migration-set hash.
fn is_schema_key(suffix: &str) -> bool {
    suffix.len() == 12
        && suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|why| format!("could not run git: {why}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} exited {:?}",
            args.join(" "),
            output.status.code()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn refuse(message: &str) -> i32 {
    eprint!("{message}");
    exit::FATAL
}
