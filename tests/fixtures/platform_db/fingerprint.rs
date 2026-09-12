// The 12-hex fingerprint the branch-keyed suite database is named after.
//
// ONE CONSTRUCTION, TWO SOURCES, and they MUST agree byte for byte. [`of_dir`]
// reads a working tree; [`of_ref`] reads a git tree. `tests/lib/suite_db.sh`
// names a database with the first and `tests/sweep_test_databases.sh` decides
// whether that database is still reachable with the second. If they ever
// disagree, every database on the server matches no reachable branch, the
// sweeper calls the lot dead, and one `--apply` deletes every agent's work at
// once. That is the single most destructive bug this design admits, which is
// why the pair is pinned against the REAL repository rather than a fixture --
// see `tests/lib_sweep_db_selftest.sh`. A fixture would pin the two
// implementations to each other and prove nothing about the files the sweeper
// actually reasons over.
//
// THAT ARGUMENT IS ABOUT THE AGREEMENT, and the fixtures below are not trying
// to take it over. They cover what the real repository cannot be asked to
// demonstrate on command: that the hash MOVES when the migration set does and
// HOLDS when anything else does. That pair used to be sampled from history --
// "a commit 40 back must differ" -- which made the harness's verdict a
// function of how recently somebody touched `db/migrations-ts`, and it read as
// a broken fingerprint during every quiet spell.
//
// WHY THE BASENAME GOES INTO THE HASH BESIDE THE BYTES. The platform runner
// orders by filename and journals under it, so `20260101_a.ts` and
// `20260301_a.ts` with identical bytes are two different schemas and must not
// share a name.
//
// WHY CONTENTS AND NOT FILENAMES ALONE. A branch that edits an existing
// migration in place changes the schema without changing the file list, and
// the platform runner keys its journal on a sha256 of each file's source
// bytes -- so a name-only hash would hand that branch a database whose journal
// refuses every later run with `ChecksumMismatch`.
//
// THE WIRE FORMAT IS `sha256sum`'s, deliberately. Each entry is
// `<basename> <64 hex>  -\n`: the two spaces and the `-` are what GNU
// `sha256sum` prints when it reads stdin, and the shell built the digest by
// piping `sha256sum <file` and `git cat-file blob | sha256sum` into `sort`.
// Reproducing it here is what lets the ported and unported halves of the
// harness agree during the migration, and it costs nothing to keep afterwards.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The set hashed is exactly `discover_ts_files`'s
/// (`crates/zeroship-migrate-adapter/src/platform.rs`, DELETED 2026-08-28 in
/// `ccda4bb42` with the rest of the platform-migrate binary; neither the file
/// nor that function is in the tree today): `*.ts` in this directory, ordered
/// by filename.
pub const MIGRATIONS_DIR: &str = "db/migrations-ts";

