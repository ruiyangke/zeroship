// The generated test overlay -- `deploy/ops/zeroship.test.toml`.
//
// WHAT THIS REPLACES. Every suite used to carry its own copy of the test
// backends' coordinates and then export the resulting DSN under whichever
// names the crates it ran happened to read. Two copies of the address drift;
// five names for one value means a crate whose name nobody exported runs
// against nothing and reports passes. `tests/provision_test_backends.sh`
// writes the overlay in the platform's own config schema and this module reads
// it. Rust *service* code reads the same document through
// `zeroship_core::config::test_overlay`, which parses it with `FileConfig`
// under `deny_unknown_fields`.
//
// The scalar reader serves the harness's database settings. KV tests own
// their backend configuration and containers. Platform code validates the
// complete overlay through its generated configuration contract.
//
// WHAT IT DOES NOT DO. It does not invent a DSN when the file is missing. A
// suite that silently falls back to a compiled default when its configuration
// is absent is the deleted `ZEROSHIP_REQUIRE_LIVE_BACKENDS` flag in another
// costume: it converts "there is no configuration" into "the run passed".

use std::path::{Path, PathBuf};

/// Read one `section.key` out of an overlay document.
///
/// Mirrors the awk the shell library used, deliberately and line for line: a
/// `#` comment is skipped, a line whose first non-space character is `[` starts
/// a section and everything from the first `]` is discarded, and inside the
/// wanted section the first `key = value` whose key matches wins. Surrounding
/// double quotes are stripped from the value.
pub fn get(document: &str, want_section: &str, want_key: &str) -> Option<String> {
    let mut section = String::new();
    for line in document.lines() {
        let trimmed_start = line.trim_start();
        if trimmed_start.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed_start.strip_prefix('[') {
            section = match rest.find(']') {
                Some(end) => rest[..end].to_string(),
                None => rest.to_string(),
            };
            continue;
        }
        if section != want_section {
            continue;
        }
        let line = trimmed_start;
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim_end();
        if key != want_key {
            continue;
        }
        let mut value = line[eq + 1..].trim().to_string();
        // `gsub(/^"|"$/, "", v)` -- a leading quote and a trailing quote, each
        // removed at most once, and independently of the other.
        if value.starts_with('"') {
            value.remove(0);
        }
        if value.ends_with('"') {
            value.pop();
        }
        return Some(value);
    }
    None
}

/// Everything one overlay load establishes, in one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// `ZS_TEST_OVERLAY` -- the file that was read.
    pub overlay: PathBuf,
    /// `ZS_TEST_PG_DSN` -- the server DSN exactly as written.
    pub dsn: String,
    pub host: String,
    pub port: String,
    pub user: String,
    pub pass: String,
    pub db: String,
}

/// Where the overlay lives under a repository root.
pub fn overlay_path(root: &Path) -> PathBuf {
    root.join("deploy/ops/zeroship.test.toml")
}

/// Read the overlay under `root` and split its `[control] database_url`.
///
/// The error is the exact multi-line refusal the shell printed, so a caller
/// that greps for a phrase keeps finding it.
pub fn load(root: &Path) -> Result<Loaded, String> {
    let overlay = overlay_path(root);
    let Ok(document) = std::fs::read_to_string(&overlay) else {
        return Err(format!(
            "FATAL: no test overlay at {}\n\
             \x20      It names the PostgreSQL the suites dial, and is\n\
             \x20      written alongside them by:\n\
             \x20        tests/provision_test_backends.sh\n",
            overlay.display()
        ));
    };

    let dsn = get(&document, "control", "database_url").unwrap_or_default();

    if dsn.is_empty() {
        return Err(format!(
            "FATAL: {} has no [control] database_url.\n\
             \x20      Re-run tests/provision_test_backends.sh to rewrite it.\n",
            overlay.display()
        ));
    }

    let split = split_dsn(&dsn);
    Ok(Loaded {
        overlay,
        dsn,
        host: split.host,
        port: split.port,
        user: split.user,
        pass: split.pass,
        db: split.db,
    })
}

