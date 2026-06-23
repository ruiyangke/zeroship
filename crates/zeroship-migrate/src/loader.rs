//! The **Flyway-style file loader** (design §6) — turn a directory of plain
//! `.sql` files into an ordered list of [`Migration`]s the engine can plan +
//! apply under the **Platform** profile.
//!
//! This is the Phase-2 peer of [`crate::submit`]: where `submit_migration`
//! ingests ONE client-authored script, the loader ingests a whole **directory**
//! of operator-authored platform-schema files. Like `submit`, it builds each
//! [`Migration`] with **server-derived flags** (never author-declared) — the
//! `destructive`/`transactional`/`requires_approval` facets come from the SAME
//! [`flags_for`] classifier `submit` uses, so a `DROP TABLE` file is gated and a
//! non-transactional file routes to the two-phase path automatically.
//!
//! # The grammar (§6.1)
//!
//! A migration directory holds plain `.sql` files; the **filename** encodes
//! everything (there is no Liquibase `--changeset`/`--rollback` header parsing):
//!
//! ```text
//! V<NNNN>__<description>.sql        # versioned "up" migration
//! V<NNNN>__<description>.down.sql   # OPTIONAL reverse for the SAME version
//! R__<description>.sql              # repeatable (re-applies on checksum change)
//! ```
//!
//! - `<NNNN>` is one-or-more digits (numeric, leading zeros OK, gaps OK).
//! - `<description>` is `[A-Za-z0-9_]+` (underscores for spaces).
//! - A filename matching NONE of these is a hard [`LoaderError::UnrecognizedFile`]
//!   (never silently skipped).
//! - A `.down.sql` with no matching `V<NNNN>__` up is a hard
//!   [`LoaderError::OrphanDown`].
//! - Two files sharing a numeric `V<NNNN>` (even with different descriptions) is a
//!   hard [`LoaderError::DuplicateVersion`].
//!
//! # Why the loader does NOT need the operator `OperatorCapability` token
//!
//! Trust is decided at the OPERATOR CALL SITE (§5), never by the loader. The
//! loader's job is purely to produce a `Vec<Migration>`; the CLI (Phase 3) supplies
//! the Platform [`GuardConfig`](crate::guard::GuardConfig)/`ExecutorConfig` to the
//! engine when it plans + applies the loaded set. So the loader never constructs a
//! Platform guard and never holds the token.
//!
//! Flag derivation is **trust-independent by construction**: [`flags_for`] reads
//! only [`GuardReport::classes`](crate::guard::GuardReport)/`.destructive`, and
//! those come from [`classify`](crate::classify::classify), which maps statement
//! KINDS (`DROP TABLE` ⇒ destructive, `CREATE INDEX CONCURRENTLY` ⇒
//! non-transactional) with **no notion of trust**. The loader therefore builds the
//! `GuardReport` straight from `classify` + [`analyze`](crate::analyze::analyze)
//! and feeds it to `flags_for` — never running the deny-list guard, which would
//! (correctly) deny a privileged platform file like `CREATE ROLE` under any profile
//! the loader could itself construct. The actual deny-list enforcement happens
//! later, in the engine, under the operator-supplied Platform guard.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use zeroship_core::typed_id;

use crate::analyze::analyze;
use crate::classify::{classify, ParseError};
use crate::guard::{flags_for, GuardReport};
use crate::migration::{Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId};

/// The fixed `owner_app` sentinel stamped on every loaded platform migration.
///
/// Platform migrations are not owned by any creator app; ownership enforcement
/// (the `submit` path's [`enforce_ownership`](crate::submit)) does not apply to
/// the Platform profile. The sentinel is folded into the [`Checksum`] like any
/// other `owner_app`, so it is part of the tamper-evident unit.
pub const PLATFORM_OWNER_APP: &str = "platform";

/// The 48-bit ceiling for a file version (the high six bytes of the derived
/// UUID). A numeric prefix this large is unreachable in practice (the platform
/// ships ~57 files); the loader rejects it as [`LoaderError::VersionOutOfRange`].
const VERSION_CEILING: u64 = 1u64 << 48;

/// A failure of the file loader.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoaderError {
    /// A directory entry's filename matched NONE of the `V<NNNN>__`,
    /// `V<NNNN>__….down.sql`, or `R__` grammars. Hard error — a stray file is
    /// never silently skipped (§6.1).
    #[error("unrecognized migration filename: '{name}' (expected V<NNNN>__<desc>.sql, V<NNNN>__<desc>.down.sql, or R__<desc>.sql)")]
    UnrecognizedFile {
        /// The offending filename.
        name: String,
    },
    /// A `V<NNNN>__….down.sql` file with no matching `V<NNNN>__….sql` up (§6.1).
    #[error("orphan down migration: '{name}' has no matching V{version}__ up migration")]
    OrphanDown {
        /// The offending `.down.sql` filename.
        name: String,
        /// The numeric version it claims to reverse.
        version: u64,
    },
    /// Two files share the same numeric `V<NNNN>` version (even with different
    /// descriptions) (§6.1).
    #[error("duplicate version V{version}: files '{first}' and '{second}' share the same numeric version")]
    DuplicateVersion {
        /// The duplicated numeric version.
        version: u64,
        /// The first file seen with this version.
        first: String,
        /// The second file seen with this version.
        second: String,
    },
    /// A numeric file version exceeded the 48-bit ordering field (§6.2). Unreachable
    /// for the real port (≤ 0057); rejected loudly rather than silently truncated.
    #[error("file version {version} exceeds the 48-bit ordering ceiling ({ceiling})")]
    VersionOutOfRange {
        /// The out-of-range version.
        version: u64,
        /// The exclusive ceiling (`2^48`).
        ceiling: u64,
    },
    /// A file's SQL body failed to parse during flag derivation. The loader must
    /// classify each `up` to derive `transactional`/`destructive` flags, so an
    /// unparseable body is a hard error (the engine would reject it anyway).
    #[error("failed to classify '{name}': {source}")]
    Parse {
        /// The file whose body failed to parse.
        name: String,
        /// The underlying parse error.
        source: ParseError,
    },
    /// Reading the directory or a file failed.
    #[error("io error for '{path}': {message}")]
    Io {
        /// The path that failed.
        path: String,
        /// The OS error message.
        message: String,
    },
    /// A directory mixes the **Flyway** (`V<NNNN>__…`) and **dbmate**
    /// (`<14-digit>_…`) filename shapes. A migration directory is one format or
    /// the other — auto-detected per file by filename shape — and a mixed
    /// directory is ambiguous (the two formats parse bodies differently:
    /// Flyway pairs `.down.sql` siblings, dbmate splits `-- migrate:up`/`down`
    /// sections), so it is a hard error rather than a silent guess.
    #[error("mixed migration formats in one directory: Flyway file '{flyway}' and dbmate file '{dbmate}' (a directory must be all-Flyway or all-dbmate)")]
    MixedFormats {
        /// An example Flyway-shaped filename seen.
        flyway: String,
        /// An example dbmate-shaped filename seen.
        dbmate: String,
    },
    /// A dbmate file (`<14-digit>_<desc>.sql`) had no `-- migrate:up` section
    /// marker. The `up` body is required (dbmate parity); a file without it is
    /// not a runnable migration.
    #[error("dbmate file '{name}' has no '-- migrate:up' section")]
    MissingUpSection {
        /// The offending dbmate filename.
        name: String,
    },
}

