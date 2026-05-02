//! Sealed-record persistence for `SandboxAuth` (preview-URL § II.0).
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
//!   per round-6 H8 of the design doc, env-var sourcing leaks via
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
pub const SEAL_VERSION: u8 = 1;

/// Length of the truncated SHA-256 digest used as the filename. 16
/// bytes → 32 hex chars. See module doc for the collision argument.
const FILENAME_HEX_LEN: usize = 32;

/// AEAD nonce length. XChaCha20 mandates 24 bytes; ChaCha20 is 12.
/// We pick the X-flavor so a fresh nonce per file can come straight
/// from `/dev/urandom` without having to track a per-key counter.
const NONCE_LEN: usize = 24;

/// AEAD key length (32 bytes for XChaCha20-Poly1305).
pub const AEAD_KEY_LEN: usize = 32;

/// Per-sandbox auth record persisted to disk. Mirrors `SandboxAuth`
/// plus enough metadata to re-bind the agent on restart (`vm_index`
/// for nomad-ch derives `agent_url`; `pubkey_fp` is checked by the
/// signed `/version` rebind probe in § II.5).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SealedAuth {
    pub version: u8,
    pub sandbox_id: String,
    pub user_id: String,
    pub project_id: String,
    /// `"docker"` | `"k8s"` | `"nomad-ch"`. Surfaced so the
    /// controller's restore path can route each record to the right
    /// backend; lifted from `SandboxInfo.backend`.
    pub backend: String,
    /// Raw 32-byte Ed25519 secret key. The on-disk form is AEAD-
    /// sealed; once decrypted, callers must wrap in `Arc<SigningKey>`
    /// promptly (matching the in-memory hygiene of the per-backend
    /// records) and avoid copying these bytes around.
    pub signing_key_bytes: [u8; 32],
    /// Nomad-CH only — used to recompute the deterministic
    /// `agent_url` (`http://10.99.<100+idx>.2:7777`) at restart.
    /// Round-6 I3: NOT sealing `agent_url` shrinks the attack surface
    /// and removes the migration hazard if the agent listen address
    /// ever changes.
    pub vm_index: Option<u16>,
    /// `agent_url` for backends where it isn't a deterministic
    /// function of an integer (Docker: container bridge IP; K8s:
    /// Pod IP / port-forward loopback). For nomad-ch we leave this
    /// `None` and recompute from `vm_index`.
    pub agent_url: Option<String>,
    /// SHA-256(verifying_key_bytes)[..8] hex. Used by the controller's
    /// signed `/version` rebind probe (§ II.5) to confirm the agent
    /// at `agent_url` is the one we minted keys for.
    pub pubkey_fp: String,
    pub created_at_secs: u64,
}

impl SealedAuth {
    /// Convenience: derive the on-disk `SealedAuth` from the
    /// in-memory `SandboxAuth` plus the surrounding metadata the
    /// registry already holds. Caller computes `created_at_secs`.
    pub fn from_components(
        sandbox_id: Uuid,
        user_id: &str,
        project_id: &str,
        backend: &str,
        auth: &SandboxAuth,
        vm_index: Option<u16>,
        nomad_ch_agent_url_is_derived: bool,
        created_at_secs: u64,
    ) -> Self {
        let agent_url = if nomad_ch_agent_url_is_derived {
            None
        } else {
            Some(auth.agent_url.clone())
        };
        Self {
            version: SEAL_VERSION,
            sandbox_id: sandbox_id.to_string(),
            user_id: user_id.to_string(),
            project_id: project_id.to_string(),
            backend: backend.to_string(),
            signing_key_bytes: auth.signing_key.to_bytes(),
            vm_index,
            agent_url,
            pubkey_fp: auth.pubkey_fp.clone(),
            created_at_secs,
        }
    }

    /// Convert back to an in-memory `SandboxAuth`. The caller is
    /// responsible for supplying `agent_url` for nomad-ch (where it
    /// isn't sealed; see § II.0 §4 round-6 I3). Other backends use
    /// the sealed `agent_url` directly via `into_sandbox_auth`.
    pub fn into_sandbox_auth_with(
        &self,
        agent_url: String,
    ) -> Result<SandboxAuth, String> {
        if self.version != SEAL_VERSION {
            return Err(format!(
                "sealed record version {} not understood by this binary \
                 (binary supports v{})",
                self.version, SEAL_VERSION
            ));
        }
        let signing_key = Arc::new(SigningKey::from_bytes(&self.signing_key_bytes));
        let computed_fp = sig::pubkey_fingerprint(&signing_key.verifying_key());
        if computed_fp != self.pubkey_fp {
            return Err(format!(
                "sealed record corrupted: pubkey_fp mismatch (record={}, \
                 derived-from-key={computed_fp})",
                self.pubkey_fp
            ));
        }
        Ok(SandboxAuth {
            signing_key,
            agent_url,
            pubkey_fp: self.pubkey_fp.clone(),
        })
    }

