//! Sealed-record persistence for `SandboxAuth` (`docs/proposals/sandbox-preview-urls.md` § II.0).
//!
//! The controller mints per-sandbox Ed25519 keypairs at
//! `Backend::create` time. Without persistence, a controller restart
//! wipes those keys — every still-alive sandbox is unreachable
//! forever (the agent in the VM has the matching verifying key and
//! 401s every signed request from the new controller). This module
//! seals the keys (plus the metadata needed to re-bind the sandbox
//! to its agent on restart) into per-sandbox files on disk:
//!
//! ```text
//! $SANDBOX_PERSIST_DIR/sealed-records/<hex(sha256(sandbox_id))[..32]>.sealed
//! ```
//!
//! ## Wire format
//!
//! ```text
//! 24-byte XChaCha20 nonce || ciphertext || 16-byte Poly1305 tag
//! ```
//!
//! AEAD: XChaCha20-Poly1305. The associated-data input is the file's
//! stem (the 32-char SHA-256-derived filename, ASCII), so a record
//! moved between files won't decrypt — the binding is to the file's
//! location. The plaintext is `serde_json::to_vec(&SealedAuth)`.
//!
//! ## Threat model
//!
//! - **Disk read of a sealed file**: confidentiality + integrity by
//!   AEAD; an attacker with file read can't recover the signing key
//!   or substitute one. See § IX.a "Operational secrets" of the
//!   design doc.
//! - **AEAD key compromise**: full loss. Mitigated by host-side file
//!   permissions (`SANDBOX_AEAD_KEY_PATH` mode 0400, owner = the
//!   controller's runtime UID), and by the keys being short-lived
//!   per sandbox (≤ `SANDBOX_MAX_LIFETIME_SECS`, default 8 h).
//! - **Path traversal via attacker-controlled sandbox_id**: defended
//!   by hashing the sandbox_id and using ONLY the hash digest as the
//!   filename. A malicious or buggy `sandbox_id="../../etc/passwd"`
//!   still produces `<hex>.sealed` inside `sealed-records/`. See the
//!   `seal_path_is_inside_dir_for_evil_id` regression test.
//!
//! ## Operational notes (§ IX.a)
//!
//! - Key source: file mount only — `SANDBOX_AEAD_KEY_PATH`. The
//!   `SANDBOX_AEAD_KEY` env-var sourcing is **not supported** here;
//!   per file-mounted AEAD key rule of the design doc, env-var sourcing leaks via
//!   `/proc/<pid>/environ`. The constructor refuses an env-only
//!   path.
//! - Feature-flag: the call sites only invoke this module when
//!   `SANDBOX_PERSIST_AUTH=1`. With the flag off, the controller
//!   behaves exactly as before this module landed (no on-disk state).
//! - Filename: a 16-byte truncation of SHA-256 (32 hex chars). The
//!   probability of two distinct sandbox-ids hashing to the same
//!   filename is 2^-64 per pair; far smaller than UUIDv7's birthday
//!   bound on creation. Collisions would surface as decrypt-failures
//!   (the AEAD's associated-data covers the filename) — survivable.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::backend::SandboxAuth;
use zeroship_sandbox_agent::sig;

/// Wire-format version. Bumped on any schema change to the JSON
/// envelope (`SealedAuth`). On unseal, a record with a higher
/// version than this binary understands is treated as corrupt and
/// surfaced via the operator's "unknown record" runbook — never
/// silently downgraded.
///
/// **v3 (pg-as-system-of-record).** Schema shrink.
/// Pg is the system of record for non-secret state from day 1, so
/// sealed records hold secrets only — `signing_key_bytes` and
/// `preview_secrets`. The legacy fields (`user_id`, `project_id`,
/// `backend`, `vm_index`, `agent_url`, `pubkey_fp`, `created_at_secs`,
/// `preview_audit`) are gone from the v3 struct; pg holds them. A new
/// `boot_id: Option<u64>` lets the boot-time reconciler tell apart
/// orphans from this controller's lifetime vs orphans from a previous
/// boot. v2 records still load — the deserializer ignores the dropped
/// fields — and the v3 binary gets the missing data from pg at
/// restore time.
///
/// **v1 → v2 (legacy).** v2 added `preview_secrets` + `preview_audit`.
/// v3 keeps `preview_secrets` (it's secret) and drops `preview_audit`
/// (now a pg row in `sandbox.shares`). The v1 reader stayed in v2 for
/// back-compat; this keeps that compat one more step (v1 + v2
/// records both deserialize through the v3 struct).
pub const SEAL_VERSION: u8 = 3;

/// Length of the truncated SHA-256 digest used as the filename. 16
/// bytes → 32 hex chars. See module doc for the collision argument.
const FILENAME_HEX_LEN: usize = 32;

/// AEAD nonce length. XChaCha20 mandates 24 bytes; ChaCha20 is 12.
/// We pick the X-flavor so a fresh nonce per file can come straight
/// from `/dev/urandom` without having to track a per-key counter.
const NONCE_LEN: usize = 24;

/// AEAD key length (32 bytes for XChaCha20-Poly1305).
pub const AEAD_KEY_LEN: usize = 32;