/// The pieces of a `postgres://user:pass@host:port/db` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DsnParts {
    pub host: String,
    pub port: String,
    pub user: String,
    pub pass: String,
    pub db: String,
}

/// Split a DSN without a URL parser, exactly as the shell did.
///
/// The password is taken to the LAST `@` inside the authority, so a password
/// containing `@` cannot truncate the host -- the same rule `redact_dsn`
/// applies in `libs/compio-postgres/tests/common/mod.rs`. The helpers below are
/// bash's own parameter expansions, kept literal so the two implementations can
/// be compared line by line rather than by intent.
pub fn split_dsn(dsn: &str) -> DsnParts {
    // `${dsn#*://}`
    let mut authority = after_first(dsn, "://").to_string();
    // `${authority#*/}` then `${db%%\?*}`. A DSN with no `/` after the
    // authority leaves the first expansion unchanged, so `db` becomes the
    // authority itself -- bash does that, and so does this.
    let db = before_first(after_first(&authority, "/"), "?").to_string();
    // `${authority%%/*}`
    authority = before_first(&authority, "/").to_string();

    let (user, pass, hostport) = if authority.contains('@') {
        // `${authority%@*}` and `${authority##*@}`
        let userinfo = before_last(&authority, "@");
        let hostport = after_last(&authority, "@");
        let user = before_first(userinfo, ":");
        let pass_candidate = after_first(userinfo, ":");
        // Bash: `${userinfo#*:}` is the whole string when there is no colon, and
        // the shell then blanked it. Without that arm a password-less DSN gets
        // the USERNAME as its password.
        let pass = if pass_candidate == userinfo {
            ""
        } else {
            pass_candidate
        };
        (user.to_string(), pass.to_string(), hostport.to_string())
    } else {
        (String::new(), String::new(), authority.clone())
    };

    // `${hostport%%:*}` / `${hostport##*:}`, and 5432 when there was no colon.
    let host = before_first(&hostport, ":").to_string();
    let port_candidate = after_last(&hostport, ":");
    let port = if port_candidate == hostport {
        "5432".to_string()
    } else {
        port_candidate.to_string()
    };

    DsnParts {
        host,
        port,
        user,
        pass,
        db,
    }
}

/// `${s#*needle}` -- everything after the FIRST `needle`, or `s` when absent.
fn after_first<'a>(s: &'a str, needle: &str) -> &'a str {
    match s.find(needle) {
        Some(i) => &s[i + needle.len()..],
        None => s,
    }
}

/// `${s##*needle}` -- everything after the LAST `needle`, or `s` when absent.
fn after_last<'a>(s: &'a str, needle: &str) -> &'a str {
    match s.rfind(needle) {
        Some(i) => &s[i + needle.len()..],
        None => s,
    }
}

/// `${s%%needle*}` -- everything before the FIRST `needle`, or `s` when absent.
fn before_first<'a>(s: &'a str, needle: &str) -> &'a str {
    match s.find(needle) {
        Some(i) => &s[..i],
        None => s,
    }
}

/// `${s%needle*}` -- everything before the LAST `needle`, or `s` when absent.
fn before_last<'a>(s: &'a str, needle: &str) -> &'a str {
    match s.rfind(needle) {
        Some(i) => &s[..i],
        None => s,
    }
}

/// What the caller asked for, when they asked for anything.
///
/// An empty string is "did not ask": the shell tested `[ -n "$want_host" ]`,
/// and a caller who exported nothing is asking for nothing.
///
/// NOTHING HERE READS THE PROCESS ENVIRONMENT. `PG_HOST`/`PG_PORT`/`PG_USER`/
/// `PG_PASS` are what a *shell* caller sets, and the shell shim reads its own
/// environment and hands the four values to this binary explicitly. A Rust
/// helper that reached for them itself would be ambient configuration wearing a
/// function's clothes: two callers with different environments would get
/// different answers from identical arguments, which is the property that makes
/// an ambient opt-out impossible to review.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub host: String,
    pub port: String,
    pub user: String,
    pub pass: String,
}