/// The migration files a working tree would apply, by basename.
///
/// ONE FILTER, TWO CONSUMERS: [`of_dir`] hashes this set, and
/// [`super::live_db`] counts it against the journal of a live database. They
/// have to agree on membership or the two answers describe different corpora,
/// so the selection rule lives here and neither caller restates it.
///
/// Order is the directory's, not sorted: [`of_dir`] sorts the digest lines it
/// builds and the ledger check only needs the cardinality. A caller that needs
/// an order must impose one.
pub fn files_in(root: &Path) -> Result<Vec<PathBuf>, String> {
    let dir = root.join(MIGRATIONS_DIR);
    if !dir.is_dir() {
        return Err(format!(
            "FATAL: no platform migrations directory at {}\n\
             \x20      The suite database is named after the migration set; without\n\
             \x20      one there is nothing to name it after.\n",
            dir.display()
        ));
    }

    let mut files: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| format!("FATAL: could not read {}: {e}\n", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("FATAL: could not read {}: {e}\n", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // The shell's glob was `"$dir"/*.ts`, which does not match a leading
        // dot, followed by `[ -f "$file" ] || continue`.
        if name.starts_with('.') || !name.ends_with(".ts") || !entry.path().is_file() {
            continue;
        }
        files.push(entry.path());
    }

    if files.is_empty() {
        return Err(format!(
            "FATAL: {} holds no *.ts migrations\n\
             \x20      An empty set would hash to a fixed value, so every broken\n\
             \x20      checkout would share one database and call it fresh.\n",
            dir.display()
        ));
    }

    Ok(files)
}

/// Hash the migration set a working tree would apply.
///
/// `Err` is the refusal text; the caller exits 2. Never a fingerprint of
/// nothing -- an empty set hashes to a FIXED value, so every broken checkout
/// would share one database and call it schema-fresh.
pub fn of_dir(root: &Path) -> Result<String, String> {
    let mut lines: Vec<String> = Vec::new();
    for file in files_in(root)? {
        let name = file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let bytes = std::fs::read(&file)
            .map_err(|e| format!("FATAL: could not read {}: {e}\n", file.display()))?;
        lines.push(sha256sum_line(&name, &bytes));
    }

    Ok(digest_of(lines))
}

/// Hash the migration set a git tree carries.
///
/// `None` rather than an error message, and that is the shell's contract
/// preserved rather than an omission: `zs_fingerprint_of_ref` printed nothing
/// and returned 1, and the sweeper's loop is `fp="$(...)" || continue`. "This
/// ref carries no migrations" is an ordinary answer about a ref -- most tags
/// and many old branches -- not a failure worth narrating 21 times.
///
/// What it must never do is return a VALUE for such a tree. A fingerprint of
/// nothing is a legitimate-looking hash no working tree can ever produce, so
/// every database keyed to it would look reachable forever.
///
/// WHY `repo` IS AN ARGUMENT. It used to be the process's working directory,
/// which made this the one function here with an ambient input -- and the
/// sweeper's ref list comes from a `git for-each-ref` run somewhere else, so
/// "which repository" was agreed by coincidence rather than stated. Naming it
/// also makes the discrimination control constructible: a test can build two
/// trees that differ by exactly one migration and ask about THEM, instead of
/// sampling this repository's history and hoping a migration changed recently.
pub fn of_ref(repo: &Path, reference: &str) -> Option<String> {
    let listing = Command::new("git")
        .current_dir(repo)
        .args(["ls-tree", reference, "--", &format!("{MIGRATIONS_DIR}/")])
        .output()
        .ok()?;
    if !listing.status.success() {
        return None;
    }
    let listing = String::from_utf8_lossy(&listing.stdout);
    if listing.trim().is_empty() {
        return None;
    }

    let mut lines: Vec<String> = Vec::new();
    for entry in listing.lines() {
        // `<mode> SP <type> SP <sha> TAB <path>`. Split on the TAB rather than
        // on whitespace: bash's `read -r _mode type sha name` treated both the
        // same, which quietly meant a path with a space in it landed intact in
        // `name` only because it was the last field. Naming the separator makes
        // that deliberate instead of lucky.
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let mut fields = meta.split_whitespace();
        let (Some(_mode), Some(kind), Some(sha)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if kind != "blob" || !path.ends_with(".ts") {
            continue;
        }
        let blob = Command::new("git")
            .current_dir(repo)
            .args(["cat-file", "blob", sha])
            .output()
            .ok()?;
        if !blob.status.success() {
            return None;
        }
        let basename = path.rsplit('/').next().unwrap_or(path);
        lines.push(sha256sum_line(basename, &blob.stdout));
    }

    if lines.is_empty() {
        return None;
    }
    Some(digest_of(lines))
}

/// One line of what `printf '%s ' <name>; sha256sum < file` emits.
fn sha256sum_line(basename: &str, bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{basename} {:x}  -\n", hasher.finalize())
}

/// `LC_ALL=C sort | sha256sum`, truncated to 12.
///
/// The sort is a byte sort, which is what `LC_ALL=C` buys: under a locale that
/// folds case or ignores punctuation two checkouts could order the same files
/// differently and fingerprint differently, and no two agents would ever share
/// a database.
fn digest_of(mut lines: Vec<String>) -> String {
    lines.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let mut hasher = Sha256::new();
    for line in &lines {
        hasher.update(line.as_bytes());
    }
    let digest = format!("{:x}", hasher.finalize());
    digest[..12].to_string()
}