/// Per-sandbox auth record persisted to disk.
///
/// **v3.** Holds secret material only:
/// `signing_key_bytes` and `preview_secrets`. Pg owns every other
/// piece of per-sandbox state (`user_id`, `project_id`, `backend`,
/// `vm_index`, `agent_url`, `key_fp`, `created_at`, share-token audit
/// rows). The boot-time reconciler reads `boot_id` to distinguish
/// orphans from this controller's lifetime vs orphans from a previous
/// boot.
///
/// v2 records still deserialize through this struct — serde silently
/// drops the legacy fields the v2 writer emitted (`user_id`,
/// `project_id`, etc.). The v3 reader trusts pg for those values, not
/// the sealed record. There are no production v2 records (pre-launch);
/// the back-compat read is for unit tests and dev fixtures.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SealedAuth {
    pub version: u8,
    /// Kept as the join key during restart-restore. The
    /// caller computes the sealed filename from the sandbox UUID
    /// (SHA-256-truncated) and passes the typed-id back via this
    /// field after decrypt; pg's `sandboxes.sandbox_id` row is the
    /// source for everything else.
    pub sandbox_id: String,
    /// Raw 32-byte Ed25519 secret key. The on-disk form is AEAD-
    /// sealed; once decrypted, callers must wrap in `Arc<SigningKey>`
    /// promptly (matching the in-memory hygiene of the per-backend
    /// records) and avoid copying these bytes around.
    pub signing_key_bytes: [u8; 32],
    /// Per-sandbox HMAC secret ring used to sign preview share
    /// tokens. `None` until the controller has minted at least one
    /// share-token secret for this sandbox. The on-wire JSON omits
    /// the field entirely (via `skip_serializing_if`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_secrets: Option<SealedPreviewSecrets>,
    /// Monotonically-increasing per-process boot
    /// counter. Set by the controller when sealing; the boot-time
    /// reconciler reads this to tell "sealed in this controller's
    /// lifetime" (orphan from a partially-cancelled create here)
    /// from "sealed in a previous boot" (different alarm severity).
    /// `None` for v2 records that never carried a boot_id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<u64>,
}

/// On-disk form of the per-sandbox HMAC secret ring used to sign
/// preview share tokens. Mirrors [`crate::registry::PreviewSecrets`]
/// — the registry holds the live in-memory copy; this is the
/// sealed-record snapshot.
///
/// **Field stability.** This struct is part of the v2 sealed-record
/// wire format. Adding a field is OK only if it's `Option<…>` with
/// `serde(default)`; removing or renaming a field is a wire-incompat
/// change that requires a SEAL_VERSION bump. Audit-table entries
/// ([`SealedAuditEntry`]) are NOT in this struct — they live in
/// [`SealedAuth::preview_audit`] alongside the ring so a corrupt-ring
/// recovery doesn't lose audit history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SealedPreviewSecrets {
    /// Monotonically-increasing version counter. Each rotate bumps
    /// this by 1; tokens carry the value in their `sv` claim.
    pub sv_current: u32,
    pub current: [u8; 32],
    /// Previous-version secret, retained during the rotation grace
    /// window. `None` after explicit `DELETE` (zero-grace path) or
    /// before the first rotation.
    pub previous: Option<[u8; 32]>,
    /// Unix-seconds at which the previous secret ages out. `None`
    /// when `previous` is `None`. Validators ignore the previous
    /// secret past this point even if `previous` is `Some` (covers
    /// the controller-restart case where the in-memory grace timer
    /// wouldn't otherwise survive).
    pub grace_until_unix: Option<u64>,
}

/// On-disk audit-log entry per minted share token. **No longer carried
/// inside `SealedAuth`** — share-token audit metadata
/// is now a `sandbox.shares` row in pg. The struct is retained for
/// v2-record back-compat (the v2 deserializer would otherwise refuse
/// to round-trip records that carried `preview_audit`) and for the
/// share-list internal helper that adapts the pg row shape into the
/// public JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SealedAuditEntry {
    pub token_id: String,
    pub port: u16,
    pub issued_at_unix: u64,
    pub expires_at_unix: u64,
    pub scope: String,
    pub secret_version: u32,
    /// Issuer typed-id (creator's `usr_…`), if known at mint-time.
    /// `None` for legacy / dev-mode mints where the controller didn't
    /// surface a typed-id principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
    pub last_used_at_unix: u64,
    pub use_count: u64,
}

impl SealedAuth {
    /// The sealed record holds only the secret
    /// material the controller can't reproduce from pg. Pg holds
    /// `user_id`, `project_id`, `backend`, `vm_index`, `agent_url`,
    /// `key_fp`, `created_at` — they don't pass through here.
    pub fn from_secrets(
        sandbox_id: Uuid,
        auth: &SandboxAuth,
        boot_id: Option<u64>,
    ) -> Self {
        Self {
            version: SEAL_VERSION,
            sandbox_id: sandbox_id.to_string(),
            signing_key_bytes: auth.signing_key.to_bytes(),
            preview_secrets: None,
            boot_id,
        }
    }

    /// Convert back to an in-memory `SandboxAuth`, given the
    /// non-secret fields (`agent_url`, `pubkey_fp`) sourced from pg
    /// at restore time.
    ///
    /// The caller is responsible for verifying the supplied
    /// `pubkey_fp` against the agent's response (the boot-path probe
    /// does this); this method only checks that the persisted signing
    /// key produces the same fingerprint as the one pg recorded —
    /// catches the rare case where pg and the sealed file disagree
    /// (operator review needed).
    pub fn into_sandbox_auth_with(
        &self,
        agent_url: String,
        pubkey_fp: String,
    ) -> Result<SandboxAuth, String> {
        if self.version == 0 || self.version > SEAL_VERSION {
            return Err(format!(
                "sealed record version {} not understood by this binary \
                 (binary supports v1..=v{SEAL_VERSION})",
                self.version
            ));
        }
        let signing_key = Arc::new(SigningKey::from_bytes(&self.signing_key_bytes));
        let computed_fp = sig::pubkey_fingerprint(&signing_key.verifying_key());
        if computed_fp != pubkey_fp {
            return Err(format!(
                "sealed record corrupted: derived pubkey_fp ({computed_fp}) \
                 does not match pg-side key_fp ({pubkey_fp})",
            ));
        }
        Ok(SandboxAuth {
            signing_key,
            agent_url,
            pubkey_fp,
        })
    }
}

/// Errors at the persist boundary. Strings carry detail; the variant
/// drives operator-visible action (corrupt → quarantine; wrong-key →
/// rotate; ...). String-only `Err` works for v1; introduce a richer
/// enum if/when the operator runbook needs structured codes.
pub fn seal_filename_for(sandbox_id: Uuid) -> String {
    let hex = sha256_hex_truncated(sandbox_id.to_string().as_bytes(), FILENAME_HEX_LEN);
    format!("{hex}.sealed")
}