impl Wanted {
    /// Parse the `key=value` block the shim writes on stdin.
    ///
    /// STDIN RATHER THAN ARGV, for the password alone but applied to all four
    /// so there is one channel rather than two. `PG_PASS` on a command line is
    /// readable by every user on the box through `ps` and through
    /// `/proc/<pid>/cmdline` -- which this same crate's [`super::sweep`] scanner
    /// reads by design. Unknown keys are ignored rather than refused: this is a
    /// private channel between the shim and the binary, and a refusal here
    /// would fire on the shim, not on anything a user typed.
    pub fn from_block(block: &str) -> Wanted {
        let mut out = Wanted::default();
        for line in block.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "host" => out.host = value.to_string(),
                "port" => out.port = value.to_string(),
                "user" => out.user = value.to_string(),
                "pass" => out.pass = value.to_string(),
                _ => {}
            }
        }
        out
    }
}

/// Refuse when the caller asked for one server and the overlay names another.
///
/// WHY A REFUSAL AND NOT A WARNING. A harness reads `PG_HOST`/`PG_PORT` for its
/// OWN database calls; every SERVICE it spawns reads the overlay through
/// `zeroship_core::config::test_overlay`. Point a suite at a second cluster with
/// `PG_PORT=5444` and the two halves connect to two different servers -- the
/// provisioning and the probes on one, the code under test on the other.
/// Nothing announces that. The run completes and reports a plausible number
/// computed against two databases, which is the worst available outcome: not a
/// failure, a WRONG MEASUREMENT that reads like a result.
///
/// `localhost`, `127.0.0.1` and `::1` are the same host and are treated as
/// such. They differ as strings and CI writes one while the generator writes
/// the other, so comparing them literally would refuse every CI run for no
/// reason -- a gate that cries wolf is a gate somebody deletes.
pub fn assert_agrees(have: &Loaded, want: &Wanted) -> Result<(), String> {
    let mut mismatch = String::new();

    if !want.host.is_empty() && !same_host(&want.host, &have.host) {
        mismatch.push_str(&format!(
            "  PG_HOST: you asked for '{}', the overlay says '{}'\n",
            want.host, have.host
        ));
    }
    if !want.port.is_empty() && want.port != have.port {
        mismatch.push_str(&format!(
            "  PG_PORT: you asked for '{}', the overlay says '{}'\n",
            want.port, have.port
        ));
    }
    if !want.user.is_empty() && want.user != have.user {
        mismatch.push_str(&format!(
            "  PG_USER: you asked for '{}', the overlay says '{}'\n",
            want.user, have.user
        ));
    }
    // The password is compared but never printed.
    if !want.pass.is_empty() && want.pass != have.pass {
        mismatch.push_str("  PG_PASS: differs from the overlay's (values not shown)\n");
    }

    if mismatch.is_empty() {
        return Ok(());
    }

    let fix_host = if want.host.is_empty() {
        &have.host
    } else {
        &want.host
    };
    let fix_port = if want.port.is_empty() {
        &have.port
    } else {
        &want.port
    };
    Err(format!(
        "FATAL: the server you asked for is not the server the overlay names.\n\
         {mismatch}\
         \x20      {overlay}\n\
         \x20      This is not a preference the harness can honour halfway. Its own\n\
         \x20      psql calls would go to your server while every service it starts\n\
         \x20      read the overlay and went to the other one, and the run would\n\
         \x20      report a number computed against two different databases.\n\
         \x20      Regenerate the overlay for the server you mean:\n\
         \x20        PG_HOST={fix_host} PG_PORT={fix_port} \\\n\
         \x20          tests/provision_test_backends.sh\n",
        overlay = have.overlay.display(),
    ))
}

fn same_host(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let loopback = |h: &str| matches!(h, "localhost" | "127.0.0.1" | "::1");
    loopback(a) && loopback(b)
}
