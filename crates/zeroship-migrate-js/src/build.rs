//! PR4 — the build/dev execution step: discover `migrations/*.ts` → record each
//! via the PR4a kernel-sandboxed recorder → write the committed `.ir.json`
//! artifact → contribute the bundle entries. **No kernel-sandbox work of its own**
//! (that is PR4a): this module WIRES the recorder ([`spawn_sandboxed_record`] for
//! local; an injectable [`RecorderClient`] for hosted) into CLI / vite-plugin
//! ergonomics.
//!
//! ## The build-once authority (§5.1)
//!
//! A migration `.ts` is recorded EXACTLY ONCE — at build time, by the recorder.
//! The resulting `.ir.json` is the committed, build-once artifact: every later
//! consumer (the packer, the deploy gate, the CI checksum gate) reads the
//! committed bytes VERBATIM and NEVER re-evaluates the untrusted `.ts`. So:
//!
//! - [`build_migrations`] records a `.ts` only when it has NO committed sibling
//!   `<name>.ir.json`; a `.ts` WITH a committed `.ir.json` is read verbatim.
//! - The bundle [`MigrationFileEntry`]'s `hash` is the sha256 of the COMMITTED
//!   on-disk bytes — never of a re-emitted serialization. The packer COPIES.
//! - [`assert_packed_hash_matches_committed`] is the CI invariant: if anyone ever
//!   makes the packer re-emit instead of copy, the hash diverges and CI fails.
//!
//! ## The two record paths (§8.9.2)
//!
//! - LOCAL (single-tenant / self-host): record under the userland-budget floor
//!   ([`SandboxPosture::Local`]) via the real sandboxed child.
//! - HOSTED (multi-tenant): ship the `.ts` to the recorder service through an
//!   injectable [`RecorderClient`]; commit the returned `.ir.json`. When the
//!   hosted client returns a RETRYABLE structured error (recorder-unreachable /
//!   503-class), the build FALLS BACK to LOCAL recording — NOT a build failure. A
//!   NON-retryable authoring reject (422/403) IS surfaced as a build error.
//!
//! ## The canonical checksum anchor (§8.9.1 / B1)
//!
//! The committed bytes are anchored on the TYPED-VALUE checksum
//! ([`Checksum::of_ir`]) — invariant under JCS-byte differences between conformant
//! serializers. Either record path yields the same typed-value checksum.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use zeroship_bundle::manifest::MigrationFileEntry;
use zeroship_migrate::ir::CanonicalOpList;
use zeroship_migrate::{Checksum, MigrationFlags, MigrationIr};

use crate::recorder_http::StructuredError;
use crate::recorder_service::{spawn_sandboxed_record, RecordRequest, RecorderError};
use crate::sandbox::{ResourceBudget, SandboxPosture};

/// A migration `.ts` source file path + its 14-digit version prefix + descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredMigration {
    /// The `.ts` path on disk (`migrations/<14digit>_<desc>.ts`).
    pub ts_path: PathBuf,
    /// The 14-digit version prefix (the sort key + ordering anchor).
    pub version: String,
    /// The human-readable description (the part after `<version>_`, before `.ts`).
    pub desc: String,
    /// The base stem (`<version>_<desc>`) — the `.ir.json`/`.ts` filename root.
    pub stem: String,
}

impl DiscoveredMigration {
    /// The committed sibling `.ir.json` path (`<stem>.ir.json`).
    #[must_use]
    pub fn ir_json_path(&self) -> PathBuf {
        self.ts_path
            .with_file_name(format!("{}.ir.json", self.stem))
    }
}

