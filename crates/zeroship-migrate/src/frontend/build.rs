//! PR4 — the build/dev execution step: discover `migrations/*.ts` → record each
//! via the PR4a kernel-sandboxed recorder → contribute transient in-memory IR bytes.
//! **No kernel-sandbox work of its own** (that is PR4a): this module WIRES the
//! recorder ([`spawn_sandboxed_record`] for local; an injectable [`RecorderClient`]
//! for hosted) into CLI / vite-plugin ergonomics.
//!
//! ## Source authority
//!
//! A creator migration `.ts` is the committed source of truth. Build/gen-types
//! record it fresh in the sandbox and hold the canonical `.ir.json` bytes in memory
//! for the caller; no physical sibling `.ir.json` is read or written.
//!
//! ## The two record paths (§8.9.2)
//!
//! - LOCAL (single-tenant / self-host): record under the userland-budget floor
//!   ([`SandboxPosture::Local`]) via the real sandboxed child.
//! - HOSTED (multi-tenant): ship the `.ts` to the recorder service through an
//!   injectable [`RecorderClient`]; use the returned IR in memory. When the
//!   hosted client returns a RETRYABLE structured error (recorder-unreachable /
//!   503-class), the build FALLS BACK to LOCAL recording — NOT a build failure. A
//!   NON-retryable authoring reject (422/403) IS surfaced as a build error.
//!
//! ## The canonical checksum anchor (§8.9.1 / B1)
//!
//! The transient bytes are anchored on the TYPED-VALUE checksum ([`Checksum::of_ir`])
//! — invariant under JCS-byte differences between conformant serializers. Either
//! record path yields the same typed-value checksum.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::manifest_entry::MigrationFileEntry;
use crate::model::ir::CanonicalOpList;
use crate::model::ir::Op;
use crate::plan::loader;
use crate::{resolve_create_table_policy, Checksum, MigrationFlags, MigrationIr, PolicyProfile};

use super::recorder_protocol::MAX_TS_SOURCE_BYTES;
use super::recorder_http::StructuredError;
use super::recorder_service::{spawn_sandboxed_record, RecordRequest, RecorderError};
use super::sandbox::{ResourceBudget, SandboxPosture};

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
    /// The former physical sibling `.ir.json` path (`<stem>.ir.json`).
    ///
    /// Creator builds no longer read or write this path; it remains as a helper for
    /// legacy tests/tools that need to name the logical IR filename.
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
    /// [`build_one_migration`] was asked to build a specific `<stem>.ts` that is not
    /// among the discovered migrations in its directory (e.g. the file was removed
    /// between the caller naming it and discovery).
    #[error("migration {stem}.ts not found among the discovered migrations")]
    NotFound {
        /// The requested stem.
        stem: String,
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
    /// Reading a migration `.ts` source failed before recording.
    #[error("read source {path}: {source}")]
    ReadSource {
        /// The path that failed.
        path: PathBuf,
        /// The underlying io error.
        source: std::io::Error,
    },
    /// A migration `.ts` source exceeds the recorder source cap.
    #[error(
        "source {path} is {actual} bytes; the recorder accepts at most {limit} bytes"
    )]
    SourceTooLarge {
        /// The source path.
        path: PathBuf,
        /// The accepted byte limit.
        limit: usize,
        /// The observed size.
        actual: u64,
    },
    /// A migration `.ts` source is not valid UTF-8.
    #[error("source {path} is not valid UTF-8: {message}")]
    InvalidSourceUtf8 {
        /// The source path.
        path: PathBuf,
        /// The UTF-8 error.
        message: String,
    },
    /// A recorded IR value could not be re-parsed to a [`MigrationIr`] for the
    /// canonical-checksum fold.
    #[error("recorded {stem}.ir.json is not a valid IR document: {message}")]
    CorruptArtifact {
        /// The migration stem.
        stem: String,
        /// The parse error.
        message: String,
    },
    /// The IR carries a §2.4 hint-domain field (non-default `flags` / non-empty
    /// `depends_on` / `supersedes`) this engine build cannot yet FOLD into the
    /// typed-value checksum. The build refuses to anchor a PARTIAL checksum (the
    /// `IrFlagsOverride`→`MigrationFlags` / `String`→`MigrationId` merges are a
    /// later wave) — mirroring the engine's load gate
    /// ([`crate::hint_domain_uncomputable_field`]) rather than emitting an
    /// artifact whose build-time checksum and the engine's authoritative checksum
    /// disagree on the unfolded fields. Fail-closed: a partial fold both lets a
    /// tampered unfolded field slip past the CI re-record gate AND commits an
    /// artifact the engine's load gate would refuse at deploy.
    #[error(
        "{stem}.ir.json carries a not-yet-foldable {field} domain ({detail}) — the build \
         cannot anchor a partial typed-value checksum (flags/deps/supersedes merge is a later wave)"
    )]
    UnfoldableDomain {
        /// The migration stem.
        stem: String,
        /// The §2.4 hint-domain field that is not yet foldable.
        field: &'static str,
        /// The offending value (debug-rendered).
        detail: String,
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
    /// `ir_json` string on success (`{ ok, ir }`), or a
    /// [`StructuredError`] (whose `retryable` bit drives the local-fallback
    /// decision) on failure.
    fn record(
        &self,
        ts_source: &str,
        app_id: &str,
        name: &str,
        schema_types_blob: Option<&str>,
    ) -> Result<String, StructuredError>;
}

