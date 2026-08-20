//! The 12-hex fingerprint the branch-keyed suite database is named after.
//!
//! ONE CONSTRUCTION, TWO SOURCES, and they MUST agree byte for byte. [`of_dir`]
//! reads a working tree; [`of_ref`] reads a git tree. `tests/lib/suite_db.sh`
//! names a database with the first and `tests/sweep_test_databases.sh` decides
//! whether that database is still reachable with the second. If they ever
//! disagree, every database on the server matches no reachable branch, the
//! sweeper calls the lot dead, and one `--apply` deletes every agent's work at
//! once. That is the single most destructive bug this design admits, which is
//! why the pair is pinned against the REAL repository rather than a fixture --
//! see `tests/lib_sweep_db_selftest.sh`. A fixture would pin the two
//! implementations to each other and prove nothing about the files the sweeper
//! actually reasons over.
//!
//! WHY THE BASENAME GOES INTO THE HASH BESIDE THE BYTES. The platform runner
//! orders by filename and journals under it, so `20260101_a.ts` and
//! `20260301_a.ts` with identical bytes are two different schemas and must not
//! share a name.
//!
//! WHY CONTENTS AND NOT FILENAMES ALONE. A branch that edits an existing
//! migration in place changes the schema without changing the file list, and
//! the platform runner keys its journal on a sha256 of each file's source
//! bytes -- so a name-only hash would hand that branch a database whose journal
//! refuses every later run with `ChecksumMismatch`.
//!
//! THE WIRE FORMAT IS `sha256sum`'s, deliberately. Each entry is
//! `<basename> <64 hex>  -\n`: the two spaces and the `-` are what GNU
//! `sha256sum` prints when it reads stdin, and the shell built the digest by
//! piping `sha256sum <file` and `git cat-file blob | sha256sum` into `sort`.
//! Reproducing it here is what lets the ported and unported halves of the
//! harness agree during the migration, and it costs nothing to keep afterwards.

use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

/// The set hashed is exactly `discover_ts_files`'s
/// (`crates/zeroship-migrate-adapter/src/platform.rs`): `*.ts` in this
/// directory, ordered by filename.
pub const MIGRATIONS_DIR: &str = "db/migrations-ts";

/// Hash the migration set a working tree would apply.
///
/// `Err` is the refusal text; the caller exits 2. Never a fingerprint of
/// nothing -- an empty set hashes to a FIXED value, so every broken checkout
/// would share one database and call it schema-fresh.
pub fn of_dir(root: &Path) -> Result<String, String> {
    let dir = root.join(MIGRATIONS_DIR);
    if !dir.is_dir() {
        return Err(format!(
            "FATAL: no platform migrations directory at {}\n\
             \x20      The suite database is named after the migration set; without\n\
             \x20      one there is nothing to name it after.\n",
            dir.display()
        ));
    }

    let mut lines: Vec<String> = Vec::new();
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
        let bytes = std::fs::read(entry.path())
            .map_err(|e| format!("FATAL: could not read {}: {e}\n", entry.path().display()))?;
        lines.push(sha256sum_line(&name, &bytes));
    }

    if lines.is_empty() {
        return Err(format!(
            "FATAL: {} holds no *.ts migrations\n\
             \x20      An empty set would hash to a fixed value, so every broken\n\
             \x20      checkout would share one database and call it fresh.\n",
            dir.display()
        ));
    }

    Ok(digest_of(lines))
}