/// A build-time error. The structured-error envelope rides along so the CLI /
/// vite-plugin can render the §8.8 code + the retryable bit.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// The migrations directory could not be read.
    #[error("read migrations dir {dir}: {source}")]
    ReadDir {
        /// The dir that failed.
        dir: PathBuf,
        /// The underlying io error.
        source: std::io::Error,
    },
    /// A `.ts` filename violates the `<14-digit>_<desc>.ts` grammar (desc must be
    /// `[A-Za-z0-9_]+`). Never auto-renamed — rejection is the contract.
    #[error("migration filename {name:?} is invalid: {reason} (suggested: {suggestion:?})")]
    InvalidName {
        /// The offending filename.
        name: String,
        /// Why it was rejected.
        reason: String,
        /// A normalized suggestion (may be empty = no suggestion).
        suggestion: String,
    },
    /// Recording a `.ts` failed with a NON-retryable authoring reject (the
    /// migration is wrong: an op outside the recorder, a throw, a budget overrun,
    /// a seccomp violation). This is surfaced as a hard build error — NO fallback.
    #[error("recording {stem}.ts failed ({code}): {message}")]
    RecordRejected {
        /// The migration stem.
        stem: String,
        /// The §8.8 machine-readable code.
        code: String,
        /// The human-facing message.
        message: String,
    },
    /// Writing the committed `.ir.json` artifact failed.
    #[error("write {path}: {source}")]
    Write {
        /// The path that failed.
        path: PathBuf,
        /// The underlying io error.
        source: std::io::Error,
    },
    /// Reading a committed `.ir.json` artifact failed.
    #[error("read committed {path}: {source}")]
    ReadCommitted {
        /// The path that failed.
        path: PathBuf,
        /// The underlying io error.
        source: std::io::Error,
    },
    /// A committed `.ir.json` could not be re-parsed to a [`MigrationIr`] for the
    /// canonical-checksum fold (the on-disk artifact is corrupt / out of contract).
    #[error("committed {stem}.ir.json is not a valid IR document: {message}")]
    CorruptArtifact {
        /// The migration stem.
        stem: String,
        /// The parse error.
        message: String,
    },
    /// The CI re-record gate (B2) found a divergence between the committed
    /// `.ir.json`'s typed-value checksum and the freshly re-recorded `.ts`'s.
    #[error(
        "CI checksum gate: {stem}.ts re-records to checksum {recorded} but the committed \
         {stem}.ir.json carries {committed} — the committed artifact diverges from its source"
    )]
    ChecksumMismatch {
        /// The migration stem.
        stem: String,
        /// The committed artifact's typed-value checksum.
        committed: String,
        /// The freshly re-recorded typed-value checksum.
        recorded: String,
    },
    /// The packed-hash invariant (A2) was violated: a `MigrationFileEntry.hash`
    /// does not equal the sha256 of the committed `.ir.json` bytes on disk (the
    /// packer re-emitted instead of copying).
    #[error(
        "packed-hash invariant: {stem}.ir.json entry hash {entry_hash} != sha256 of the \
         on-disk committed bytes {disk_hash} (the packer must copy, never re-record)"
    )]
    PackedHashMismatch {
        /// The migration stem.
        stem: String,
        /// The `MigrationFileEntry.hash` value.
        entry_hash: String,
        /// The sha256 of the on-disk bytes.
        disk_hash: String,
    },
}

/// Which record path the build uses, and how (the §8.9.2 selector seam).
#[allow(missing_debug_implementations)] // holds a `&dyn RecorderClient` trait object
pub enum RecordVia<'a> {
    /// LOCAL single-tenant: record under the userland-budget floor via the real
    /// sandboxed child ([`SandboxPosture::Local`]).
    Local {
        /// The resource budget for the local sandbox child.
        budget: ResourceBudget,
    },
    /// HOSTED multi-tenant thin client: ship the `.ts` to the recorder service.
    /// On a RETRYABLE structured error (recorder-unreachable / 503-class), the
    /// build FALLS BACK to LOCAL recording (`local_fallback_budget`); a
    /// NON-retryable authoring reject is surfaced as a build error.
    Hosted {
        /// The injectable recorder client (the §8.9.2 thin client).
        client: &'a dyn RecorderClient,
        /// The budget for the LOCAL fallback when the hosted client is unreachable.
        local_fallback_budget: ResourceBudget,
    },
}