/// Derive a deterministic, ORDER-PRESERVING [`MigrationId`] from a numeric file
/// version (design §6.2, Option A).
///
/// **CANONICAL: this is the ONLY version→id mapping; nothing else mints platform
/// migration ids.**
///
/// # Bit layout (128-bit UUID)
///
/// The numeric `version` occupies the **HIGH 48 bits** — the same six bytes
/// `UUIDv7` uses for its big-endian millisecond timestamp
/// ([`MigrationId::timestamp_ms`](crate::migration::MigrationId::timestamp_ms))
/// — and the **low 80 bits are ZERO**. So a larger version ⇒ a larger 128-bit
/// integer ⇒ a lexicographically larger 22-char base62 string ⇒ a larger
/// [`MigrationId`] under its derived `Ord`.
///
/// # The load-bearing invariant
///
/// "String `Ord` == numeric version order" holds **only because
/// [`typed_id::uuid_to_base62`] is a FIXED-22-char encoding of the 128-bit UUID
/// as a single big-endian integer over an ASCENDING alphabet**
/// (`BASE62 = "0123456789ABC…xyz"`, sorted so lexicographic order matches numeric
/// order — `crates/core/src/typed_id.rs:10-12,27-38`). Because the width is fixed
/// (22) and the alphabet ascends, `a < b` as 128-bit integers ⇒
/// `base62(a) < base62(b)` lexicographically, which is exactly the
/// [`MigrationId`] `Ord` (a `String` newtype with a derived `Ord`,
/// `migration.rs:35`). **If a future refactor swapped in a variable-width or
/// non-ascending base62 encoder, this ordering would silently break.** The
/// round-trip test [`tests::version_derivation_is_order_preserving`] pins the
/// invariant so that change fails loudly.
///
/// # Determinism + range
///
/// Same `version` ⇒ same id every load (required for re-run identity +
/// checksum-drift detection). `version >= 2^48` is **unreachable** for the real
/// port (≤ 0057, ceiling ~2.8e14); callers that parse a prefix already bound it,
/// and a `debug_assert!` documents the contract.
///
/// # Panics
///
/// Never in practice: the derived 16-byte UUID always base62-encodes to a valid
/// `mig_<22-char>` id that [`MigrationId::parse`] accepts.
#[must_use]
pub fn migration_id_for_version(version: u64) -> MigrationId {
    debug_assert!(
        version < VERSION_CEILING,
        "file version {version} exceeds the 48-bit ordering field"
    );
    let mut bytes = [0u8; 16];
    // The high 48 bits = the low six bytes of the big-endian u64 version.
    bytes[0..6].copy_from_slice(&version.to_be_bytes()[2..8]);
    // The low 80 bits stay zero.
    let uuid = uuid::Uuid::from_bytes(bytes);
    MigrationId::parse(&format!("mig_{}", typed_id::uuid_to_base62(&uuid)))
        .expect("derived id is a valid mig_ typed id")
}

/// Derive a deterministic, STABLE [`MigrationId`] for a REPEATABLE (`R__<desc>`)
/// migration from its description name.
///
/// **CANONICAL: this is the ONLY id mapping for repeatables.** A repeatable carries
/// no version, but its identity MUST be stable across loads: the re-run oracle
/// ([`apply_repeatables`](crate::executor)) keys its re-run-on-change decision on
/// the journal `version` column (`SELECT … WHERE kind='repeatable'`,
/// `journal::latest_completed_checksums`). A freshly-random id per load (the bug
/// this fixes) would make an UNCHANGED `R__` file re-apply on EVERY deploy
/// (defeating Flyway `R__` / Liquibase `runOnChange`) and accrue a phantom journal
/// row each load. Deriving the id deterministically from the name makes the same
/// `R__foo.sql` map to the same id every load, so the version-keyed oracle skips it
/// when unchanged and re-applies only on a checksum change.
///
/// # Layout — collision-free vs versioned ids
///
/// [`migration_id_for_version`] puts the numeric version in the HIGH 48 bits and
/// leaves the LOW 80 bits ZERO. This derivation sets the high 48 bits to a fixed
/// `0xFFFF_FFFF_FFFF` **marker** (no real file version reaches `2^48`) and fills the
/// low 80 bits with the first 10 bytes of `SHA-256(name)`. So a repeatable id can
/// NEVER equal a versioned id (the high 48 bits differ — marker vs a small version)
/// and two distinct names collide only on an 80-bit SHA-256 prefix collision
/// (negligible). Deterministic (same name ⇒ same id); no OS/random/time. Ordering
/// among repeatables is by description + `depends_on` in their own apply phase
/// (they are partitioned out of the versioned set), so this id's `Ord` position
/// (which, with the `0xFF…` marker, sorts after every versioned id — fittingly) is
/// irrelevant to apply order.
#[must_use]
pub fn repeatable_id_for_name(name: &str) -> MigrationId {
    let digest = Sha256::digest(name.as_bytes());
    let mut bytes = [0u8; 16];
    // High 48 bits = a fixed marker no real version reaches ⇒ never collides with a
    // versioned id (whose high 48 bits hold a small numeric version).
    bytes[0..6].copy_from_slice(&[0xFFu8; 6]);
    // Low 80 bits = the name hash ⇒ stable per name, distinct across names.
    bytes[6..16].copy_from_slice(&digest[0..10]);
    let uuid = uuid::Uuid::from_bytes(bytes);
    MigrationId::parse(&format!("mig_{}", typed_id::uuid_to_base62(&uuid)))
        .expect("derived repeatable id is a valid mig_ typed id")
}

/// One parsed filename, before bodies are read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedName {
    /// `V<NNNN>__<desc>.sql` — a versioned up migration.
    VersionedUp { version: u64, description: String },
    /// `V<NNNN>__<desc>.down.sql` — the reverse for a version.
    VersionedDown { version: u64 },
    /// `R__<desc>.sql` — a repeatable migration.
    Repeatable { description: String },
}

/// Parse a single migration filename against the §6.1 grammar. Returns `None`
/// for a filename that matches no pattern (the caller turns that into a hard
/// [`LoaderError::UnrecognizedFile`]).
fn parse_filename(name: &str) -> Option<ParsedName> {
    // Repeatable: R__<desc>.sql  (check before V so an "R" prefix is unambiguous).
    if let Some(rest) = name.strip_prefix("R__") {
        let desc = rest.strip_suffix(".sql")?;
        if is_valid_description(desc) {
            return Some(ParsedName::Repeatable {
                description: desc.to_string(),
            });
        }
        return None;
    }

    // Versioned (up or down): V<NNNN>__<desc>(.down)?.sql
    let rest = name.strip_prefix('V')?;
    // Split the leading run of digits off the version.
    let digit_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    if digit_end == 0 {
        return None; // "V__foo" — no digits.
    }
    let (digits, after) = rest.split_at(digit_end);
    let version: u64 = digits.parse().ok()?;
    let after = after.strip_prefix("__")?;

    // Down: <desc>.down.sql ; Up: <desc>.sql.
    if let Some(desc) = after.strip_suffix(".down.sql") {
        if is_valid_description(desc) {
            return Some(ParsedName::VersionedDown { version });
        }
        return None;
    }
    if let Some(desc) = after.strip_suffix(".sql") {
        if is_valid_description(desc) {
            return Some(ParsedName::VersionedUp {
                version,
                description: desc.to_string(),
            });
        }
        return None;
    }
    None
}

/// A `<description>` is `[A-Za-z0-9_]+` (non-empty).
fn is_valid_description(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `true` iff `name` is a valid migration description the loader's filename parser
/// would later accept: `[A-Za-z0-9_]+` (non-empty). This is the SINGLE source of
/// truth — the CLI's `new` pre-check calls THIS so a scaffolded name can never be
/// written and then rejected at load time (no false accept/reject drift).
#[must_use]
pub fn is_valid_migration_name(name: &str) -> bool {
    is_valid_description(name)
}

/// Normalize an arbitrary user-supplied `new <name>` into a valid migration
/// description, for the CLI's "did you mean …?" suggestion when a name is rejected.
/// Every run of disallowed characters collapses to a single `_`, leading/trailing
/// `_` are trimmed; an all-invalid name yields the empty string (the caller treats
/// that as "no suggestion"). PURE; does NO I/O — the CLI decides whether to print
/// it. This NEVER auto-renames: rejection is the contract; this is only a hint.
#[must_use]
pub fn suggest_migration_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_us = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
            prev_us = ch == '_';
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    out.trim_matches('_').to_string()
}

