// The 12-hex fingerprint of the platform migration set a working tree would
// apply.
//
// `crate::postgres::server_inputs` folds it into the identity of the shared
// platform server, so a branch that edits a migration boots a server of its own
// rather than joining one migrated from another corpus. The hash has to MOVE
// when the migration set does and HOLD when anything else does; the shared
// server contract in `tests/integration/testkit_shared_server.rs` drives both
// directions over constructed trees.
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
// THE LINE FORMAT IS `sha256sum`'s. Each entry is `<basename> <64 hex>  -\n`:
// the two spaces and the `-` are what GNU `sha256sum` prints when it reads
// stdin, so the digest can be reproduced from a shell with `sha256sum` and
// `LC_ALL=C sort` when a fingerprint needs checking by hand.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The directory holding the platform migration set.
///
/// The set is the one `discover` in `packages/zero-migrate-cli/src/cli.ts`
/// selects: every file named `*.{ts,mts,cts,js,mjs,cjs}` (case-insensitively)
/// except `*.d.ts`, ordered by filename. [`of_dir`] applies that same rule, so a
/// migration written under any of the six extensions moves the hash the same
/// way it moves the set an apply runs.
pub const MIGRATIONS_DIR: &str = "db/migrations-ts";

/// Whether `name` is a file `discover` admits, to the letter of its filter.
///
/// One of the six module extensions, case-insensitively, and not `.d.ts`. The
/// CLI's exclusion pattern is only `\.d\.ts$`, so a `.d.mts` or `.d.cts` passes
/// its filter and passes this one too; skipping more here would fingerprint a
/// smaller corpus than an apply runs.
fn is_migration_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".ts", ".mts", ".cts", ".js", ".mjs", ".cjs"]
        .iter()
        .any(|extension| lower.ends_with(extension))
        && !lower.ends_with(".d.ts")
}

/// The migration files a working tree would apply, by basename.
///
/// The order is `discover`'s: filenames sorted, the migration order contract
/// the CLI applies in.
fn files_in(root: &Path) -> Result<Vec<PathBuf>, String> {
    let dir = root.join(MIGRATIONS_DIR);
    if !dir.is_dir() {
        return Err(format!(
            "FATAL: no platform migrations directory at {}\n\
             \x20      The shared platform server is keyed to the migration set;\n\
             \x20      without one there is nothing to key it to.\n",
            dir.display()
        ));
    }

    let mut files: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| format!("FATAL: could not read {}: {e}\n", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("FATAL: could not read {}: {e}\n", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_migration_name(&name) {
            files.push(entry.path());
        }
    }
    // `discover` sorts the names it admits and applies them in that order, so
    // the ledger counts and the hash are built from the same sequence.
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    if files.is_empty() {
        return Err(format!(
            "FATAL: {} holds no migrations\n\
             \x20      An empty set would hash to a fixed value, so every broken\n\
             \x20      checkout would share one server and call it fresh.\n",
            dir.display()
        ));
    }

    Ok(files)
}

/// Hash the migration set a working tree would apply.
///
/// `Err` is the refusal text. Never a fingerprint of nothing -- an empty set
/// hashes to a FIXED value, so every broken checkout would share one server and
/// call it schema-fresh.
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
/// differently and fingerprint differently, and no two checkouts would ever
/// share a server.
fn digest_of(mut lines: Vec<String>) -> String {
    lines.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let mut hasher = Sha256::new();
    for line in &lines {
        hasher.update(line.as_bytes());
    }
    let digest = format!("{:x}", hasher.finalize());
    digest[..12].to_string()
}