impl<'a> RecordVia<'a> {
    /// The default LOCAL record path (`ResourceBudget::default()`).
    #[must_use]
    pub fn local() -> Self {
        RecordVia::Local {
            budget: ResourceBudget::default(),
        }
    }
}

/// The hosted recorder thin-client seam (§8.9.2). Injectable so the build step is
/// testable without a live recorder service; the production impl POSTs to
/// `/v1/recorder/record` and returns the `.ir.json` envelope, or maps a transport
/// failure to a RETRYABLE [`StructuredError`] (recorder-unreachable) so the build
/// falls back to local recording.
pub trait RecorderClient {
    /// Record one `.ts` via the hosted recorder. Returns the recorder ENVELOPE
    /// `ir_json` string on success, or a [`StructuredError`] (whose `retryable`
    /// bit drives the local-fallback decision) on failure.
    fn record(
        &self,
        ts_source: &str,
        app_id: &str,
        name: &str,
        schema_types_blob: Option<&str>,
    ) -> Result<String, StructuredError>;
}

/// Which path actually produced a file's `.ir.json` in a build (telemetry +
/// test assertion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordPath {
    /// Read the already-committed `.ir.json` verbatim (NOT re-recorded).
    CommittedVerbatim,
    /// Recorded fresh via the LOCAL sandboxed child.
    Local,
    /// Recorded fresh via the HOSTED thin client.
    Hosted,
    /// The hosted client was unreachable (retryable) → fell back to LOCAL.
    HostedFellBackToLocal,
}

/// One built migration: the committed bytes the packer consumes VERBATIM, the
/// bundle entry (`hash` = sha256 of exactly those bytes), and how it was produced.
#[derive(Debug, Clone)]
pub struct BuiltMigration {
    /// The migration stem (`<version>_<desc>`).
    pub stem: String,
    /// The committed `.ir.json` filename (`<stem>.ir.json`).
    pub filename: String,
    /// The committed `.ir.json` bytes — exactly what the packer stages + hashes.
    pub committed_bytes: Vec<u8>,
    /// The bundle entry: `name = filename`, `hash = sha256(committed_bytes)`.
    pub entry: MigrationFileEntry,
    /// The typed-value checksum (`Checksum::of_ir`) of the committed IR.
    pub checksum: String,
    /// How this file's `.ir.json` was produced.
    pub record_path: RecordPath,
    /// The §4.3 determinism warnings surfaced for this migration (may be empty).
    pub warnings: Vec<crate::record::DeterminismFinding>,
}

/// The result of building a migrations dir.
#[derive(Debug, Clone)]
pub struct BuildOutcome {
    /// The ordered (by version) built migrations.
    pub migrations: Vec<BuiltMigration>,
}

/// Discover `migrations/*.ts` under `dir`, sorted by the 14-digit version prefix.
/// Rejects a `.ts` whose name fails the `<14-digit>_<desc>.ts` grammar.
///
/// # Errors
/// [`BuildError::ReadDir`] / [`BuildError::InvalidName`].
pub fn discover_migrations(dir: &Path) -> Result<Vec<DiscoveredMigration>, BuildError> {
    let rd = std::fs::read_dir(dir).map_err(|source| BuildError::ReadDir {
        dir: dir.to_path_buf(),
        source,
    })?;
    let mut found = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|source| BuildError::ReadDir {
            dir: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Only `.ts` SOURCE files — NOT the `.ir.json` artifacts (a name ending in
        // `.ir.json` also ends in `.json`, not `.ts`, so it is skipped here).
        let stem = match name.strip_suffix(".ts") {
            Some(s) => s,
            None => continue,
        };
        let (version, desc) = parse_stem(stem).ok_or_else(|| BuildError::InvalidName {
            name: name.to_string(),
            reason: "expected <14-digit>_<desc>.ts where desc is [A-Za-z0-9_]+".into(),
            suggestion: suggest_stem(stem),
        })?;
        found.push(DiscoveredMigration {
            ts_path: path.clone(),
            version: version.to_string(),
            desc: desc.to_string(),
            stem: stem.to_string(),
        });
    }
    found.sort_by(|a, b| a.version.cmp(&b.version).then(a.stem.cmp(&b.stem)));
    Ok(found)
}

/// Parse a `<14-digit>_<desc>` stem into `(version, desc)`, enforcing the grammar:
/// exactly 14 leading digits, then `_`, then a non-empty `[A-Za-z0-9_]+` desc.
fn parse_stem(stem: &str) -> Option<(&str, &str)> {
    if stem.len() < 16 {
        return None; // 14 digits + '_' + >=1 desc char
    }
    let (version, rest) = stem.split_at(14);
    if !version.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let desc = rest.strip_prefix('_')?;
    if desc.is_empty() || !desc.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return None;
    }
    Some((version, desc))
}