// =============================================================================
// dbmate-native format (Track A, Phase A2)
// =============================================================================
//
// dbmate's on-disk format is ONE timestamped file per migration carrying both
// directions via section markers:
//
// ```text
// db/migrations/20240617123000_create_users.sql
// -- migrate:up
// CREATE TABLE users (id bigserial primary key, ...);
//
// -- migrate:down
// DROP TABLE users;
// ```
//
// - Filename grammar: `<version>_<description>.sql` where `<version>` is a
//   14-digit `YYYYMMDDHHMMSS` timestamp. (dbmate accepts other digit widths in
//   the wild; zeroship STANDARDIZES on the 14-digit stamp the `new` helper mints,
//   so the loader recognizes exactly 14 digits as the dbmate shape — every other
//   leading-digit width is not a dbmate file. This keeps auto-detect crisp and the
//   `V<NNNN>__` Flyway shape unambiguous.)
// - `-- migrate:up` (required) then optional `-- migrate:down`. A file with no
//   `-- migrate:up` is a hard `LoaderError::MissingUpSection`.
// - Per-section option `-- migrate:up transaction:false` → the non-transactional
//   path (reconciled with the classify-derived flag in `flags_for_file_opts`).
//
// The 14-digit timestamp (~2.0e13 for 2026) is < `VERSION_CEILING` (2^48 ≈
// 2.8e14), so `migration_id_for_version(timestamp)` is REUSED for the version id
// and orders timestamps correctly. A timestamp ≥ `VERSION_CEILING` is rejected as
// `VersionOutOfRange`, identically to the Flyway path.

/// The exact digit-width of a dbmate `<version>` timestamp the loader recognizes
/// (`YYYYMMDDHHMMSS`). Standardized to keep auto-detect unambiguous.
const DBMATE_VERSION_DIGITS: usize = 14;

/// A parsed dbmate filename: `<14-digit version>_<description>.sql`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DbmateName {
    /// The numeric timestamp version (`YYYYMMDDHHMMSS`).
    version: u64,
    /// The description (`[A-Za-z0-9_]+`).
    description: String,
}

/// The two section bodies of a dbmate file, plus the parsed `up` option.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DbmateSections {
    /// The `-- migrate:up` body (required; trimmed).
    up: String,
    /// The `-- migrate:down` body, or `None` if the file has no down section
    /// (trimmed).
    down: Option<String>,
    /// `true` iff the `-- migrate:up` marker carried `transaction:false`.
    up_non_transactional: bool,
}

/// Parse a dbmate filename `<14-digit version>_<description>.sql`. Returns `None`
/// for any name that is not the dbmate shape (the caller decides whether that is a
/// hard error or a different format).
fn parse_dbmate_filename(name: &str) -> Option<DbmateName> {
    let stem = name.strip_suffix(".sql")?;
    // The version is the LEADING run of ascii digits, ended by the first `_`.
    let digit_end = stem.find(|c: char| !c.is_ascii_digit()).unwrap_or(stem.len());
    // Standardize on EXACTLY 14 digits (YYYYMMDDHHMMSS) so the dbmate shape is
    // unambiguous vs. Flyway's `V<NNNN>__`.
    if digit_end != DBMATE_VERSION_DIGITS {
        return None;
    }
    let (digits, rest) = stem.split_at(digit_end);
    let description = rest.strip_prefix('_')?;
    if !is_valid_description(description) {
        return None;
    }
    let version: u64 = digits.parse().ok()?;
    Some(DbmateName {
        version,
        description: description.to_string(),
    })
}

/// True if a filename has the dbmate shape (`<14-digit>_<desc>.sql`).
fn is_dbmate_filename(name: &str) -> bool {
    parse_dbmate_filename(name).is_some()
}

/// True if a filename has the Flyway shape (`V<NNNN>__…` / `R__…`).
fn is_flyway_filename(name: &str) -> bool {
    parse_filename(name).is_some()
}

/// Split a dbmate file body on its `-- migrate:up` / `-- migrate:down` section
/// markers. `up` is REQUIRED; `down` is optional. Section bodies are trimmed. The
/// `-- migrate:up` marker may carry a `transaction:false` option.
///
/// # Errors
/// [`LoaderError::MissingUpSection`] if there is no `-- migrate:up` marker.
fn parse_dbmate_sections(name: &str, body: &str) -> Result<DbmateSections, LoaderError> {
    // Marker lines: a line whose TRIMMED form begins with `-- migrate:`. We split
    // the file into (marker, body) regions. `up`/`down` are the only directions
    // dbmate defines; an unknown direction is ignored (its body folds into the
    // previous section), but `up` must be present.
    #[derive(PartialEq)]
    enum Sec {
        None,
        Up,
        Down,
        Other,
    }
    let mut current = Sec::None;
    let mut up: Option<String> = None;
    let mut down: Option<String> = None;
    let mut up_non_transactional = false;
    let mut up_buf = String::new();
    let mut down_buf = String::new();

    for line in body.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("-- migrate:") {
            // The direction is the first whitespace-delimited token; the rest are
            // options (`transaction:false`).
            let mut parts = rest.split_whitespace();
            let direction = parts.next().unwrap_or("");
            match direction {
                "up" => {
                    current = Sec::Up;
                    up = Some(String::new());
                    up_non_transactional = parts.any(|opt| opt == "transaction:false");
                }
                "down" => {
                    current = Sec::Down;
                    down = Some(String::new());
                }
                _ => current = Sec::Other,
            }
            continue;
        }
        match current {
            Sec::Up => {
                up_buf.push_str(line);
                up_buf.push('\n');
            }
            Sec::Down => {
                down_buf.push_str(line);
                down_buf.push('\n');
            }
            Sec::None | Sec::Other => {}
        }
    }

    if up.is_none() {
        return Err(LoaderError::MissingUpSection {
            name: name.to_string(),
        });
    }

    Ok(DbmateSections {
        up: up_buf.trim().to_string(),
        // `down` present only if a `-- migrate:down` marker appeared. An empty
        // down body trims to `Some("")`-equivalent → we keep `None` for an
        // absent marker and the trimmed string otherwise.
        down: down.map(|_| down_buf.trim().to_string()),
        up_non_transactional,
    })
}

/// The skeleton `new`-command output: the dbmate filename + the empty section
/// scaffold (`-- migrate:up\n\n\n-- migrate:down\n`).
///
/// PURE + deterministic: the caller (the A3 CLI `new` command) supplies the
/// 14-digit timestamp string (formatted from `SystemTime`), so this function does
/// NO I/O and NO clock read — it is fully unit-testable with a fixed stamp.
///
/// `timestamp` must be the 14-digit `YYYYMMDDHHMMSS` stamp and `name` the
/// description (`[A-Za-z0-9_]+` by convention; the function does not validate it —
/// the CLI normalizes the user's input before calling). Returns
/// `(filename, contents)`.
#[must_use]
pub fn new_dbmate_migration(timestamp: &str, name: &str) -> (String, String) {
    let filename = format!("{timestamp}_{name}.sql");
    let contents = "-- migrate:up\n\n\n-- migrate:down\n".to_string();
    (filename, contents)
}

/// Build the [`MigrationFlags`] for a file's `up` SQL, **trust-independently**.
///
/// Mirrors [`submit_migration`](crate::submit::submit_migration)'s server-side
/// flag derivation — `destructive`/`transactional`/`requires_approval` come from
/// [`flags_for`], layered with the authoring-only `repeatable` facet from the
/// `R__` filename. But where `submit` runs the full deny-list guard to obtain the
/// [`GuardReport`], the loader builds the report directly from [`classify`] +
/// [`analyze`]: the deny-list would reject privileged platform SQL (`CREATE ROLE`)
/// under any profile the loader could construct, and trust is the operator's call
/// (§5), not the loader's. `flags_for` reads ONLY statement kinds, so this derives
/// the identical flags the engine's later Platform guard would.
fn flags_for_file(name: &str, up: &str, repeatable: bool) -> Result<MigrationFlags, LoaderError> {
    flags_for_file_opts(name, up, repeatable, false)
}