/// Validate-then-hash filename derivation. Refuses anything that
/// doesn't parse as a UUID — the call sites already type sandbox_id
/// as `Uuid`, so this is belt-and-suspenders for any future caller
/// that takes a string.
///
/// `#[cfg(test)]`-gated: the only callers today are the round-6
/// CRITICAL-3 path-traversal regression tests in this module. If a
/// future stringly-typed caller appears, drop the cfg gate (and
/// promote to `pub` if cross-module).
#[cfg(test)]
pub(crate) fn seal_filename_for_str(sandbox_id_str: &str) -> Result<String, String> {
    // Parse-then-canonicalize; we hash the canonical form so two
    // alternate UUID encodings (hyphenated vs. simple) collide to the
    // same file. Unparseable inputs surface a clean error rather than
    // hashing arbitrary bytes (round-3 path-traversal hardening).
    let id: Uuid = sandbox_id_str.parse().map_err(|_| {
        format!("sandbox_id is not a valid UUID: {sandbox_id_str:?}")
    })?;
    Ok(seal_filename_for(id))
}

fn sha256_hex_truncated(bytes: &[u8], hex_len: usize) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = hex::encode(digest);
    s.truncate(hex_len);
    s
}

/// AEAD key wrapper. Owns the 32 bytes; redacting Debug. Constructed
/// from `SANDBOX_AEAD_KEY_PATH` only (file-mounted AEAD key rule: env-var sourcing
/// removed intentionally).
pub struct AeadKey {
    bytes: [u8; AEAD_KEY_LEN],
}

impl std::fmt::Debug for AeadKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AeadKey")
            .field("bytes", &"<redacted>")
            .finish()
    }
}

impl AeadKey {
    /// Construct from a raw 32-byte buffer. Caller is responsible for
    /// sourcing the bytes from a 0400-mode file. Tests use this
    /// directly; production goes through [`AeadKey::from_path`].
    pub fn from_bytes(bytes: [u8; AEAD_KEY_LEN]) -> Self {
        Self { bytes }
    }

    /// Read a 32-byte AEAD key from `path`. The file MUST be exactly
    /// 32 bytes. On Unix the file's mode is checked: anything other
    /// than `0400` is refused so a wider permission can't sneak past
    /// review (round-6 H8). The file's owner uid is also checked:
    /// only uid 0 (root) is accepted (R9-S4b) — mode 0o400 alone is
    /// insufficient because a non-root attacker who pre-creates a
    /// chmod-400 file at the path before systemd starts could supply
    /// a known/attacker-controlled key, breaking confidentiality of
    /// every future sealed record. The "uid == 0" invariant matches
    /// systemd-style secret loading at `/etc/zeroship/`, where the
    /// worker startup script creates the key as root.
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let path = path.as_ref();
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("stat AEAD key file {path:?}: {e}"))?;
        if meta.len() as usize != AEAD_KEY_LEN {
            return Err(format!(
                "AEAD key file {path:?} is {} bytes; expected exactly {AEAD_KEY_LEN}",
                meta.len()
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            use std::os::unix::fs::PermissionsExt as _;
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o400 {
                return Err(format!(
                    "AEAD key file {path:?} has mode {mode:#o}; expected 0o400 \
                     (owner read-only) — refuse to start. \
                     fix: chmod 0400 {path:?}",
                ));
            }
            let uid = meta.uid();
            if uid != 0 {
                return Err(format!(
                    "AEAD key file {path:?} has owner uid {uid} != 0 \
                     (refusing to load; chown root:root the file)"
                ));
            }
        }
        let mut buf = [0u8; AEAD_KEY_LEN];
        File::open(path)
            .map_err(|e| format!("open AEAD key file {path:?}: {e}"))?
            .read_exact(&mut buf)
            .map_err(|e| format!("read AEAD key file {path:?}: {e}"))?;
        Ok(Self { bytes: buf })
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new_from_slice(&self.bytes)
            .expect("AEAD key length validated by AeadKey::new")
    }
}

/// Seal `auth` into `<dir>/<filename>.sealed`. The filename is
/// derived from `sandbox_id` via SHA-256 truncation; **the raw
/// sandbox_id is never composed into a filesystem path** (round-6
/// CRITICAL-3 path-traversal hardening). Caller passes the parsed
/// `Uuid` so the type system precludes a stringly-typed bypass.
///
/// Returns the absolute path written, on success.
pub(crate) fn seal(
    sandbox_id: Uuid,
    auth: &SealedAuth,
    dir: &Path,
    key: &AeadKey,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let filename = seal_filename_for(sandbox_id);
    let path = dir.join(&filename);

    // Plaintext: JSON. Compact (no whitespace) — the file is meant
    // to be loaded back; a human pretty-print would only widen the
    // disk footprint.
    let plaintext = serde_json::to_vec(auth).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    })?;

    // Per-record nonce from the kernel CSPRNG. XChaCha20's 192-bit
    // nonce makes random selection collision-safe in practice
    // (well below 2^96 records before the birthday bound).
    let mut nonce_bytes = [0u8; NONCE_LEN];
    File::open("/dev/urandom")?.read_exact(&mut nonce_bytes)?;
    let nonce = XNonce::from(nonce_bytes);

    // Associated-data: the filename stem (the 32-char hex). Binds
    // the ciphertext to the file location. A record copied to a
    // different filename won't decrypt (its associated-data won't
    // match).
    let aad = filename.trim_end_matches(".sealed").as_bytes();

    let ct = key
        .cipher()
        .encrypt(&nonce, Payload { msg: &plaintext, aad })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

    // Layout: 24-byte nonce || ciphertext (which contains the 16-byte
    // tag at the end per Poly1305).
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);

    // Atomic-ish write via a `.tmp` next to the target + rename.
    // Avoids a half-written file confusing the next restart's
    // restore loop.
    let tmp_path = dir.join(format!("{filename}.tmp"));
    {
        let mut f = File::create(&tmp_path)?;
        f.write_all(&out)?;
        // 0400 like the AEAD key — the file holds an encrypted
        // signing key; even with AEAD a wider mode invites accidental
        // exfiltration via host backup tooling.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = f.metadata()?.permissions();
            perms.set_mode(0o400);
            std::fs::set_permissions(&tmp_path, perms)?;
        }
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, &path)?;
    Ok(path)
}