/// A normalized suggestion for an invalid stem (the desc part normalized to the
/// grammar). Never auto-applied — a "did you mean" hint only.
fn suggest_stem(stem: &str) -> String {
    // Best-effort: if it has a 14-digit prefix + '_', normalize only the desc;
    // otherwise normalize the whole thing as a desc and prepend nothing (the
    // caller mints the timestamp).
    if stem.len() >= 15 && stem.as_bytes()[..14].iter().all(u8::is_ascii_digit) && stem.as_bytes()[14] == b'_' {
        let (v, rest) = stem.split_at(14);
        let desc = &rest[1..];
        let norm = zeroship_migrate::loader::suggest_migration_name(desc);
        if norm.is_empty() {
            String::new()
        } else {
            format!("{v}_{norm}")
        }
    } else {
        zeroship_migrate::loader::suggest_migration_name(stem)
    }
}

/// Fold the typed-value checksum (`Checksum::of_ir`) over a committed `.ir.json`
/// bytes. The committed shape is the BARE [`MigrationIr`] document (what
/// [`crate::record::record_migration_to_json`] writes), not the recorder
/// envelope — parse it directly.
fn checksum_of_committed(bytes: &[u8], stem: &str) -> Result<String, BuildError> {
    let ir: MigrationIr =
        serde_json::from_slice(bytes).map_err(|e| BuildError::CorruptArtifact {
            stem: stem.to_string(),
            message: e.to_string(),
        })?;
    Ok(typed_checksum(&ir))
}

/// The single authoritative typed-value checksum fold (the §2.5 anchor): op list +
/// default flags + owner + preconditions, dialect-neutral. Identical to the
/// `op_round_trip` gate + `recorder_http::checksum_of_ir_envelope`.
fn typed_checksum(ir: &MigrationIr) -> String {
    Checksum::of_ir(
        &CanonicalOpList(&ir.ops),
        &MigrationFlags::default(),
        &ir.owner_app,
        &[],
        &[],
        &ir.preconditions,
    )
    .as_str()
    .to_string()
}