/// Hash the migration set a git tree carries.
///
/// `Err(())` rather than a message: the shell's `zs_fingerprint_of_ref` printed
/// nothing and returned 1, and the sweeper treats "this ref has no migrations"
/// as an ordinary answer about a ref, not as a failure worth narrating. A
/// fingerprint for a tree with no migrations would be a legitimate-looking hash
/// no working tree can ever produce, so every database keyed to it would look
/// reachable forever.
pub fn of_ref(reference: &str) -> Result<String, ()> {
    let listing = Command::new("git")
        .args(["ls-tree", reference, "--", &format!("{MIGRATIONS_DIR}/")])
        .output()
        .map_err(|_| ())?;
    if !listing.status.success() {
        return Err(());
    }
    let listing = String::from_utf8_lossy(&listing.stdout);
    if listing.trim().is_empty() {
        return Err(());
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
        let (_mode, kind, sha) = match (fields.next(), fields.next(), fields.next()) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => continue,
        };
        if kind != "blob" || !path.ends_with(".ts") {
            continue;
        }
        let blob = Command::new("git")
            .args(["cat-file", "blob", sha])
            .output()
            .map_err(|_| ())?;
        if !blob.status.success() {
            return Err(());
        }
        let basename = path.rsplit('/').next().unwrap_or(path);
        lines.push(sha256sum_line(basename, &blob.stdout));
    }

    if lines.is_empty() {
        return Err(());
    }
    Ok(digest_of(lines))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(dir: &Path, files: &[(&str, &str)]) {
        std::fs::create_dir_all(dir.join(MIGRATIONS_DIR)).unwrap();
        for (name, body) in files {
            std::fs::write(dir.join(MIGRATIONS_DIR).join(name), body).unwrap();
        }
    }

    /// A throwaway tree that looks like a repo root: only `db/migrations-ts`
    /// is ever read out of it. `TempDir` removes it on drop, so the caller
    /// holds it for the length of the test.
    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("scratch tree")
    }

    #[test]
    fn identical_migration_sets_in_different_directories_agree() {
        // The absolute path differs between the two, which is the property that
        // has to hold or two worktrees of one commit never share a database.
        let (a, b) = (scratch(), scratch());
        tree(a.path(), &[("20260101_one.ts", "one"), ("20260202_two.ts", "two")]);
        tree(b.path(), &[("20260101_one.ts", "one"), ("20260202_two.ts", "two")]);
        let fa = of_dir(a.path()).unwrap();
        assert_eq!(fa, of_dir(b.path()).unwrap());
        assert_eq!(fa.len(), 12);
        assert!(fa.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_fingerprint_moves_when_the_schema_does() {
        // Three ways it can move, and all three matter: a function returning a
        // constant passes the agreement test above on its own.
        let base = scratch();
        tree(base.path(), &[("20260101_one.ts", "one"), ("20260202_two.ts", "two")]);
        let f = of_dir(base.path()).unwrap();

        let added = scratch();
        tree(
            added.path(),
            &[
                ("20260101_one.ts", "one"),
                ("20260202_two.ts", "two"),
                ("20260303_three.ts", "three"),
            ],
        );
        assert_ne!(f, of_dir(added.path()).unwrap(), "an added migration");

        let edited = scratch();
        tree(
            edited.path(),
            &[("20260101_one.ts", "one, but different"), ("20260202_two.ts", "two")],
        );
        assert_ne!(f, of_dir(edited.path()).unwrap(), "an edited migration");

        // Bytes untouched, name changed. The runner orders by filename and
        // journals under it, so this is a different schema; a digest over
        // contents alone would miss it.
        let renamed = scratch();
        tree(renamed.path(), &[("20260101_one.ts", "one"), ("20269999_two.ts", "two")]);
        assert_ne!(f, of_dir(renamed.path()).unwrap(), "a renamed migration");
    }

    #[test]
    fn an_empty_or_absent_migration_set_is_refused() {
        let empty = scratch();
        std::fs::create_dir_all(empty.path().join(MIGRATIONS_DIR)).unwrap();
        let err = of_dir(empty.path()).unwrap_err();
        assert!(err.contains("holds no *.ts migrations"), "{err}");

        let nodir = scratch();
        let err = of_dir(nodir.path()).unwrap_err();
        assert!(err.contains("no platform migrations directory"), "{err}");
    }

    #[test]
    fn a_non_ts_file_is_not_part_of_the_set() {
        // `discover_ts_files` takes `*.ts` only, so a README dropped in the
        // directory must not change which database the branch lands in.
        let (a, b) = (scratch(), scratch());
        tree(a.path(), &[("20260101_one.ts", "one")]);
        tree(b.path(), &[("20260101_one.ts", "one"), ("README.md", "hello")]);
        assert_eq!(of_dir(a.path()).unwrap(), of_dir(b.path()).unwrap());
    }
}