/// Which path actually produced a migration's transient IR in a build (telemetry
/// + test assertion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordPath {
    /// Recorded fresh via the LOCAL sandboxed child.
    Local,
    /// Recorded fresh via the HOSTED thin client.
    Hosted,
    /// The hosted client was unreachable (retryable) → fell back to LOCAL.
    HostedFellBackToLocal,
}

/// One built migration: canonical transient bytes, the logical bundle entry
/// (`hash` = sha256 of exactly those bytes), and how it was produced.
#[derive(Debug, Clone)]
pub struct BuiltMigration {
    /// The migration stem (`<version>_<desc>`).
    pub stem: String,
    /// The logical `.ir.json` filename (`<stem>.ir.json`).
    pub filename: String,
    /// The canonical `.ir.json` bytes, held in memory only.
    pub committed_bytes: Vec<u8>,
    /// The bundle entry: `name = filename`, `hash = sha256(committed_bytes)`.
    pub entry: MigrationFileEntry,
    /// The typed-value checksum (`Checksum::of_ir`) of the transient IR.
    pub checksum: String,
    /// How this file's transient IR was produced.
    pub record_path: RecordPath,
    /// Advisory determinism warnings surfaced from the source regex lint.
    pub warnings: Vec<super::record::DeterminismFinding>,
}

/// One migration recorded for immediate in-memory consumption.
///
/// This is the Platform Model-C front-end: it reuses the exact sandboxed recorder
/// and Rust-side owner-stamp/canonicalization path as [`build_migrations`], and it
/// deliberately does NOT write a committed `.ir.json` artifact. Creator builds use
/// the same transient path: the committed `.ts` is recorded fresh for each
/// build/gen-types invocation.
#[derive(Debug, Clone)]
pub struct TransientRecordedMigration {
    /// The migration stem (`<version>_<desc>`).
    pub stem: String,
    /// The 14-digit filename version prefix.
    pub version: String,
    /// Canonical `.ir.json` bytes (pretty + trailing newline), held in memory only.
    pub ir_json: String,
    /// The frozen-contract-validated typed IR.
    pub ir: MigrationIr,
    /// Which recorder path produced this transient IR.
    pub record_path: RecordPath,
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
        let norm = loader::suggest_migration_name(desc);
        if norm.is_empty() {
            String::new()
        } else {
            format!("{v}_{norm}")
        }
    } else {
        loader::suggest_migration_name(stem)
    }
}