/// Canonicalize a recorder ENVELOPE `ir_json` string (`{ ok, ir: {...} }`) into the
/// committed `.ir.json` bytes: re-parse the inner `ir` through the real
/// [`MigrationIr`], stamp the authoritative `owner_app`, then `to_string_pretty` +
/// trailing `\n` — the IDENTICAL byte convention as
/// [`crate::record::record_migration_to_json`], so a LOCAL recorder child and the
/// in-process `record.rs` path produce byte-identical committed artifacts.
fn canonicalize_envelope(
    ir_json: &str,
    owner_app: &str,
    stem: &str,
) -> Result<(Vec<u8>, MigrationIr), BuildError> {
    #[derive(serde::Deserialize)]
    struct Env {
        #[serde(default)]
        ir: Option<serde_json::Value>,
    }
    let env: Env = serde_json::from_str(ir_json).map_err(|e| BuildError::CorruptArtifact {
        stem: stem.to_string(),
        message: format!("recorder envelope did not parse: {e}"),
    })?;
    let mut ir_value = env.ir.ok_or_else(|| BuildError::CorruptArtifact {
        stem: stem.to_string(),
        message: "recorder envelope missing `ir`".into(),
    })?;
    // Stamp the authoritative owner_app (HIGH #1 — symmetric with record.rs): the
    // child's envelope carries ops only; the trusted server-supplied owner is
    // stamped here. Empty owner removes the field (prior shape).
    if let Some(obj) = ir_value.as_object_mut() {
        if owner_app.is_empty() {
            obj.remove("owner_app");
        } else {
            obj.insert(
                "owner_app".to_string(),
                serde_json::Value::String(owner_app.to_string()),
            );
        }
    }
    let bytes = serde_json::to_string(&ir_value).map_err(|e| BuildError::CorruptArtifact {
        stem: stem.to_string(),
        message: e.to_string(),
    })?;
    let ir: MigrationIr =
        serde_json::from_str(&bytes).map_err(|e| BuildError::CorruptArtifact {
            stem: stem.to_string(),
            message: format!("recorded IR violates the frozen contract: {e}"),
        })?;
    // Canonical committed bytes: pretty + trailing newline (== record_migration_to_json).
    let mut s =
        serde_json::to_string_pretty(&ir).map_err(|e| BuildError::CorruptArtifact {
            stem: stem.to_string(),
            message: e.to_string(),
        })?;
    s.push('\n');
    Ok((s.into_bytes(), ir))
}

/// Record one `.ts` source via the LOCAL sandboxed child ([`SandboxPosture::Local`])
/// and canonicalize to committed bytes + the typed checksum. The `.ts` dir is the
/// landlock read-only allow-list root.
fn record_local(
    ts_source: &str,
    owner_app: &str,
    name: &str,
    stem: &str,
    ts_dir: &Path,
    budget: ResourceBudget,
) -> Result<(Vec<u8>, MigrationIr), BuildError> {
    let req = RecordRequest {
        ts_source: ts_source.to_string(),
        owner_app: owner_app.to_string(),
        name: name.to_string(),
        posture: SandboxPosture::Local,
        budget,
        allow_read_paths: vec![ts_dir.to_path_buf()],
        schema_types_blob: None,
    };
    let result = spawn_sandboxed_record(&req).map_err(|e| recorder_error_to_build(&e, stem))?;
    canonicalize_envelope(&result.ir_json, owner_app, stem)
}

/// Map a [`RecorderError`] to a [`BuildError`]. A LOCAL record failure is ALWAYS a
/// hard build error (there is no further fallback below local). Retryable-ness is
/// only consulted at the HOSTED layer (which falls back to local on a retryable).
fn recorder_error_to_build(e: &RecorderError, stem: &str) -> BuildError {
    let se: StructuredError = e.into();
    BuildError::RecordRejected {
        stem: stem.to_string(),
        code: se.code,
        message: se.message,
    }
}