/// As [`flags_for_file`], but honoring a dbmate `-- migrate:up transaction:false`
/// option (`force_non_transactional`).
///
/// Flags are still **classify-derived** (a `CREATE INDEX CONCURRENTLY` auto-routes
/// to the non-transactional path with no option needed). The dbmate option is a
/// one-way override: if the file declares `transaction:false`, the migration runs
/// on the non-transactional two-phase path regardless of what `classify` derived;
/// otherwise the classify-derived `transactional` flag stands (the option never
/// *forces* a non-transactional migration back into a transaction).
fn flags_for_file_opts(
    name: &str,
    up: &str,
    repeatable: bool,
    force_non_transactional: bool,
) -> Result<MigrationFlags, LoaderError> {
    let classes = classify(up).map_err(|source| LoaderError::Parse {
        name: name.to_string(),
        source,
    })?;
    let destructive = classes.iter().any(|c| c.destructive);
    let report = GuardReport {
        classes,
        destructive,
        advisories: analyze(up),
    };
    let derived = flags_for(&report);
    Ok(MigrationFlags {
        repeatable,
        // dbmate `transaction:false` honored as a one-way override → always the
        // non-transactional path; otherwise keep the classify-derived flag.
        transactional: derived.transactional && !force_non_transactional,
        ..derived
    })
}

/// Load a directory of Flyway-style `.sql` files into an ordered list of
/// [`Migration`]s (design §6).
///
/// Files are parsed against the §6.1 grammar, paired with their `.down.sql`
/// siblings, classified for server-derived flags, checksummed, and returned
/// **ordered by numeric version ascending, with repeatables last** (the existing
/// repeatable semantics — repeatables run after all versioned migrations,
/// `migration.rs:159-160`). The derived [`MigrationId`]s preserve numeric version
/// order under their `Ord`, so the engine's executor ordering sees the loaded set
/// already in apply order with no reordering surprise.
///
/// Each migration is built mirroring [`crate::submit`]:
/// - `version` = [`migration_id_for_version`] of the parsed numeric prefix (a
///   deterministic [`MigrationId`]) — repeatables, which carry no version, get a
///   freshly-minted [`MigrationId::generate`] (they are identified by checksum +
///   name, not version, per the repeatable semantics);
/// - `name` = the `<description>`;
/// - `up` = the file body; `down` = the sibling `.down.sql` body or `None`;
/// - `flags` = [`flags_for`]-derived (auto), with `repeatable` from the `R__`
///   filename;
/// - `owner_app` = [`PLATFORM_OWNER_APP`]; `depends_on`/`supersedes`/
///   `preconditions` = `[]` (file order IS the dependency);
/// - `checksum` = [`Checksum::of`] over the whole apply-relevant unit.
///
/// # Errors
///
/// [`LoaderError`] on an unrecognized filename, an orphan `.down.sql`, a duplicate
/// `V<NNNN>`, an out-of-range version, an unparseable body, or an I/O fault.
pub fn load_dir(dir: impl AsRef<Path>) -> Result<Vec<crate::plan::AppliedPlan>, LoaderError> {
    // PR0 (`op.*` DSL §5.2): `load_dir` returns `Vec<AppliedPlan>`. A `.sql` file
    // lowers to a **single-step plan** via the `AppliedPlan::single_step()` facade
    // (one `Ddl` step), preserving the order-by-version contract. When the IR
    // `.ir.json` branch lands (PR1), a richer artifact lowers to a multi-step plan
    // here; the `.sql` path stays a one-step plan.
    let migrations = load_dir_migrations(dir)?;
    Ok(migrations
        .into_iter()
        .map(crate::plan::AppliedPlan::single_step)
        .collect())
}

/// The `Vec<Migration>` form of [`load_dir`] — the directory read + version-order
/// grammar pass that produces the raw [`Migration`]s, before they are wrapped
/// into single-step [`AppliedPlan`](crate::plan::AppliedPlan)s. Retained as the
/// internal core (and for any caller that still needs the flat `Migration` set,
/// e.g. the integrity-manifest fold), so the wrap into plans is a thin shell.
pub fn load_dir_migrations(dir: impl AsRef<Path>) -> Result<Vec<Migration>, LoaderError> {
    let dir = dir.as_ref();
    let mut entries: Vec<PathBuf> = Vec::new();
    let read = std::fs::read_dir(dir).map_err(|e| LoaderError::Io {
        path: dir.display().to_string(),
        message: e.to_string(),
    })?;
    for entry in read {
        let entry = entry.map_err(|e| LoaderError::Io {
            path: dir.display().to_string(),
            message: e.to_string(),
        })?;
        let path = entry.path();
        // Skip subdirectories silently (a `.sql` is always a file); a non-`.sql`
        // FILE still falls through to the grammar check and errors.
        if path.is_file() {
            entries.push(path);
        }
    }
    // Sort by filename so iteration + duplicate diagnostics are deterministic.
    entries.sort();
    load_files(&entries)
}

/// The filenames split by kind, version-validated and duplicate-/orphan-checked.
struct Classified {
    /// Versioned ups, sorted ascending by numeric version: `(version, desc, path)`.
    ups: Vec<(u64, String, PathBuf)>,
    /// Versioned `.down.sql` siblings: `(version, path)`.
    downs: Vec<(u64, PathBuf)>,
    /// Repeatables: `(description, path)`.
    repeatables: Vec<(String, PathBuf)>,
}

/// First pass: parse every filename against the §6.1 grammar and run the hard
/// structural checks (unrecognized name, out-of-range version, duplicate
/// `V<NNNN>`, orphan `.down.sql`). Returns the kind-split, version-sorted file set.
fn classify_filenames(paths: &[PathBuf]) -> Result<Classified, LoaderError> {
    let mut ups: Vec<(u64, String, PathBuf)> = Vec::new();
    let mut downs: Vec<(u64, PathBuf)> = Vec::new();
    let mut repeatables: Vec<(String, PathBuf)> = Vec::new();

    for path in paths {
        let name = file_name_of(path);
        match parse_filename(&name) {
            Some(ParsedName::VersionedUp {
                version,
                description,
            }) => {
                if version >= VERSION_CEILING {
                    return Err(LoaderError::VersionOutOfRange {
                        version,
                        ceiling: VERSION_CEILING,
                    });
                }
                ups.push((version, description, path.clone()));
            }
            Some(ParsedName::VersionedDown { version }) => downs.push((version, path.clone())),
            Some(ParsedName::Repeatable { description }) => {
                repeatables.push((description, path.clone()));
            }
            None => return Err(LoaderError::UnrecognizedFile { name }),
        }
    }

    // Duplicate version check (same numeric V<NNNN>, even with a different desc).
    ups.sort_by_key(|(v, _, _)| *v);
    for window in ups.windows(2) {
        if window[0].0 == window[1].0 {
            return Err(LoaderError::DuplicateVersion {
                version: window[0].0,
                first: file_name_of(&window[0].2),
                second: file_name_of(&window[1].2),
            });
        }
    }

    // Pair each down with its up; an orphan down is a HARD error.
    for (version, path) in &downs {
        if !ups.iter().any(|(v, _, _)| v == version) {
            return Err(LoaderError::OrphanDown {
                name: file_name_of(path),
                version: *version,
            });
        }
    }

    Ok(Classified {
        ups,
        downs,
        repeatables,
    })
}

/// The migration-file format a directory is authored in (auto-detected by
/// filename shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirFormat {
    /// `V<NNNN>__<desc>.sql` (+ `.down.sql`, `R__`) — the platform port.
    Flyway,
    /// `<14-digit>_<desc>.sql` with `-- migrate:up`/`down` sections — the public
    /// dbmate-like CLI.
    Dbmate,
}

/// Auto-detect a directory's format by classifying each filename's SHAPE
/// (`V<NNNN>__…` ⇒ Flyway; `<14-digit>_…` ⇒ dbmate). An empty directory defaults
/// to Flyway (the choice is moot — there are no files to parse). A directory that
/// mixes both shapes is ambiguous and a hard [`LoaderError::MixedFormats`]; a file
/// matching NEITHER shape is left for the chosen format's parser to reject as an
/// [`LoaderError::UnrecognizedFile`] (so an all-dbmate dir with one stray
/// `garbage.sql` still errors precisely, not as a format-mix).
fn detect_format(paths: &[PathBuf]) -> Result<DirFormat, LoaderError> {
    let mut flyway: Option<String> = None;
    let mut dbmate: Option<String> = None;
    for path in paths {
        let name = file_name_of(path);
        // dbmate is checked first: a 14-digit `<ts>_<desc>.sql` can never match the
        // `V…`/`R__` Flyway grammar (no `V`/`R` prefix), so the two predicates are
        // disjoint and order is immaterial — but recording both lets us name a
        // concrete example file of each shape in the MixedFormats error.
        if is_dbmate_filename(&name) {
            dbmate.get_or_insert(name);
        } else if is_flyway_filename(&name) {
            flyway.get_or_insert(name);
        }
        // A name matching neither shape is NOT a format signal — defer to the
        // chosen parser, which raises UnrecognizedFile.
    }
    match (flyway, dbmate) {
        (Some(f), Some(d)) => Err(LoaderError::MixedFormats {
            flyway: f,
            dbmate: d,
        }),
        (_, Some(_)) => Ok(DirFormat::Dbmate),
        // No dbmate files seen ⇒ Flyway (incl. the empty-dir case: moot).
        (_, None) => Ok(DirFormat::Flyway),
    }
}