    /// Convert back to an in-memory `SandboxAuth` for backends whose
    /// `agent_url` is sealed alongside the keys (Docker / K8s).
    /// Returns `Err` for nomad-ch records (`agent_url == None` in
    /// that case — caller must use `into_sandbox_auth_with`).
    pub fn into_sandbox_auth(&self) -> Result<SandboxAuth, String> {
        let agent_url = self.agent_url.clone().ok_or_else(|| {
            "sealed record has no agent_url (nomad-ch); caller must \
             recompute from vm_index and use into_sandbox_auth_with"
                .to_string()
        })?;
        self.into_sandbox_auth_with(agent_url)
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
pub fn seal_filename_for_str(sandbox_id_str: &str) -> Result<String, String> {
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
/// from `SANDBOX_AEAD_KEY_PATH` only (round-6 H8: env-var sourcing
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
    /// review (round-6 H8).
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
            use std::os::unix::fs::PermissionsExt as _;
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o400 {
                return Err(format!(
                    "AEAD key file {path:?} has mode {mode:#o}; expected 0o400 \
                     (owner read-only) — refuse to start. \
                     fix: chmod 0400 {path:?}",
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
pub fn seal(
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
pub fn unseal_dir(dir: &Path, key: &AeadKey) -> std::io::Result<Vec<UnsealedRecord>> {
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

    fn make_auth(sandbox_id: Uuid) -> (SealedAuth, [u8; 32]) {
        let sk_bytes = [0xab; 32];
        let sk = SigningKey::from_bytes(&sk_bytes);
        let fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let auth = SealedAuth {
            version: SEAL_VERSION,
            sandbox_id: sandbox_id.to_string(),
            user_id: "alice".into(),
            project_id: "p1".into(),
            backend: "nomad-ch".into(),
            signing_key_bytes: sk_bytes,
            vm_index: Some(7),
            agent_url: None,
            pubkey_fp: fp,
            created_at_secs: 1_700_000_000,
        };
        (auth, sk_bytes)
    }

    #[test]
    fn roundtrip_preserves_all_fields() {
        let dir = fresh_dir("roundtrip");
        let key = fresh_key(0x42);
        let id = Uuid::now_v7();
        let (auth, _sk) = make_auth(id);
        let path = seal(id, &auth, &dir, &key).expect("seal");
        assert!(path.starts_with(&dir));
        // Filename: hex(sha256(sandbox_id.to_string()))[..32] + ".sealed"
        let expected_name = seal_filename_for(id);
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), &expected_name);

        let got = unseal_one(&path, &key).expect("unseal");
        assert_eq!(got.version, SEAL_VERSION);
        assert_eq!(got.sandbox_id, auth.sandbox_id);
        assert_eq!(got.user_id, "alice");
        assert_eq!(got.project_id, "p1");
        assert_eq!(got.backend, "nomad-ch");
        assert_eq!(got.signing_key_bytes, auth.signing_key_bytes);
        assert_eq!(got.vm_index, Some(7));
        assert_eq!(got.agent_url, None);
        assert_eq!(got.pubkey_fp, auth.pubkey_fp);
        assert_eq!(got.created_at_secs, 1_700_000_000);

        // The reconstituted SandboxAuth has a working signing key
        // (matches the persisted pubkey fingerprint). nomad-ch path:
        // caller supplies the agent_url separately.
        let reconstituted =
            got.into_sandbox_auth_with("http://10.99.107.2:7777".into()).unwrap();
        assert_eq!(reconstituted.pubkey_fp, auth.pubkey_fp);
        assert_eq!(reconstituted.agent_url, "http://10.99.107.2:7777");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_ciphertext_fails_unseal() {
        let dir = fresh_dir("tamper");
        let key = fresh_key(0x10);
        let id = Uuid::now_v7();
        let (auth, _) = make_auth(id);
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
        let (auth, _) = make_auth(id);
        let path = seal(id, &auth, &dir, &key1).expect("seal");

        let err = unseal_one(&path, &key2).expect_err("wrong key must fail");
        assert!(
            err.to_string().contains("AEAD verification failed"),
            "expected AEAD failure; got {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Round-6 CRITICAL-3 path-traversal regression. A
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
        let (a, _) = make_auth(id_a);
        let (b, _) = make_auth(id_b);
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
        let (good, _) = make_auth(id_good);
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
        let (mut auth, _) = make_auth(id);
        auth.version = SEAL_VERSION + 7; // pretend a future binary wrote this
        let path = seal(id, &auth, &dir, &key).unwrap();
        let err = unseal_one(&path, &key).expect_err("must reject future version");
        assert!(
            err.to_string().contains("schema-version"),
            "expected schema-version mention; got {err}"
        );
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
}