/// Unseal exactly one record from `path`. Returns `Err` on any of:
/// - file too short to be a valid record (< nonce + tag)
/// - AEAD verification failure (wrong key OR tampered ciphertext OR
///   moved file — associated-data mismatch)
/// - JSON deserialization failure
/// - schema-version higher than the binary understands
pub fn unseal_one(path: &Path, key: &AeadKey) -> std::io::Result<SealedAuth> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    if bytes.len() < NONCE_LEN + 16 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "sealed record {path:?} too short: {} bytes (need ≥ {})",
                bytes.len(),
                NONCE_LEN + 16
            ),
        ));
    }

    // Same AAD derivation as `seal`: the filename stem (without the
    // `.sealed` suffix). A record renamed under our nose won't
    // decrypt.
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("sealed record {path:?} has no UTF-8 filename"),
            )
        })?;
    let aad = filename.trim_end_matches(".sealed").as_bytes();

    let nonce_bytes: [u8; NONCE_LEN] = bytes[..NONCE_LEN]
        .try_into()
        .expect("split off NONCE_LEN above");
    let nonce = XNonce::from(nonce_bytes);
    let ct = &bytes[NONCE_LEN..];

    let pt = key
        .cipher()
        .decrypt(&nonce, Payload { msg: ct, aad })
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "AEAD verification failed (wrong key, tampered ciphertext, or moved file)",
            )
        })?;

    let auth: SealedAuth = serde_json::from_slice(&pt).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("sealed-record JSON parse failed for {path:?}: {e}"),
        )
    })?;
    if auth.version > SEAL_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "sealed record {path:?} schema-version {} > binary's {SEAL_VERSION}",
                auth.version
            ),
        ));
    }
    Ok(auth)
}

/// Unseal every `*.sealed` file in `dir`. Per-file failures are
/// returned as `Err` entries alongside the path so the caller can
/// quarantine a corrupt record and proceed (the boot path mustn't
/// fail-closed on a single bad file). `dir` not existing is treated
/// as "no records yet" and returns an empty Vec.
pub(crate) fn unseal_dir(dir: &Path, key: &AeadKey) -> std::io::Result<Vec<UnsealedRecord>> {
    let mut out = Vec::new();
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in read {
        let entry = entry?;
        let path = entry.path();
        if !path
            .extension()
            .is_some_and(|e| e == "sealed")
        {
            continue;
        }
        match unseal_one(&path, key) {
            Ok(auth) => out.push(UnsealedRecord {
                path: path.clone(),
                result: Ok(auth),
            }),
            Err(e) => out.push(UnsealedRecord {
                path,
                result: Err(e.to_string()),
            }),
        }
    }
    Ok(out)
}

/// One result from `unseal_dir`.
#[derive(Debug)]
pub struct UnsealedRecord {
    pub path: PathBuf,
    pub result: Result<SealedAuth, String>,
}

/// Best-effort sealed-record facility shared across backends.
///
/// Lifecycle wiring for preview auth persistence (`preview-URL` § II.0 §4):
/// each backend's
/// `create()` calls [`Persistence::seal`] after the sandbox is live;
/// each `stop()` calls [`Persistence::delete`] before returning. The
/// boot-path's restore loop reads the same files back via
/// [`Persistence::list`].
///
/// **Best-effort by contract.** Seal/delete failures NEVER propagate
/// up to fail `create`/`stop`. The sandbox is live in memory regardless;
/// persistence is for restart resilience, not for live operation.
/// Callers log loudly so operators see when persistence is degraded.
///
/// **One handle, many backends.** `from_env` returns a single
/// `Arc<Persistence>` that all three backends share — the file I/O
/// state (dir + AEAD key) is identical regardless of which backend is
/// minting the sandbox. Cloning the `Arc` is the per-backend cost.
///
/// **I/O on `spawn_blocking`.** All sync `std::fs` calls land inside a
/// `compio::runtime::spawn_blocking` so the ntex worker stays
/// responsive. Matches the rest of the crate's pattern (no tokio).
pub struct Persistence {
    /// Subdir under `persist_dir` we read+write — kept as the absolute
    /// `<persist_dir>/sealed-records` so callers can pass either to
    /// `restore_at_startup` (which expects the parent) and the
    /// internal sealers (which want the subdir).
    sealed_records_dir: PathBuf,
    key: Arc<AeadKey>,
}

impl std::fmt::Debug for Persistence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Persistence")
            .field("sealed_records_dir", &self.sealed_records_dir)
            // key intentionally omitted (AeadKey already redacts; this
            // is belt-and-suspenders so a future field addition can't
            // unintentionally widen the Debug surface).
            .finish_non_exhaustive()
    }
}