/// The single authoritative typed-value checksum fold (the §2.5 anchor): op list +
/// default flags + owner + preconditions, dialect-neutral. Identical to the
/// `op_round_trip` gate + `recorder_http::checksum_of_ir_envelope`.
///
/// GATES on the engine's [`crate::hint_domain_uncomputable_field`] FIRST
/// (symmetric with `authoritative_ir_checksum`/`recompute_hint_domain_checksum`):
/// PR1 folds only the DEFAULT flags + EMPTY deps/supersedes, so anchoring an IR that
/// carries a non-default flags / deps / supersedes domain would silently fold a
/// PARTIAL checksum — which the CI re-record gate would PASS (both sides fold the
/// same partial domain) while the engine's load gate REFUSES it at deploy. Rather
/// than emit such an undeployable, tamper-permeable artifact, fail closed with
/// [`BuildError::UnfoldableDomain`]. When the `IrFlagsOverride`→`MigrationFlags` /
/// `String`→`MigrationId` merges land, this fold widens to the real
/// flags/deps/supersedes so the build anchor stays identical to
/// `authoritative_ir_checksum`.
fn typed_checksum(ir: &MigrationIr, stem: &str) -> Result<String, BuildError> {
    if let Some((field, detail)) = crate::hint_domain_uncomputable_field(ir) {
        return Err(BuildError::UnfoldableDomain {
            stem: stem.to_string(),
            field,
            detail,
        });
    }
    Ok(Checksum::of_ir(
        &CanonicalOpList(&ir.ops),
        &MigrationFlags::default(),
        &ir.owner_app,
        &[],
        &[],
        &ir.preconditions,
    )
    .as_str()
    .to_string())
}

/// Parse a recorder ENVELOPE `ir_json` string
/// (`{ ok, ir: {...} }`) and stamp the authoritative `owner_app`, leaving the
/// inner IR as raw JSON before typed deserialization rejects invalid scalars.
fn parsed_record_envelope(
    ir_json: &str,
    owner_app: &str,
    stem: &str,
) -> Result<serde_json::Value, BuildError> {
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
    Ok(ir_value)
}

fn canonicalize_ir_value(
    ir_value: serde_json::Value,
    stem: &str,
) -> Result<(Vec<u8>, MigrationIr), BuildError> {
    let bytes = serde_json::to_string(&ir_value).map_err(|e| BuildError::CorruptArtifact {
        stem: stem.to_string(),
        message: e.to_string(),
    })?;
    let ir: MigrationIr =
        serde_json::from_str(&bytes).map_err(|e| BuildError::CorruptArtifact {
            stem: stem.to_string(),
            message: format!("recorded IR violates the frozen contract: {e}"),
        })?;
    // Canonical transient bytes: pretty + trailing newline.
    let mut s =
        serde_json::to_string_pretty(&ir).map_err(|e| BuildError::CorruptArtifact {
            stem: stem.to_string(),
            message: e.to_string(),
        })?;
    s.push('\n');
    Ok((s.into_bytes(), ir))
}

fn resolve_ir_value(
    ir_value: serde_json::Value,
    profile: &PolicyProfile,
    stem: &str,
) -> Result<serde_json::Value, BuildError> {
    let bytes = serde_json::to_string(&ir_value).map_err(|e| BuildError::CorruptArtifact {
        stem: stem.to_string(),
        message: e.to_string(),
    })?;
    let ir: MigrationIr =
        serde_json::from_str(&bytes).map_err(|e| BuildError::CorruptArtifact {
            stem: stem.to_string(),
            message: format!("recorded IR violates the frozen contract: {e}"),
        })?;
    let resolved = resolve_create_table_policy(&ir, profile).map_err(|e| {
        BuildError::CorruptArtifact {
            stem: stem.to_string(),
            message: e.to_string(),
        }
    })?;
    serde_json::to_value(resolved).map_err(|e| BuildError::CorruptArtifact {
        stem: stem.to_string(),
        message: e.to_string(),
    })
}