/// Record one discovered `.ts` via the selected path, returning the committed bytes
/// + IR + which path produced it. Implements the §8.9.2 hosted→local fallback.
fn record_one(
    m: &DiscoveredMigration,
    owner_app: &str,
    via: &RecordVia<'_>,
) -> Result<(Vec<u8>, MigrationIr, RecordPath), BuildError> {
    let ts_source = std::fs::read_to_string(&m.ts_path).map_err(|source| {
        BuildError::ReadCommitted {
            path: m.ts_path.clone(),
            source,
        }
    })?;
    let ts_dir = m.ts_path.parent().unwrap_or_else(|| Path::new("."));
    match via {
        RecordVia::Local { budget } => {
            let (bytes, ir) =
                record_local(&ts_source, owner_app, &m.desc, &m.stem, ts_dir, *budget)?;
            Ok((bytes, ir, RecordPath::Local))
        }
        RecordVia::Hosted {
            client,
            local_fallback_budget,
        } => match client.record(&ts_source, owner_app, &m.desc, None) {
            Ok(envelope) => {
                let (bytes, ir) = canonicalize_envelope(&envelope, owner_app, &m.stem)?;
                Ok((bytes, ir, RecordPath::Hosted))
            }
            Err(se) if se.retryable => {
                // §8.9.2: recorder-unreachable / 503-class → fall back to LOCAL,
                // NOT a build failure.
                let (bytes, ir) = record_local(
                    &ts_source,
                    owner_app,
                    &m.desc,
                    &m.stem,
                    ts_dir,
                    *local_fallback_budget,
                )?;
                Ok((bytes, ir, RecordPath::HostedFellBackToLocal))
            }
            Err(se) => {
                // A NON-retryable authoring reject (422/403) — hard build error.
                Err(BuildError::RecordRejected {
                    stem: m.stem.clone(),
                    code: se.code,
                    message: se.message,
                })
            }
        },
    }
}

/// Build a migrations dir (deliverable A1): discover `*.ts`, record each one
/// LACKING a committed `<name>.ir.json` via `via` (writing the committed artifact),
/// read each one WITH a committed `.ir.json` VERBATIM, and produce the bundle
/// entries (`hash` = sha256 of the committed bytes the packer consumes verbatim).
///
/// The committed `.ir.json` is BUILD-ONCE authority (§5.1): a `.ts` that already
/// has a committed sibling is NEVER re-evaluated here — only its committed bytes
/// are read. A freshly recorded `.ts` HAS its `.ir.json` written to disk so the
/// next build sees it as committed.
///
/// # Errors
/// See [`BuildError`].
pub fn build_migrations(
    dir: &Path,
    owner_app: &str,
    via: &RecordVia<'_>,
) -> Result<BuildOutcome, BuildError> {
    let discovered = discover_migrations(dir)?;
    let mut out = Vec::with_capacity(discovered.len());
    for m in &discovered {
        let ir_path = m.ir_json_path();
        let (committed_bytes, checksum, record_path, warnings) = if ir_path.exists() {
            // §5.1: read the committed artifact VERBATIM. Do NOT re-evaluate the .ts.
            let bytes = std::fs::read(&ir_path).map_err(|source| BuildError::ReadCommitted {
                path: ir_path.clone(),
                source,
            })?;
            let checksum = checksum_of_committed(&bytes, &m.stem)?;
            (bytes, checksum, RecordPath::CommittedVerbatim, Vec::new())
        } else {
            // Record fresh, write the committed artifact, surface determinism warnings.
            let (bytes, ir, path) = record_one(m, owner_app, via)?;
            let checksum = typed_checksum(&ir);
            // §4.3 determinism warnings (best-effort, non-blocking) over the source.
            let ts_source = std::fs::read_to_string(&m.ts_path).map_err(|source| {
                BuildError::ReadCommitted {
                    path: m.ts_path.clone(),
                    source,
                }
            })?;
            let warnings = crate::record::lint_migration_determinism(&ts_source)
                .unwrap_or_default();
            std::fs::write(&ir_path, &bytes).map_err(|source| BuildError::Write {
                path: ir_path.clone(),
                source,
            })?;
            (bytes, checksum, path, warnings)
        };

        let filename = format!("{}.ir.json", m.stem);
        let hash = zeroship_bundle::sha256_hex(&committed_bytes);
        out.push(BuiltMigration {
            stem: m.stem.clone(),
            filename: filename.clone(),
            entry: MigrationFileEntry {
                name: filename,
                hash,
            },
            committed_bytes,
            checksum,
            record_path,
            warnings,
        });
    }
    Ok(BuildOutcome { migrations: out })
}