impl Persistence {
    /// Construct from the same env vars [`crate::AppState::from_config`]
    /// reads at boot:
    ///
    /// - `SANDBOX_PERSIST_AUTH=1` — feature flag (any other value or
    ///   unset returns `Ok(None)`; matches `persist_auth_enabled` in
    ///   `lib.rs`).
    /// - `SANDBOX_PERSIST_DIR` — parent dir; default
    ///   `/var/lib/zeroship/sandbox`.
    /// - `SANDBOX_AEAD_KEY_PATH` — file-mount only (file-mounted AEAD key rule;
    ///   env-var sourcing intentionally not supported because procfs
    ///   leaks).
    ///
    /// `Ok(None)` is the disabled/no-op shape — backends store the
    /// `Option<Arc<Persistence>>` they receive and skip the seal/delete
    /// calls entirely when it's `None`.
    pub fn from_env() -> Result<Option<Self>, String> {
        if !matches!(std::env::var("SANDBOX_PERSIST_AUTH").as_deref(), Ok("1")) {
            return Ok(None);
        }
        let key_path = std::env::var("SANDBOX_AEAD_KEY_PATH").map_err(|_| {
            "SANDBOX_AEAD_KEY_PATH not set (file-mount only — see `docs/proposals/sandbox-preview-urls.md` § IX.a)"
                .to_string()
        })?;
        let key = AeadKey::from_path(&key_path)?;
        let dir = std::env::var("SANDBOX_PERSIST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/var/lib/zeroship/sandbox"));
        Ok(Some(Self::new(dir, key)))
    }

    /// Build directly. Tests use this; production goes through
    /// [`Persistence::from_env`]. `persist_dir` is the parent
    /// (e.g. `/var/lib/zeroship/sandbox`); the `sealed-records/`
    /// subdir is appended internally to match the layout
    /// [`crate::restore::restore_at_startup`] reads.
    pub fn new(persist_dir: PathBuf, key: AeadKey) -> Self {
        Self {
            sealed_records_dir: persist_dir.join("sealed-records"),
            key: Arc::new(key),
        }
    }

    /// Seal `record` to disk. Best-effort: errors surface as `Err` for
    /// the caller to log, but the caller MUST NOT fail the surrounding
    /// `create()` on this. File I/O runs on `spawn_blocking`.
    pub async fn seal(
        &self,
        sandbox_id: Uuid,
        record: &SealedAuth,
    ) -> std::io::Result<()> {
        // Clone what we need into the blocking closure. The AEAD key
        // is already in an `Arc`; `record` is small enough to clone
        // (a Vec of strings + 32 bytes of key material).
        let dir = self.sealed_records_dir.clone();
        let key = self.key.clone();
        let record = record.clone();
        compio::runtime::spawn_blocking(move || seal(sandbox_id, &record, &dir, &key).map(|_| ()))
            .await
            .unwrap_or_else(|p| {
                Err(std::io::Error::other(format!(
                    "spawn_blocking panic: {p:?}"
                )))
            })
    }

    /// Remove the sealed record for `sandbox_id`. Best-effort: a
    /// missing file is treated as success (idempotent — matches the
    /// `stop()` contract). Other I/O errors propagate as `Err` for the
    /// caller to log.
    pub async fn delete(&self, sandbox_id: Uuid) -> std::io::Result<()> {
        let path = self.sealed_records_dir.join(seal_filename_for(sandbox_id));
        compio::runtime::spawn_blocking(move || match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        })
        .await
        .unwrap_or_else(|p| {
            Err(std::io::Error::other(format!(
                "spawn_blocking panic: {p:?}"
            )))
        })
    }

    /// Per-sandbox unseal. **B19 fix (2026-05-23)**: the wake path's
    /// `do_restore_inner` calls this after `wait_for_livez` Ok to
    /// recover the `signing_key_bytes` it needs to install the
    /// restored VM into the backend's in-memory state map. Returns
    /// `NotFound` when the sealed record is absent (the restore path
    /// will surface this as a 500 — a live restored VM whose sealed
    /// record was reaped is a controller-bug, not a normal flow).
    /// Other errors propagate the underlying `unseal_one` error.
    pub async fn unseal(&self, sandbox_id: Uuid) -> std::io::Result<SealedAuth> {
        let path = self.sealed_records_dir.join(seal_filename_for(sandbox_id));
        let key = self.key.clone();
        compio::runtime::spawn_blocking(move || unseal_one(&path, &key))
            .await
            .unwrap_or_else(|p| {
                Err(std::io::Error::other(format!(
                    "spawn_blocking panic: {p:?}"
                )))
            })
    }

    /// List + unseal every record in the sealed-records dir. Returns
    /// the same shape [`unseal_dir`] does (per-file `Ok`/`Err`
    /// results) so callers can quarantine corrupt files individually
    /// without failing the whole list. A missing dir is `Ok(vec![])`.
    pub async fn list(&self) -> std::io::Result<Vec<UnsealedRecord>> {
        let dir = self.sealed_records_dir.clone();
        let key = self.key.clone();
        compio::runtime::spawn_blocking(move || unseal_dir(&dir, &key))
            .await
            .unwrap_or_else(|p| {
                Err(std::io::Error::other(format!(
                    "spawn_blocking panic: {p:?}"
                )))
            })
    }

    /// Absolute path of the sealed-records dir. Used by tests; the
    /// boot path uses `restore::restore_at_startup` which takes the
    /// parent.
    pub fn sealed_records_dir(&self) -> &Path {
        &self.sealed_records_dir
    }

    /// Parent of [`Self::sealed_records_dir`] — i.e. the value the
    /// operator gave via `SANDBOX_PERSIST_DIR`. Used by the boot
    /// restore loop, which expects the parent and computes
    /// `<parent>/sealed-records/` itself.
    pub fn persist_dir(&self) -> PathBuf {
        // sealed_records_dir = <persist_dir>/sealed-records by
        // construction in `Persistence::new`. Strip the trailing
        // component to recover the parent.
        self.sealed_records_dir
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.sealed_records_dir.clone())
    }

    /// Borrow the AEAD key for the (synchronous) restore code path.
    /// The key is held in an `Arc` so this is a refcount bump, not a
    /// secret-bytes copy. Tests in this module also use it.
    pub fn aead_key(&self) -> &AeadKey {
        &self.key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn fresh_dir(label: &str) -> PathBuf {
        let p = std::env::temp_dir()
            .join(format!("zsbx-persist-{label}-{}", Uuid::now_v7().simple()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn fresh_key(seed: u8) -> AeadKey {
        AeadKey::from_bytes([seed; AEAD_KEY_LEN])
    }

    fn make_auth(sandbox_id: Uuid) -> (SealedAuth, [u8; 32], String) {
        let sk_bytes = [0xab; 32];
        let sk = SigningKey::from_bytes(&sk_bytes);
        let fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let auth = SealedAuth {
            version: SEAL_VERSION,
            sandbox_id: sandbox_id.to_string(),
            signing_key_bytes: sk_bytes,
            preview_secrets: None,
            boot_id: Some(7),
        };
        (auth, sk_bytes, fp)
    }

    #[test]
    fn roundtrip_preserves_secret_fields() {
        let dir = fresh_dir("roundtrip");
        let key = fresh_key(0x42);
        let id = Uuid::now_v7();
        let (auth, _sk, expected_fp) = make_auth(id);
        let path = seal(id, &auth, &dir, &key).expect("seal");
        assert!(path.starts_with(&dir));
        // Filename: hex(sha256(sandbox_id.to_string()))[..32] + ".sealed"
        let expected_name = seal_filename_for(id);
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), &expected_name);

        let got = unseal_one(&path, &key).expect("unseal");
        assert_eq!(got.version, SEAL_VERSION);
        assert_eq!(got.sandbox_id, auth.sandbox_id);
        assert_eq!(got.signing_key_bytes, auth.signing_key_bytes);
        assert_eq!(got.boot_id, Some(7));
        assert!(got.preview_secrets.is_none());

        // The reconstituted SandboxAuth has a working signing key
        // (matches the persisted pubkey fingerprint). The pg-side
        // `key_fp` is supplied by the caller at restore time; the
        // sealed record itself is silent on it.
        let reconstituted =
            got.into_sandbox_auth_with("http://10.99.107.2:7777".into(), expected_fp.clone()).unwrap();
        assert_eq!(reconstituted.pubkey_fp, expected_fp);
        assert_eq!(reconstituted.agent_url, "http://10.99.107.2:7777");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_ciphertext_fails_unseal() {
        let dir = fresh_dir("tamper");
        let key = fresh_key(0x10);
        let id = Uuid::now_v7();
        let (auth, _, _) = make_auth(id);
        let path = seal(id, &auth, &dir, &key).expect("seal");

        // Flip a single byte mid-ciphertext (past the 24-byte
        // nonce). AEAD must reject.
        let mut bytes = std::fs::read(&path).unwrap();
        let target = NONCE_LEN + 5;
        bytes[target] ^= 0x01;
        // Re-write as 0644 because the seal produced 0400; cheat for
        // the test only.
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, &bytes).unwrap();

        let err = unseal_one(&path, &key).expect_err("must fail");
        assert!(
            err.to_string().contains("AEAD verification failed"),
            "expected AEAD failure; got {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_key_fails_unseal() {
        let dir = fresh_dir("wrongkey");
        let key1 = fresh_key(0x11);
        let key2 = fresh_key(0x22);
        let id = Uuid::now_v7();
        let (auth, _, _) = make_auth(id);
        let path = seal(id, &auth, &dir, &key1).expect("seal");

        let err = unseal_one(&path, &key2).expect_err("wrong key must fail");
        assert!(
            err.to_string().contains("AEAD verification failed"),
            "expected AEAD failure; got {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// path-traversal regression. A
    /// stringly-typed path that `seal_filename_for_str` should
    /// REJECT (not parseable as UUID); the typed `seal_filename_for`
    /// path is unreachable for non-UUID inputs at compile time. The
    /// belt-and-suspenders test asserts:
    ///   1. The string-form helper refuses the malicious input.
    ///   2. Even if a future contributor weakens the typed path, the
    ///      filename derivation goes through SHA-256 → 32 hex chars,
    ///      which by construction can't escape the dir.
    #[test]
    fn seal_path_is_inside_dir_for_evil_id() {
        // (1) String-form helper refuses non-UUID inputs.
        let err = seal_filename_for_str("../../etc/passwd").expect_err("must reject");
        assert!(err.contains("not a valid UUID"), "expected UUID parse error; got {err}");

        // (2) Even if we hash the malicious string directly (the
        //     internal helper), the result is 32 hex chars and
        //     `Path::join` can't escape `dir`.
        let evil_hash = sha256_hex_truncated(b"../../etc/passwd", FILENAME_HEX_LEN);
        // 32 hex chars, all lowercase [0-9a-f]. No slashes, no dots,
        // no parent-dir tokens.
        assert_eq!(evil_hash.len(), FILENAME_HEX_LEN);
        assert!(evil_hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!evil_hash.contains('/'));
        assert!(!evil_hash.contains('.'));

        let dir = fresh_dir("evil-id");
        let composed = dir.join(format!("{evil_hash}.sealed"));
        assert!(
            composed.starts_with(&dir),
            "filename must stay inside dir; composed={composed:?}, dir={dir:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unseal_dir_returns_per_file_results() {
        let dir = fresh_dir("dir");
        let key = fresh_key(0x99);
        let id_a = Uuid::now_v7();
        let id_b = Uuid::now_v7();
        let (a, _, _) = make_auth(id_a);
        let (b, _, _) = make_auth(id_b);
        seal(id_a, &a, &dir, &key).unwrap();
        seal(id_b, &b, &dir, &key).unwrap();

        // One stray non-`.sealed` file MUST be ignored.
        std::fs::write(dir.join("README.txt"), b"not a sealed record").unwrap();

        let mut got = unseal_dir(&dir, &key).expect("unseal_dir");
        got.sort_by(|x, y| {
            let xa = x.result.as_ref().map(|r| r.sandbox_id.clone()).unwrap_or_default();
            let ya = y.result.as_ref().map(|r| r.sandbox_id.clone()).unwrap_or_default();
            xa.cmp(&ya)
        });
        assert_eq!(got.len(), 2, "exactly two .sealed files");
        for r in got {
            r.result.expect("ok");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unseal_dir_returns_err_for_corrupt_records_inline() {
        let dir = fresh_dir("dir-mixed");
        let key = fresh_key(0x77);
        let id_good = Uuid::now_v7();
        let (good, _, _) = make_auth(id_good);
        seal(id_good, &good, &dir, &key).unwrap();

        // A second file with the right naming but garbage content.
        let stem = sha256_hex_truncated(b"never-existed", FILENAME_HEX_LEN);
        std::fs::write(dir.join(format!("{stem}.sealed")), b"garbage").unwrap();

        let got = unseal_dir(&dir, &key).expect("unseal_dir");
        assert_eq!(got.len(), 2);
        let oks = got.iter().filter(|r| r.result.is_ok()).count();
        let errs = got.iter().filter(|r| r.result.is_err()).count();
        assert_eq!((oks, errs), (1, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_higher_than_binary_is_rejected() {
        let dir = fresh_dir("future");
        let key = fresh_key(0x33);
        let id = Uuid::now_v7();
        let (mut auth, _, _) = make_auth(id);
        auth.version = SEAL_VERSION + 7; // pretend a future binary wrote this
        let path = seal(id, &auth, &dir, &key).unwrap();
        let err = unseal_one(&path, &key).expect_err("must reject future version");
        assert!(
            err.to_string().contains("schema-version"),
            "expected schema-version mention; got {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v2 → v3 back-compat: a record written by the v2 binary (with
    /// the legacy metadata fields `user_id`, `project_id`, `backend`,
    /// `vm_index`, `agent_url`, `pubkey_fp`, `created_at_secs`,
    /// `preview_audit`) must round-trip through the v3 deserializer
    /// with the dropped fields silently ignored. The v3 reader then
    /// gets the missing data from pg.
    #[test]
    fn v2_record_loads_into_v3_struct_with_legacy_fields_ignored() {
        let dir = fresh_dir("v2-compat");
        let key = fresh_key(0x44);
        let id = Uuid::now_v7();
        let sk = SigningKey::from_bytes(&[0xab; 32]);
        let fp = sig::pubkey_fingerprint(&sk.verifying_key());
        // Hand-craft the v2 JSON (the original v2 envelope).
        let sk_bytes_v: Vec<u8> = vec![0xab; 32];
        let v2_json = serde_json::json!({
            "version": 2u8,
            "sandbox_id": id.to_string(),
            "user_id": "alice",
            "project_id": "p1",
            "backend": "nomad-ch",
            "signing_key_bytes": sk_bytes_v,
            "vm_index": 7,
            "agent_url": null,
            "pubkey_fp": fp,
            "created_at_secs": 1_700_000_000u64,
        });
        // Serde silently drops the legacy fields; v3 only knows
        // version + sandbox_id + signing_key_bytes + preview_secrets
        // + boot_id.
        let parsed: SealedAuth = serde_json::from_value(v2_json).expect("parse v2");
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.sandbox_id, id.to_string());
        assert_eq!(parsed.signing_key_bytes, [0xab; 32]);
        assert!(parsed.preview_secrets.is_none(), "v2 record had no ring");
        assert_eq!(parsed.boot_id, None, "v2 had no boot_id");

        // Seal+unseal a hand-rolled v2 record on disk through the
        // AEAD layer; unseal_one must accept and return a v3-shaped
        // struct (legacy fields dropped).
        let v2_bytes = serde_json::to_vec(&parsed).unwrap();
        let filename = seal_filename_for(id);
        let path = dir.join(&filename);
        std::fs::create_dir_all(&dir).unwrap();
        let mut nonce_bytes = [0u8; NONCE_LEN];
        File::open("/dev/urandom").unwrap().read_exact(&mut nonce_bytes).unwrap();
        let nonce = XNonce::from(nonce_bytes);
        let aad = filename.trim_end_matches(".sealed").as_bytes();
        let ct = key
            .cipher()
            .encrypt(&nonce, Payload { msg: &v2_bytes, aad })
            .unwrap();
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        std::fs::write(&path, &out).unwrap();

        let got = unseal_one(&path, &key).expect("v2 record must load through v3 reader");
        assert_eq!(got.version, 2);
        assert_eq!(got.signing_key_bytes, [0xab; 32]);
        assert!(got.preview_secrets.is_none());
        assert_eq!(got.boot_id, None);
        // The v3 reader is silent on agent_url + key_fp; the caller
        // (restore loop) supplies them from the pg row.
        let recon = got
            .into_sandbox_auth_with("http://10.99.107.2:7777".into(), fp.clone())
            .unwrap();
        assert_eq!(recon.pubkey_fp, fp);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aead_key_debug_redacts_bytes() {
        let k = fresh_key(0x55);
        let s = format!("{k:?}");
        assert!(s.contains("redacted"), "AEAD key Debug must redact bytes; got {s}");
        assert!(!s.contains("55"), "AEAD key Debug must not print bytes; got {s}");
    }

    #[test]
    fn aead_key_from_path_rejects_wrong_size() {
        let dir = fresh_dir("aead-wrong-size");
        let p = dir.join("k");
        std::fs::write(&p, b"too-short").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o400)).unwrap();
        }
        let err = AeadKey::from_path(&p).expect_err("wrong size must fail");
        assert!(err.contains("expected exactly"), "got {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn aead_key_from_path_rejects_loose_permissions() {
        let dir = fresh_dir("aead-loose");
        let p = dir.join("k");
        std::fs::write(&p, [0u8; AEAD_KEY_LEN]).unwrap();
        // 0644 — wider than 0400; must be refused.
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = AeadKey::from_path(&p).expect_err("loose perms must fail");
        assert!(err.contains("0o400") || err.contains("400"), "got {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R9-S4b: a 0o400 AEAD key file owned by a non-root uid (i.e.
    /// the test-runner user, which is uid != 0 in CI/dev) MUST be
    /// refused. Without the owner check, a non-root attacker who
    /// pre-creates a chmod-400 file at `SANDBOX_AEAD_KEY_PATH` before
    /// the controller starts can supply an attacker-known key,
    /// breaking confidentiality of every future sealed record.
    /// Sibling of the R9-S4 fix on `snapshot_aead.rs::RootKek`.
    #[cfg(unix)]
    #[test]
    fn aead_key_from_path_rejects_non_root_owned_file() {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        let dir = fresh_dir("aead-non-root-owner");
        let p = dir.join("k");
        std::fs::write(&p, [0xa5u8; AEAD_KEY_LEN]).unwrap();
        // The file is created by the test-runner process, so its uid
        // == effective uid of the runner. Detect "running as root" by
        // reading that uid back: if it's 0 there's no non-root-owned
        // file to materialise, so skip (the positive-arm test below
        // covers that branch).
        let runner_uid = std::fs::metadata(&p).unwrap().uid();
        if runner_uid == 0 {
            eprintln!(
                "skipping aead_key_from_path_rejects_non_root_owned_file: \
                 running as root, can't materialise a non-root-owned AEAD key file"
            );
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o400)).unwrap();

        let err = AeadKey::from_path(&p)
            .expect_err("non-root-owned AEAD key must be refused even at mode 0o400");
        assert!(
            err.contains("owner uid") && err.contains("!= 0"),
            "error must mention owner uid != 0; got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R9-S4b positive arm: when the test runs as root, a 0o400 AEAD
    /// key file owned by root loads OK. Skipped when not running as
    /// root (the common case in CI/dev) — the negative arm above
    /// already pins the bug-fix assertion in non-root environments.
    #[cfg(unix)]
    #[test]
    fn aead_key_from_path_accepts_root_owned_file_when_running_as_root() {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        let dir = fresh_dir("aead-root-owner");
        let p = dir.join("k");
        std::fs::write(&p, [0xa5u8; AEAD_KEY_LEN]).unwrap();
        let runner_uid = std::fs::metadata(&p).unwrap().uid();
        if runner_uid != 0 {
            eprintln!(
                "skipping aead_key_from_path_accepts_root_owned_file_when_running_as_root: \
                 not running as root, can't create a root-owned AEAD key file"
            );
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o400)).unwrap();
        let key = AeadKey::from_path(&p).expect("root-owned 0o400 AEAD key must load");
        // Sanity-check the bytes round-trip — first byte of the key.
        assert_eq!(key.bytes[0], 0xa5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seal_filename_for_str_round_trips_with_canonical_uuid() {
        let id = Uuid::now_v7();
        let by_uuid = seal_filename_for(id);
        let by_str = seal_filename_for_str(&id.to_string()).unwrap();
        let by_simple = seal_filename_for_str(&id.simple().to_string()).unwrap();
        // Hyphenated and simple are the same UUID; same filename.
        assert_eq!(by_uuid, by_str);
        assert_eq!(by_uuid, by_simple);
    }

    /// `Persistence` end-to-end: seal → list → delete → list. Covers
    /// the surface backends call from their `create`/`stop` paths.
    #[compio::test]
    async fn persistence_seal_list_delete_round_trip() {
        let dir = fresh_dir("persist-rt");
        let key = fresh_key(0xab);
        let p = Persistence::new(dir.clone(), key);
        let id = Uuid::now_v7();
        let (sealed, _, _) = make_auth(id);

        // Empty start.
        let listed = p.list().await.expect("list empty");
        assert!(listed.is_empty(), "fresh dir must list zero records");

        // Seal one.
        p.seal(id, &sealed).await.expect("seal");
        // On-disk filename matches the documented hash format.
        let expected = dir.join("sealed-records").join(seal_filename_for(id));
        assert!(expected.exists(), "sealed file at expected path");

        // List sees it.
        let listed = p.list().await.expect("list one");
        assert_eq!(listed.len(), 1, "one record after seal");
        let rec = listed[0].result.as_ref().expect("ok");
        assert_eq!(rec.sandbox_id, id.to_string());

        // Delete by id.
        p.delete(id).await.expect("delete");
        assert!(!expected.exists(), "delete must remove the file");

        // List is back to empty.
        let listed = p.list().await.expect("list after delete");
        assert!(listed.is_empty(), "post-delete list must be empty");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Delete is idempotent: removing a record that was never sealed
    /// is `Ok(())`. Mirrors the `stop()` contract — controller crashed
    /// between `create` and `seal`, then `stop` fires; we must not
    /// fail-stop on a missing on-disk record.
    #[compio::test]
    async fn persistence_delete_missing_is_ok() {
        let dir = fresh_dir("persist-del-missing");
        let key = fresh_key(0xcd);
        let p = Persistence::new(dir.clone(), key);
        let id = Uuid::now_v7();
        p.delete(id).await.expect("delete missing must be ok");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Filename invariant — the on-disk filename matches
    /// `seal_filename_for`, and lives under `<persist_dir>/sealed-records/`.
    #[compio::test]
    async fn persistence_seal_uses_documented_filename_format() {
        let dir = fresh_dir("persist-fname");
        let key = fresh_key(0x07);
        let p = Persistence::new(dir.clone(), key);
        let id = Uuid::now_v7();
        let (sealed, _, _) = make_auth(id);
        p.seal(id, &sealed).await.expect("seal");

        let expected_name = seal_filename_for(id);
        let expected_path = dir.join("sealed-records").join(&expected_name);
        assert!(expected_path.exists(), "filename matches seal_filename_for");
        // 32 hex chars + ".sealed".
        assert_eq!(expected_name.len(), 32 + ".sealed".len());
        assert!(expected_name.ends_with(".sealed"));
        let stem = &expected_name[..32];
        assert!(stem.chars().all(|c| c.is_ascii_hexdigit()));

        let _ = std::fs::remove_dir_all(&dir);
    }

}