/// Record one `.ts` source via the LOCAL sandboxed child ([`SandboxPosture::Local`])
/// and return the raw recorder envelope. The `.ts` dir is the landlock read-only
/// allow-list root.
fn record_local_envelope(
    ts_source: &str,
    owner_app: &str,
    name: &str,
    stem: &str,
    ts_dir: &Path,
    budget: ResourceBudget,
) -> Result<String, BuildError> {
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
    Ok(result.ir_json)
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

fn record_envelope_once(
    m: &DiscoveredMigration,
    owner_app: &str,
    via: &RecordVia<'_>,
    ts_source: &str,
    ts_dir: &Path,
) -> Result<(String, RecordPath), BuildError> {
    match via {
        RecordVia::Local { budget } => {
            let envelope = record_local_envelope(
                ts_source,
                owner_app,
                &m.desc,
                &m.stem,
                ts_dir,
                *budget,
            )?;
            Ok((envelope, RecordPath::Local))
        }
        RecordVia::Hosted {
            client,
            local_fallback_budget,
        } => match client.record(
            ts_source,
            owner_app,
            &m.desc,
            None,
        ) {
            Ok(envelope) => Ok((envelope, RecordPath::Hosted)),
            Err(se) if se.retryable => {
                // §8.9.2: recorder-unreachable / 503-class → fall back to LOCAL,
                // NOT a build failure.
                let envelope = record_local_envelope(
                    ts_source,
                    owner_app,
                    &m.desc,
                    &m.stem,
                    ts_dir,
                    *local_fallback_budget,
                )?;
                Ok((envelope, RecordPath::HostedFellBackToLocal))
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

/// Record one discovered `.ts` via the selected path, then return the committed
/// bytes + IR + which path produced it. Implements the §8.9.2 hosted→local
/// fallback for the recording pass.
fn record_one(
    m: &DiscoveredMigration,
    owner_app: &str,
    via: &RecordVia<'_>,
    profile: &PolicyProfile,
) -> Result<(Vec<u8>, MigrationIr, RecordPath, String), BuildError> {
    let ts_source = read_ts_source_bounded(&m.ts_path)?;
    let ts_dir = m.ts_path.parent().unwrap_or_else(|| Path::new("."));

    let (envelope, path) = record_envelope_once(m, owner_app, via, &ts_source, ts_dir)?;
    let parsed = parsed_record_envelope(&envelope, owner_app, &m.stem)?;
    let resolved = resolve_ir_value(parsed, profile, &m.stem)?;
    let (bytes, ir) = canonicalize_ir_value(resolved, &m.stem)?;

    Ok((bytes, ir, path, ts_source))
}

/// Record one discovered `.ts` migration through the canonical recorder and return
/// the canonical IR bytes in memory only.
///
/// Unlike [`build_migrations`], this NEVER checks for or writes a committed
/// `<stem>.ir.json` sibling. It exists for trusted Platform migrations, whose
/// source of truth is `.ts` and whose IR is a transient apply-time artifact.
///
/// # Errors
/// See [`BuildError`].
pub fn record_migration_transient(
    m: &DiscoveredMigration,
    owner_app: &str,
    via: &RecordVia<'_>,
    profile: &PolicyProfile,
) -> Result<TransientRecordedMigration, BuildError> {
    let (bytes, ir, record_path, _ts_source) = record_one(m, owner_app, via, profile)?;
    let ir_json = String::from_utf8(bytes).map_err(|e| BuildError::InvalidSourceUtf8 {
        path: m.ts_path.clone(),
        message: e.utf8_error().to_string(),
    })?;
    Ok(TransientRecordedMigration {
        stem: m.stem.clone(),
        version: m.version.clone(),
        ir_json,
        ir,
        record_path,
    })
}

fn read_ts_source_bounded(path: &Path) -> Result<String, BuildError> {
    let meta = std::fs::metadata(path).map_err(|source| BuildError::ReadSource {
        path: path.to_path_buf(),
        source,
    })?;
    let len = meta.len();
    if len > MAX_TS_SOURCE_BYTES as u64 {
        return Err(BuildError::SourceTooLarge {
            path: path.to_path_buf(),
            limit: MAX_TS_SOURCE_BYTES,
            actual: len,
        });
    }

    let file = std::fs::File::open(path).map_err(|source| BuildError::ReadSource {
        path: path.to_path_buf(),
        source,
    })?;
    let mut limited = file.take((MAX_TS_SOURCE_BYTES as u64) + 1);
    let mut bytes = Vec::with_capacity(len as usize);
    limited
        .read_to_end(&mut bytes)
        .map_err(|source| BuildError::ReadSource {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > MAX_TS_SOURCE_BYTES {
        return Err(BuildError::SourceTooLarge {
            path: path.to_path_buf(),
            limit: MAX_TS_SOURCE_BYTES,
            actual: bytes.len() as u64,
        });
    }
    String::from_utf8(bytes).map_err(|e| BuildError::InvalidSourceUtf8 {
        path: path.to_path_buf(),
        message: e.utf8_error().to_string(),
    })
}

fn warn_on_advisory_determinism_lint(
    host: &impl crate::runtime_host::AuthoringHost,
    stem: &str,
    ts_source: &str,
) -> Vec<super::record::DeterminismFinding> {
    match super::record::lint_migration_determinism(host, ts_source) {
        Ok(findings) => {
            for finding in &findings {
                eprintln!(
                    "build: WARNING: {stem}.ts mentions {}; advisory only: {}",
                    finding.accessor, finding.suggested_fix
                );
            }
            findings
        }
        Err(e) => {
            eprintln!(
                "build: WARNING: determinism advisory lint failed for {stem}.ts: {e}"
            );
            Vec::new()
        }
    }
}

fn warn_on_confined_rejected_raw_sql(stem: &str, ir: &MigrationIr) {
    if ir.ops.iter().any(|op| matches!(op, Op::PgRaw { .. })) {
        eprintln!(
            "build: WARNING: {stem}.ts records pgRaw/raw SQL; Confined deploy validation \
             rejects raw SQL unless an operator explicitly grants the raw-SQL capability"
        );
    }
}

/// Build a migrations dir: discover `*.ts`, record each one via `via`, and produce
/// logical bundle entries over canonical bytes held in memory only.
///
/// # Errors
/// See [`BuildError`].
pub fn build_migrations(
    host: &impl crate::runtime_host::AuthoringHost,
    dir: &Path,
    owner_app: &str,
    via: &RecordVia<'_>,
) -> Result<BuildOutcome, BuildError> {
    let discovered = discover_migrations(dir)?;
    build_discovered(host, &discovered, owner_app, via, &PolicyProfile::confined())
}

/// Build EXACTLY ONE migration `.ts` by path (the CLI `record <file.ts>` surface):
/// discover the file's dir, select only the requested stem, and build that single
/// migration. Unlike [`build_migrations`] (a whole-dir operation), this never
/// records an unrelated in-progress sibling `.ts` — `record half_finished.ts`
/// touches only `half_finished`.
///
/// The selected source is always recorded fresh and surfaced as in-memory canonical
/// IR bytes.
///
/// # Errors
/// [`BuildError::InvalidName`] if `file` is not a `<14-digit>_<desc>.ts` migration,
/// [`BuildError::NotFound`] if no such `.ts` is discovered in its dir; otherwise the
/// same record / io / checksum errors as [`build_migrations`].
pub fn build_one_migration(
    host: &impl crate::runtime_host::AuthoringHost,
    file: &Path,
    owner_app: &str,
    via: &RecordVia<'_>,
) -> Result<BuildOutcome, BuildError> {
    let stem = file
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".ts"))
        .ok_or_else(|| BuildError::InvalidName {
            name: file.display().to_string(),
            reason: "not a .ts migration file".to_string(),
            suggestion: String::new(),
        })?
        .to_string();
    let dir = match file.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let discovered = discover_migrations(&dir)?;
    let only: Vec<DiscoveredMigration> =
        discovered.into_iter().filter(|m| m.stem == stem).collect();
    if only.is_empty() {
        return Err(BuildError::NotFound { stem });
    }
    build_discovered(host, &only, owner_app, via, &PolicyProfile::confined())
}

/// The shared per-migration build loop behind [`build_migrations`] (whole dir) and
/// [`build_one_migration`] (a single discovered file). Each selected `.ts` is
/// recorded fresh through the sandbox and surfaced in memory only.
fn build_discovered(
    host: &impl crate::runtime_host::AuthoringHost,
    discovered: &[DiscoveredMigration],
    owner_app: &str,
    via: &RecordVia<'_>,
    profile: &PolicyProfile,
) -> Result<BuildOutcome, BuildError> {
    let mut out = Vec::with_capacity(discovered.len());
    for m in discovered {
        // Record fresh every time. The regex source lint is advisory only and must
        // never gate the build.
        let (committed_bytes, ir, record_path, ts_source) =
            record_one(m, owner_app, via, profile)?;
        let checksum = typed_checksum(&ir, &m.stem)?;
        let warnings = warn_on_advisory_determinism_lint(host, &m.stem, &ts_source);
        warn_on_confined_rejected_raw_sql(&m.stem, &ir);

        let filename = format!("{}.ir.json", m.stem);
        let hash = crate::manifest_entry::sha256_hex(&committed_bytes);
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