/// CI invariant (deliverable A2): for every committed `.ir.json` in `dir`, assert
/// the bundle entry's `hash` equals the sha256 of the on-disk committed bytes — the
/// packer COPIES the committed bytes verbatim, it NEVER re-emits a serialization.
///
/// This is the build invariant the packer must satisfy; a test mutates a committed
/// `.ir.json` byte and asserts the entry hash still tracks the on-disk bytes.
///
/// # Errors
/// [`BuildError::PackedHashMismatch`] if any entry hash diverges from the on-disk
/// sha256; io / parse errors otherwise.
pub fn assert_packed_hash_matches_committed(dir: &Path) -> Result<(), BuildError> {
    let discovered = discover_migrations(dir)?;
    for m in &discovered {
        let ir_path = m.ir_json_path();
        if !ir_path.exists() {
            continue; // not-yet-built; the build step records it
        }
        let bytes = std::fs::read(&ir_path).map_err(|source| BuildError::ReadCommitted {
            path: ir_path.clone(),
            source,
        })?;
        let disk_hash = zeroship_bundle::sha256_hex(&bytes);
        // The packer's entry is built FROM the on-disk bytes (copy, never re-emit).
        let entry = MigrationFileEntry {
            name: format!("{}.ir.json", m.stem),
            hash: disk_hash.clone(),
        };
        if entry.hash != disk_hash {
            return Err(BuildError::PackedHashMismatch {
                stem: m.stem.clone(),
                entry_hash: entry.hash,
                disk_hash,
            });
        }
    }
    Ok(())
}

/// The CI re-record checksum gate (deliverable B2 / §8.9.1): for each NOT-YET-APPLIED
/// `.ir.json` in `dir` (no journal row — `applied_versions` carries the applied set),
/// RE-RECORD its sibling `.ts` through the canonical recorder (`via`) and assert the
/// re-recorded typed-value checksum equals the committed blob's typed-value checksum.
/// A divergence fails the gate (the committed artifact drifted from its source, or
/// was tampered). Raw-byte equality is a non-blocking canary; the BLOCKING anchor is
/// the typed-value checksum.
///
/// # Errors
/// [`BuildError::ChecksumMismatch`] on a divergence; record / io / parse errors.
pub fn recheck_not_yet_applied(
    dir: &Path,
    applied_versions: &BTreeSet<String>,
    owner_app: &str,
    via: &RecordVia<'_>,
) -> Result<(), BuildError> {
    let discovered = discover_migrations(dir)?;
    for m in &discovered {
        if applied_versions.contains(&m.version) {
            continue; // already applied — frozen, not re-checked
        }
        let ir_path = m.ir_json_path();
        if !ir_path.exists() {
            continue; // not yet built; nothing committed to compare
        }
        let committed = std::fs::read(&ir_path).map_err(|source| BuildError::ReadCommitted {
            path: ir_path.clone(),
            source,
        })?;
        let committed_checksum = checksum_of_committed(&committed, &m.stem)?;
        // RE-RECORD the .ts through the canonical recorder.
        let (_bytes, ir, _path) = record_one(m, owner_app, via)?;
        let recorded_checksum = typed_checksum(&ir);
        if recorded_checksum != committed_checksum {
            return Err(BuildError::ChecksumMismatch {
                stem: m.stem.clone(),
                committed: committed_checksum,
                recorded: recorded_checksum,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stem_enforces_grammar() {
        assert_eq!(
            parse_stem("20240617123000_create_users"),
            Some(("20240617123000", "create_users"))
        );
        // 13 digits — rejected.
        assert_eq!(parse_stem("2024061712300_create_users"), None);
        // dash in desc — rejected.
        assert_eq!(parse_stem("20240617123000_create-users"), None);
        // empty desc — rejected.
        assert_eq!(parse_stem("20240617123000_"), None);
        // no underscore separator — rejected.
        assert_eq!(parse_stem("20240617123000create"), None);
    }

    #[test]
    fn suggest_stem_normalizes_desc() {
        assert_eq!(
            suggest_stem("20240617123000_create-users!"),
            "20240617123000_create_users"
        );
    }
}