/// Load an explicit, ordered list of file paths (the directory-independent core
/// of [`load_dir`], factored out so tests can drive it with a fixed file set).
/// Routes to the Flyway or dbmate builder by the auto-detected directory format.
fn load_files(paths: &[PathBuf]) -> Result<Vec<Migration>, LoaderError> {
    match detect_format(paths)? {
        DirFormat::Flyway => load_flyway_files(paths),
        DirFormat::Dbmate => load_dbmate_files(paths),
    }
}

/// Build the `Migration` set for a **dbmate** directory: one timestamped file per
/// migration, both directions via `-- migrate:up`/`down` sections. Ordered by the
/// numeric timestamp version ascending. dbmate has no repeatable concept, so every
/// file is a versioned migration. Mirrors [`load_flyway_files`]'s `Migration`
/// construction (server-derived flags, `PLATFORM_OWNER_APP`, checksum over the
/// whole unit).
fn load_dbmate_files(paths: &[PathBuf]) -> Result<Vec<Migration>, LoaderError> {
    // First pass: parse filenames, range-check + duplicate-check versions.
    let mut parsed: Vec<(u64, String, PathBuf)> = Vec::new();
    for path in paths {
        let name = file_name_of(path);
        let Some(DbmateName {
            version,
            description,
        }) = parse_dbmate_filename(&name)
        else {
            // In a dbmate-detected dir, any non-dbmate-shaped FILE is a stray.
            return Err(LoaderError::UnrecognizedFile { name });
        };
        if version >= VERSION_CEILING {
            return Err(LoaderError::VersionOutOfRange {
                version,
                ceiling: VERSION_CEILING,
            });
        }
        parsed.push((version, description, path.clone()));
    }

    parsed.sort_by_key(|(v, _, _)| *v);
    for window in parsed.windows(2) {
        if window[0].0 == window[1].0 {
            return Err(LoaderError::DuplicateVersion {
                version: window[0].0,
                first: file_name_of(&window[0].2),
                second: file_name_of(&window[1].2),
            });
        }
    }

    // Second pass: read bodies, split sections, build each Migration.
    let mut migrations: Vec<Migration> = Vec::with_capacity(parsed.len());
    for (version, description, path) in &parsed {
        let name = file_name_of(path);
        let raw = read_body(path)?;
        let sections = parse_dbmate_sections(&name, &raw)?;
        let flags = flags_for_file_opts(
            &name,
            &sections.up,
            false,
            sections.up_non_transactional,
        )?;
        let checksum = Checksum::of(&ChecksumInput {
            up: &sections.up,
            down: sections.down.as_deref(),
            flags: &flags,
            owner_app: PLATFORM_OWNER_APP,
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        migrations.push(Migration {
            version: migration_id_for_version(*version),
            name: description.clone(),
            up: sections.up,
            down: sections.down,
            checksum,
            flags,
            owner_app: PLATFORM_OWNER_APP.to_string(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
        });
    }

    Ok(migrations)
}

/// Build the `Migration` set for a **Flyway** directory (the original
/// `load_files` body — the platform port path, unchanged).
fn load_flyway_files(paths: &[PathBuf]) -> Result<Vec<Migration>, LoaderError> {
    let Classified {
        ups,
        downs,
        mut repeatables,
    } = classify_filenames(paths)?;

    // Second pass: build a Migration per versioned up, in ascending version order.
    let mut migrations: Vec<Migration> = Vec::with_capacity(ups.len() + repeatables.len());
    for (version, description, up_path) in &ups {
        let up = read_body(up_path)?;
        let down = match downs.iter().find(|(v, _)| v == version) {
            Some((_, down_path)) => Some(read_body(down_path)?),
            None => None,
        };
        let name = file_name_of(up_path);
        let flags = flags_for_file(&name, &up, false)?;
        let checksum = Checksum::of(&ChecksumInput {
            up: &up,
            down: down.as_deref(),
            flags: &flags,
            owner_app: PLATFORM_OWNER_APP,
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        migrations.push(Migration {
            version: migration_id_for_version(*version),
            name: description.clone(),
            up,
            down,
            checksum,
            flags,
            owner_app: PLATFORM_OWNER_APP.to_string(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
        });
    }

    // Repeatables run AFTER all versioned migrations (migration.rs:159-160),
    // ordered by description for a deterministic load.
    repeatables.sort_by(|a, b| a.0.cmp(&b.0));
    for (description, path) in &repeatables {
        let up = read_body(path)?;
        let name = file_name_of(path);
        let flags = flags_for_file(&name, &up, true)?;
        let checksum = Checksum::of(&ChecksumInput {
            up: &up,
            down: None,
            flags: &flags,
            owner_app: PLATFORM_OWNER_APP,
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        migrations.push(Migration {
            // A repeatable carries no version: its identity is its stable
            // name/checksum (migration.rs:154-157). Derive a DETERMINISTIC id from
            // the name so the SAME R__ file maps to the SAME id every load — the
            // re-run oracle (apply_repeatables) keys on the journal `version`, so a
            // random-per-load id would re-apply an unchanged repeatable every deploy
            // and accrue a phantom journal row. Ordering among repeatables is by
            // description/`depends_on` in their own phase, so this id's Ord is moot.
            version: repeatable_id_for_name(description),
            name: description.clone(),
            up,
            down: None,
            checksum,
            flags,
            owner_app: PLATFORM_OWNER_APP.to_string(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
        });
    }

    Ok(migrations)
}

/// The filename of a path as a `String` (best-effort; empty if non-UTF-8).
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

/// Read a file body as a `String`.
fn read_body(path: &Path) -> Result<String, LoaderError> {
    std::fs::read_to_string(path).map_err(|e| LoaderError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- §6.1 filename grammar -----

    #[test]
    fn parses_versioned_up_down_and_repeatable() {
        assert_eq!(
            parse_filename("V0001__extensions_schemas.sql"),
            Some(ParsedName::VersionedUp {
                version: 1,
                description: "extensions_schemas".to_string()
            })
        );
        assert_eq!(
            parse_filename("V42__roles.down.sql"),
            Some(ParsedName::VersionedDown { version: 42 })
        );
        assert_eq!(
            parse_filename("R__current_views.sql"),
            Some(ParsedName::Repeatable {
                description: "current_views".to_string()
            })
        );
        // Leading zeros and large numbers parse to their numeric value.
        assert_eq!(
            parse_filename("V0057__final.sql"),
            Some(ParsedName::VersionedUp {
                version: 57,
                description: "final".to_string()
            })
        );
        assert_eq!(
            parse_filename("V10000__big.sql"),
            Some(ParsedName::VersionedUp {
                version: 10_000,
                description: "big".to_string()
            })
        );
    }

    #[test]
    fn rejects_unrecognized_filenames() {
        // No double underscore.
        assert_eq!(parse_filename("V1_nope.sql"), None);
        // No digits.
        assert_eq!(parse_filename("V__nope.sql"), None);
        // Bad description char.
        assert_eq!(parse_filename("V1__has-dash.sql"), None);
        // Wrong extension.
        assert_eq!(parse_filename("V1__x.txt"), None);
        // Empty description.
        assert_eq!(parse_filename("V1__.sql"), None);
        // Bare junk.
        assert_eq!(parse_filename("README.md"), None);
        // R with no double underscore.
        assert_eq!(parse_filename("R_single.sql"), None);
    }

    #[test]
    fn load_files_unrecognized_filename_is_hard_error() {
        let dir = tempdir();
        write_file(&dir, "V1__ok.sql", "CREATE TABLE t ();");
        write_file(&dir, "garbage.sql", "SELECT 1;");
        let err = load_dir_migrations(&dir).unwrap_err();
        assert!(
            matches!(err, LoaderError::UnrecognizedFile { ref name } if name == "garbage.sql"),
            "got {err:?}"
        );
    }

    #[test]
    fn orphan_down_is_hard_error() {
        let dir = tempdir();
        write_file(&dir, "V1__ok.sql", "CREATE TABLE t ();");
        write_file(&dir, "V2__orphan.down.sql", "DROP TABLE t;");
        let err = load_dir_migrations(&dir).unwrap_err();
        assert!(
            matches!(err, LoaderError::OrphanDown { version: 2, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn duplicate_version_is_hard_error() {
        let dir = tempdir();
        write_file(&dir, "V1__one.sql", "CREATE TABLE a ();");
        write_file(&dir, "V1__two.sql", "CREATE TABLE b ();");
        let err = load_dir_migrations(&dir).unwrap_err();
        assert!(
            matches!(err, LoaderError::DuplicateVersion { version: 1, .. }),
            "got {err:?}"
        );
    }

    // ----- §6.2 canonical version→id derivation -----

    #[test]
    fn version_derivation_is_deterministic_and_parses() {
        // Determinism: same version => same id every call.
        assert_eq!(migration_id_for_version(1), migration_id_for_version(1));
        assert_eq!(
            migration_id_for_version(10_000),
            migration_id_for_version(10_000)
        );
        // Round-trips through MigrationId::parse (and therefore matches the
        // ^[a-z]{3}_[A-Za-z0-9]{22}$ typed-id shape).
        for v in [0u64, 1, 2, 10, 57, 100, 10_000, (1u64 << 48) - 1] {
            let id = migration_id_for_version(v);
            assert!(id.as_str().starts_with("mig_"), "v={v} got {}", id.as_str());
            assert_eq!(id.as_str().len(), 26, "mig_ + 22 base62 = 26: v={v}");
            MigrationId::parse(id.as_str())
                .unwrap_or_else(|e| panic!("derived id for v={v} must parse: {e:?}"));
        }
    }

    #[test]
    fn version_derivation_is_order_preserving() {
        // The numeric-vs-string trap: lexically "2" > "10", but the DERIVED ids
        // must order numerically. Prove V1 < V2 < V10 < V100 < V10000 under the
        // MigrationId Ord (a String newtype with a derived Ord).
        let v1 = migration_id_for_version(1);
        let v2 = migration_id_for_version(2);
        let v10 = migration_id_for_version(10);
        let v100 = migration_id_for_version(100);
        let v10000 = migration_id_for_version(10_000);
        assert!(v1 < v2, "V1 ({}) < V2 ({})", v1.as_str(), v2.as_str());
        assert!(v2 < v10, "V2 ({}) < V10 ({})", v2.as_str(), v10.as_str());
        assert!(v10 < v100, "V10 < V100");
        assert!(v100 < v10000, "V100 < V10000");
        // And the raw string Ord agrees (the load-bearing fixed-width invariant).
        assert!(v2.as_str() < v10.as_str(), "string Ord must be numeric, not lexical-on-the-number");
    }

    #[test]
    fn distinct_versions_yield_distinct_ids() {
        let ids: Vec<_> = [0u64, 1, 2, 10, 42, 57, 100, 10_000]
            .iter()
            .map(|v| migration_id_for_version(*v))
            .collect();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "ids {i} and {j} collided");
            }
        }
    }

    #[test]
    fn repeatable_id_is_stable_distinct_and_clear_of_versioned() {
        // Deterministic: same name ⇒ same id.
        assert_eq!(
            repeatable_id_for_name("a_view"),
            repeatable_id_for_name("a_view")
        );
        // Distinct names ⇒ distinct ids.
        assert_ne!(
            repeatable_id_for_name("a_view"),
            repeatable_id_for_name("b_view")
        );
        // Valid typed id.
        let id = repeatable_id_for_name("a_view");
        assert!(id.as_str().starts_with("mig_"));
        MigrationId::parse(id.as_str()).expect("repeatable id parses");
        // NEVER collides with a versioned id (high-48 marker vs a small version).
        for v in [0u64, 1, 2, 10, 57, 100, 10_000, (1u64 << 48) - 1] {
            assert_ne!(
                repeatable_id_for_name("a_view"),
                migration_id_for_version(v),
                "repeatable id must not collide with versioned id v={v}"
            );
        }
    }

    #[test]
    fn loaded_repeatable_id_is_deterministic_across_loads() {
        let dir = tempdir();
        write_file(&dir, "R__a_view.sql", "CREATE OR REPLACE VIEW v AS SELECT 1;");
        let first = load_dir_migrations(&dir).unwrap();
        let second = load_dir_migrations(&dir).unwrap();
        // The same R__ file must get the SAME version id on a fresh reload — this is
        // what makes the version-keyed re-run oracle skip an unchanged repeatable.
        assert_eq!(first[0].version, second[0].version, "repeatable id must be stable across loads");
        assert_eq!(first[0].version, repeatable_id_for_name("a_view"));
    }

    // ----- the loaded Vec<Migration> -----

    #[test]
    fn loads_dir_into_ordered_migrations() {
        let dir = tempdir();
        // Intentionally out of filesystem-lexical order to prove numeric ordering.
        write_file(&dir, "V10__add_index.sql", "CREATE INDEX i ON t (a);");
        write_file(&dir, "V2__add_col.sql", "ALTER TABLE t ADD COLUMN a int;");
        write_file(&dir, "V1__create.sql", "CREATE TABLE t (id int);");
        write_file(&dir, "V1__create.down.sql", "DROP TABLE t;");
        write_file(&dir, "R__a_view.sql", "CREATE OR REPLACE VIEW v AS SELECT 1;");

        let migs = load_dir_migrations(&dir).expect("load must succeed");
        // 3 versioned + 1 repeatable.
        assert_eq!(migs.len(), 4);

        // Versioned migrations ordered by numeric version: V1, V2, V10.
        assert_eq!(migs[0].name, "create");
        assert_eq!(migs[1].name, "add_col");
        assert_eq!(migs[2].name, "add_index");
        // Repeatable runs last.
        assert_eq!(migs[3].name, "a_view");
        assert!(migs[3].flags.repeatable, "R__ file must set repeatable");
        assert!(!migs[0].flags.repeatable);

        // Derived versions preserve order.
        assert!(migs[0].version < migs[1].version);
        assert!(migs[1].version < migs[2].version);

        // up/down bodies wired correctly.
        assert_eq!(migs[0].up, "CREATE TABLE t (id int);");
        assert_eq!(migs[0].down.as_deref(), Some("DROP TABLE t;"));
        assert_eq!(migs[1].down, None, "V2 has no .down sibling");

        // owner_app sentinel on every migration.
        for m in &migs {
            assert_eq!(m.owner_app, PLATFORM_OWNER_APP);
            assert!(m.depends_on.is_empty());
            assert_eq!(m.checksum, Checksum::of(&ChecksumInput::from_migration(m)));
        }
    }

    #[test]
    fn flags_auto_derived_destructive_for_drop() {
        let dir = tempdir();
        write_file(&dir, "V1__drop_it.sql", "DROP TABLE legacy;");
        write_file(&dir, "V2__additive.sql", "CREATE TABLE t (id int);");
        let migs = load_dir_migrations(&dir).unwrap();
        let drop = migs.iter().find(|m| m.name == "drop_it").unwrap();
        let add = migs.iter().find(|m| m.name == "additive").unwrap();
        // DROP TABLE => destructive => requires_approval (server-derived, like submit).
        assert!(drop.flags.destructive, "DROP TABLE must be destructive");
        assert!(drop.flags.requires_approval, "destructive must require approval");
        // Additive create is neither.
        assert!(!add.flags.destructive);
        assert!(!add.flags.requires_approval);
    }

    #[test]
    fn flags_auto_derived_non_transactional_for_concurrently() {
        let dir = tempdir();
        write_file(
            &dir,
            "V1__concurrent_index.sql",
            "CREATE INDEX CONCURRENTLY i ON t (a);",
        );
        let migs = load_dir_migrations(&dir).unwrap();
        assert!(
            !migs[0].flags.transactional,
            "CREATE INDEX CONCURRENTLY must auto-route to the non-transactional path"
        );
    }

    #[test]
    fn empty_dir_loads_to_empty_vec() {
        let dir = tempdir();
        assert!(load_dir_migrations(&dir).unwrap().is_empty());
    }

    // ----- dbmate-native format (Phase A2) -----

    #[test]
    fn parse_dbmate_filename_accepts_14_digit_timestamp() {
        assert_eq!(
            parse_dbmate_filename("20240617123000_create_users.sql"),
            Some(DbmateName {
                version: 20_240_617_123_000,
                description: "create_users".to_string()
            })
        );
        // A different valid stamp orders correctly numerically (see ordering test).
        assert_eq!(
            parse_dbmate_filename("20240101000000_init.sql"),
            Some(DbmateName {
                version: 20_240_101_000_000,
                description: "init".to_string()
            })
        );
    }

    #[test]
    fn parse_dbmate_filename_rejects_non_dbmate_shapes() {
        // Flyway shape is NOT dbmate.
        assert_eq!(parse_dbmate_filename("V0001__x.sql"), None);
        // Too few digits (not a 14-digit stamp).
        assert_eq!(parse_dbmate_filename("123_x.sql"), None);
        // Too many digits.
        assert_eq!(parse_dbmate_filename("202406171230001_x.sql"), None);
        // No underscore after the version.
        assert_eq!(parse_dbmate_filename("20240617123000.sql"), None);
        // Empty description.
        assert_eq!(parse_dbmate_filename("20240617123000_.sql"), None);
        // Bad description char.
        assert_eq!(parse_dbmate_filename("20240617123000_has-dash.sql"), None);
        // Wrong extension.
        assert_eq!(parse_dbmate_filename("20240617123000_x.txt"), None);
    }

    #[test]
    fn parse_dbmate_sections_extracts_up_and_down() {
        let body = "-- migrate:up\nCREATE TABLE users (id bigserial primary key);\n\n-- migrate:down\nDROP TABLE users;\n";
        let secs = parse_dbmate_sections("20240617123000_create_users.sql", body).unwrap();
        assert_eq!(secs.up, "CREATE TABLE users (id bigserial primary key);");
        assert_eq!(secs.down.as_deref(), Some("DROP TABLE users;"));
        assert!(!secs.up_non_transactional);
    }

    #[test]
    fn parse_dbmate_sections_up_only_yields_no_down() {
        let body = "-- migrate:up\nCREATE TABLE t (id int);\n";
        let secs = parse_dbmate_sections("20240101000000_t.sql", body).unwrap();
        assert_eq!(secs.up, "CREATE TABLE t (id int);");
        assert_eq!(secs.down, None, "no -- migrate:down marker => down is None");
    }

    #[test]
    fn parse_dbmate_sections_missing_up_is_hard_error() {
        // Only a down section, no up.
        let body = "-- migrate:down\nDROP TABLE t;\n";
        let err = parse_dbmate_sections("20240101000000_t.sql", body).unwrap_err();
        assert!(
            matches!(err, LoaderError::MissingUpSection { ref name } if name == "20240101000000_t.sql"),
            "got {err:?}"
        );
        // A file with no markers at all is equally missing its up.
        let err2 = parse_dbmate_sections("20240101000000_t.sql", "CREATE TABLE t ();").unwrap_err();
        assert!(matches!(err2, LoaderError::MissingUpSection { .. }), "got {err2:?}");
    }

    #[test]
    fn parse_dbmate_sections_honors_transaction_false() {
        let body = "-- migrate:up transaction:false\nCREATE INDEX i ON t (a);\n\n-- migrate:down\nDROP INDEX i;\n";
        let secs = parse_dbmate_sections("20240101000000_idx.sql", body).unwrap();
        assert!(
            secs.up_non_transactional,
            "the transaction:false option on -- migrate:up must be parsed"
        );
        assert_eq!(secs.up, "CREATE INDEX i ON t (a);");
    }

    #[test]
    fn load_dir_dbmate_orders_by_timestamp_version() {
        let dir = tempdir();
        // Out of filesystem-lexical / insertion order to prove version ordering.
        write_file(&dir, "20240617123000_add_index.sql", "-- migrate:up\nCREATE INDEX i ON t (a);\n-- migrate:down\nDROP INDEX i;\n");
        write_file(&dir, "20240101000000_create.sql", "-- migrate:up\nCREATE TABLE t (id int);\n-- migrate:down\nDROP TABLE t;\n");
        write_file(&dir, "20240301000000_add_col.sql", "-- migrate:up\nALTER TABLE t ADD COLUMN a int;\n");

        let migs = load_dir_migrations(&dir).expect("dbmate dir loads");
        assert_eq!(migs.len(), 3);
        // Ordered by numeric timestamp: 20240101 < 20240301 < 20240617.
        assert_eq!(migs[0].name, "create");
        assert_eq!(migs[1].name, "add_col");
        assert_eq!(migs[2].name, "add_index");
        assert!(migs[0].version < migs[1].version);
        assert!(migs[1].version < migs[2].version);
        // Section bodies wired through.
        assert_eq!(migs[0].up, "CREATE TABLE t (id int);");
        assert_eq!(migs[0].down.as_deref(), Some("DROP TABLE t;"));
        assert_eq!(migs[1].down, None, "up-only file has no down");
        // Version id reuses the canonical version→id mapping.
        assert_eq!(migs[0].version, migration_id_for_version(20_240_101_000_000));
        // owner_app + checksum invariants identical to the Flyway path.
        for m in &migs {
            assert_eq!(m.owner_app, PLATFORM_OWNER_APP);
            assert!(m.depends_on.is_empty());
            assert_eq!(m.checksum, Checksum::of(&ChecksumInput::from_migration(m)));
            assert!(!m.flags.repeatable, "dbmate has no repeatable concept");
        }
    }

    #[test]
    fn load_dir_dbmate_drop_table_is_destructive() {
        let dir = tempdir();
        write_file(&dir, "20240101000000_drop_it.sql", "-- migrate:up\nDROP TABLE legacy;\n");
        let migs = load_dir_migrations(&dir).unwrap();
        assert!(migs[0].flags.destructive, "DROP TABLE up must be destructive");
        assert!(migs[0].flags.requires_approval, "destructive => requires approval");
    }

    #[test]
    fn load_dir_dbmate_transaction_false_routes_non_transactional() {
        let dir = tempdir();
        // A plain (transactional-by-classify) statement, but the file declares
        // transaction:false — the override must force the non-transactional path.
        write_file(
            &dir,
            "20240101000000_seed.sql",
            "-- migrate:up transaction:false\nINSERT INTO t (id) VALUES (1);\n",
        );
        let migs = load_dir_migrations(&dir).unwrap();
        assert!(
            !migs[0].flags.transactional,
            "transaction:false must honor the non-transactional path even when classify would allow a txn"
        );
    }

    #[test]
    fn load_dir_dbmate_missing_up_is_hard_error() {
        let dir = tempdir();
        write_file(&dir, "20240101000000_bad.sql", "-- migrate:down\nDROP TABLE t;\n");
        let err = load_dir_migrations(&dir).unwrap_err();
        assert!(matches!(err, LoaderError::MissingUpSection { .. }), "got {err:?}");
    }

    #[test]
    fn load_dir_dbmate_far_future_stamp_in_range_loads() {
        // Every 14-digit YYYYMMDDHHMMSS stamp (max ≈ 1.0e14) is below
        // VERSION_CEILING (2^48 ≈ 2.81e14), so a real dbmate stamp is always in
        // range and loads cleanly via the canonical version→id mapping — even a
        // far-future stamp (no VersionOutOfRange). The dbmate builder still carries
        // the same `>= VERSION_CEILING` guard as the Flyway path as defense in
        // depth, identical to `migration_id_for_version`'s debug-assert contract.
        let dir = tempdir();
        write_file(&dir, "20991231235959_far_future.sql", "-- migrate:up\nCREATE TABLE t (id int);\n");
        let migs = load_dir_migrations(&dir).unwrap();
        assert_eq!(migs.len(), 1);
        assert_eq!(migs[0].version, migration_id_for_version(20_991_231_235_959));
    }

    #[test]
    fn load_dir_dbmate_duplicate_version_is_hard_error() {
        let dir = tempdir();
        write_file(&dir, "20240101000000_one.sql", "-- migrate:up\nCREATE TABLE a ();\n");
        write_file(&dir, "20240101000000_two.sql", "-- migrate:up\nCREATE TABLE b ();\n");
        let err = load_dir_migrations(&dir).unwrap_err();
        assert!(
            matches!(err, LoaderError::DuplicateVersion { version: 20_240_101_000_000, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn load_dir_dbmate_stray_file_is_unrecognized() {
        let dir = tempdir();
        write_file(&dir, "20240101000000_ok.sql", "-- migrate:up\nCREATE TABLE t ();\n");
        write_file(&dir, "20240102000000_also.sql", "-- migrate:up\nCREATE TABLE u ();\n");
        // A bare non-dbmate, non-Flyway file in a dbmate-detected dir.
        write_file(&dir, "notes.txt", "hello");
        let err = load_dir_migrations(&dir).unwrap_err();
        assert!(
            matches!(err, LoaderError::UnrecognizedFile { ref name } if name == "notes.txt"),
            "got {err:?}"
        );
    }

    // ----- format auto-detection + coexistence -----

    #[test]
    fn detect_format_distinguishes_flyway_and_dbmate() {
        assert_eq!(
            detect_format(&[PathBuf::from("V0001__x.sql")]).unwrap(),
            DirFormat::Flyway
        );
        assert_eq!(
            detect_format(&[PathBuf::from("R__view.sql")]).unwrap(),
            DirFormat::Flyway
        );
        assert_eq!(
            detect_format(&[PathBuf::from("20240101000000_x.sql")]).unwrap(),
            DirFormat::Dbmate
        );
        // Empty dir defaults to Flyway (moot — no files to parse).
        assert_eq!(detect_format(&[]).unwrap(), DirFormat::Flyway);
        // A stray non-conforming file is not a format signal (defers to parser).
        assert_eq!(
            detect_format(&[PathBuf::from("20240101000000_x.sql"), PathBuf::from("README.md")]).unwrap(),
            DirFormat::Dbmate
        );
    }

    #[test]
    fn mixed_format_directory_is_hard_error() {
        let dir = tempdir();
        write_file(&dir, "V0001__flyway.sql", "CREATE TABLE t ();");
        write_file(&dir, "20240101000000_dbmate.sql", "-- migrate:up\nCREATE TABLE u ();\n");
        let err = load_dir_migrations(&dir).unwrap_err();
        assert!(matches!(err, LoaderError::MixedFormats { .. }), "got {err:?}");
    }

    #[test]
    fn auto_detect_does_not_misclassify_either_shape() {
        // A V0001 file is Flyway, never dbmate (it has no 14-digit prefix).
        assert!(!is_dbmate_filename("V0001__extensions.sql"));
        assert!(is_flyway_filename("V0001__extensions.sql"));
        // A 14-digit timestamped file is dbmate, never Flyway (no V/R prefix).
        assert!(is_dbmate_filename("20240617123000_create_users.sql"));
        assert!(!is_flyway_filename("20240617123000_create_users.sql"));
    }

    // ----- the `new` helper (Phase A2; A3 CLI consumes it) -----

    #[test]
    fn new_dbmate_migration_is_deterministic() {
        let (filename, contents) = new_dbmate_migration("20240617123000", "create_users");
        assert_eq!(filename, "20240617123000_create_users.sql");
        assert_eq!(contents, "-- migrate:up\n\n\n-- migrate:down\n");
        // Deterministic: same inputs => identical output.
        assert_eq!(
            new_dbmate_migration("20240617123000", "create_users"),
            (filename, contents)
        );
    }

    #[test]
    fn is_valid_migration_name_matches_loader_grammar() {
        // The public pre-check is EXACTLY the loader's `is_valid_description`.
        assert!(is_valid_migration_name("create_users"));
        assert!(is_valid_migration_name("v2_users_42"));
        assert!(is_valid_migration_name("A"));
        // Rejected: spaces, dashes, punctuation, empty.
        assert!(!is_valid_migration_name("bad name"));
        assert!(!is_valid_migration_name("with-dash"));
        assert!(!is_valid_migration_name("bang!"));
        assert!(!is_valid_migration_name(""));
        // The pre-check agrees with the filename parser it guards: any name it
        // accepts produces a filename `parse_dbmate_filename` accepts, and any it
        // rejects produces one the parser rejects (no false accept/reject).
        for name in ["ok_name", "bad name", "x-y", ""] {
            let fname = format!("20240101000000_{name}.sql");
            assert_eq!(
                is_valid_migration_name(name),
                parse_dbmate_filename(&fname).is_some(),
                "pre-check must agree with the loader for {name:?}"
            );
        }
    }

    #[test]
    fn suggest_migration_name_normalizes_invalid_input() {
        assert_eq!(suggest_migration_name("bad name!"), "bad_name");
        assert_eq!(suggest_migration_name("with-dash"), "with_dash");
        assert_eq!(suggest_migration_name("  lead  trail  "), "lead_trail");
        assert_eq!(suggest_migration_name("a--b__c"), "a_b__c");
        assert_eq!(suggest_migration_name("already_ok"), "already_ok");
        // An all-invalid name yields the empty string (caller: "no suggestion").
        assert_eq!(suggest_migration_name("!!!"), "");
        // The suggestion is itself always a valid name (or empty).
        for raw in ["bad name!", "with-dash", "a.b.c", "café"] {
            let s = suggest_migration_name(raw);
            assert!(
                s.is_empty() || is_valid_migration_name(&s),
                "suggestion {s:?} for {raw:?} must be valid or empty"
            );
        }
    }

    #[test]
    fn new_dbmate_migration_output_round_trips_through_the_loader() {
        // The skeleton the `new` helper emits must itself be a loadable dbmate file
        // (its filename parses + its body has a -- migrate:up section). The empty up
        // body classifies to an empty statement set (no error).
        let (filename, contents) = new_dbmate_migration("20240101000000", "scaffold");
        assert!(parse_dbmate_filename(&filename).is_some(), "emitted filename parses");
        let secs = parse_dbmate_sections(&filename, &contents).expect("emitted body has an up section");
        assert_eq!(secs.up, "", "the scaffold up body is empty");
        assert_eq!(secs.down.as_deref(), Some(""), "the scaffold down marker is present but empty");
    }

    #[test]
    fn flyway_db_migrations_still_load_as_56_ordered() {
        // Coexistence regression: the EXISTING platform port (`db/migrations/`,
        // the `V<NNNN>__…` Flyway set) must auto-detect as Flyway and load
        // IDENTICALLY after the dbmate path was added — 56 versioned migrations,
        // strictly ascending. This is the pure (no-PG) peer of the PG-gated
        // `tests/platform_port_pg.rs`, guaranteeing the dbmate work did not perturb
        // the platform loader.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../db/migrations");
        if !dir.is_dir() {
            // The repo always ships db/migrations; skip only if a stripped checkout
            // somehow lacks it (never in CI).
            eprintln!("skipping: {} not present", dir.display());
            return;
        }
        let migs = load_dir_migrations(&dir).expect("the platform Flyway port still loads");
        assert_eq!(migs.len(), 56, "all 56 ported V<NNNN>__ files load (0045 is a gap)");
        // Auto-detect chose Flyway, not dbmate: none is repeatable here and the
        // versions are strictly ascending (the Flyway numeric ordering).
        for w in migs.windows(2) {
            assert!(w[0].version < w[1].version, "Flyway versions strictly ascending");
        }
        for m in &migs {
            assert_eq!(m.owner_app, PLATFORM_OWNER_APP);
        }
    }

    // ----- test fixtures -----

    /// A throwaway temp directory under the OS temp dir, unique per call.
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("zsmig_loader_{pid}_{nanos}_{n}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_file(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("write fixture file");
    }
}
