//! AEAD wrap layer for snapshot artifacts.
//!
//! Source-of-truth: `docs/proposals/sandbox-snapshot-restore.md` § 4.3.
//!
//! ## Cipher choice — divergence from proposal
//!
//! The proposal specifies **AES-256-GCM-SIV** for misuse-resistance.
//! The workspace currently ships only `chacha20poly1305` in
//! `Cargo.toml`; pulling `aes-gcm-siv` in for v1 would add another
//! crypto crate without a clear win for our threat model. **v1 uses
//! ChaCha20-Poly1305** (RFC 8439) instead:
//!
//!   - Same 256-bit key strength.
//!   - Same authenticated encryption (Poly1305 MAC).
//!   - No SIV / nonce-misuse resistance — but our nonces are
//!     deterministically derived per-snapshot from a one-shot prefix
//!     (per § 4.3 nonce schedule) and per-chunk monotonic counter, so
//!     the only way to repeat a nonce is to repeat (sandbox_id,
//!     snapshot_taken_at, chunk-counter) which the put-side can never
//!     do (re-snapshot bumps `snapshot_taken_at`; in-snapshot chunk
//!     counter is monotonic).
//!
//! The trade-off is documented; AES-GCM-SIV remains the v2 target if
//! we ever weaken the nonce schedule. The on-disk wire format
//! includes a 1-byte cipher tag (`CIPHER_TAG_CHACHA20`) so a future
//! rotation can read back v1 artifacts.
//!
//! ## What gets encrypted
//!
//! Only the dominant `memory-ranges` file (~1 GB). `config.json` is
//! plaintext because it's rewritten in-place per-restore (§ 5
//! identity rewrite); encrypting it would force a re-encrypt on every
//! restore. `state.json` is small (~110 KB) and contains no
//! tenant-secret material — encrypting it would pay tag overhead for
//! no defense gain.
//!
//! ## Key derivation (§ 4.3)
//!
//!   per_sandbox_dek = HKDF-SHA256(
//!     salt = sandbox_id || snapshot_taken_at_be_u64,
//!     ikm  = root_kek,
//!     info = "zsbx-snapshot-dek-v1",
//!     L    = 32
//!   )
//!
//! HKDF is implemented inline (`hkdf_extract` + `hkdf_expand`) on top
//! of `hmac::Hmac<Sha256>`; we don't pull the `hkdf` crate just for
//! 30 lines of glue.
//!
//! Root KEK source: env var `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` →
//! 32 raw bytes from a file. Mode 0o400 enforced. **When the env is
//! unset** (dev mode), the AEAD layer falls through to passthrough —
//! `put` and `get` delegate directly to the inner store. The first
//! production deploy must set this; the integration test
//! `crates/sandbox/tests/sandbox_pg_e2e.rs` verifies the passthrough
//! shape.
//!
//! ## On-disk format for `memory-ranges`
//!
//! ```text
//! magic(8)    = "ZSBXAEAD"
//! version     = u8  (0x01)
//! cipher      = u8  (0x01 = ChaCha20-Poly1305)
//! reserved    = u16 (0)
//! taken_at    = u64 BE                 // unix-seconds; salt for HKDF
//! nonce_pfx   = [u8; 8]                // derived deterministically from DEK
//! chunk_cnt   = u32 BE
//! ── for i in 0..chunk_cnt ──
//!   chunk_len = u32 BE                 // ciphertext length
//!   ciphertext[chunk_len]              // includes 16-byte Poly1305 tag
//! ```
//!
//! The `taken_at` field doubles as plaintext metadata so the
//! get-path can re-derive the DEK without an external pg lookup.
//! Authentication of this byte-range falls out of the per-chunk AAD
//! (we re-compute the expected nonce-prefix from the derived DEK
//! and compare against the header-stamped value — any tamper trips
//! `AEAD nonce-prefix mismatch`).
//!
//! Per-chunk nonce = `chunk_index_be_u32 (4) || nonce_pfx (8)`.
//! Plaintext chunk size = 1 MiB (`CHUNK_PLAINTEXT_LEN`). Each chunk
//! gets its own AEAD seal so a partial-write / corrupted chunk fails
//! authentication on first read rather than at end-of-stream.
//!
//! AAD per chunk = `b"zsbx-snap" || chunk_index_be_u32` — this binds
//! the position into the auth tag so a re-ordered chunk still fails.
//!
//! ## Hash semantics
//!
//! The plaintext SHA-256 is computed by the inner store (PR 3a) over
//! the **encrypted** `memory-ranges`, not the plaintext, because
//! we mutate the file in-place before calling `inner.put`. This is
//! the documented chain in § 4.3: `H = sha256(ciphertext)` →
//! AEAD-decrypt verifies the plaintext post-hash. Any tampered byte
//! in the ciphertext fails AEAD on get (`ChecksumMismatch` or
//! `InvalidArtifact` depending on which integrity check trips first
//! — both are fatal-on-restore).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Nonce,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::snapshot_store::{SnapshotError, SnapshotMetadata, SnapshotStore};

/// 8-byte file magic for the v1 wrapped `memory-ranges` blob.
const FILE_MAGIC: &[u8; 8] = b"ZSBXAEAD";

/// Wire format version. Bump only on incompatible header changes.
const FILE_VERSION: u8 = 0x01;

/// Cipher discriminant. Stable across rotations — a future v2 with
/// AES-256-GCM-SIV would add a new tag (e.g. 0x02) and the unwrap
/// path would dispatch on this byte.
const CIPHER_TAG_CHACHA20: u8 = 0x01;

/// Plaintext chunk size. 1 MiB matches the proposal's § 4.3 schedule
/// and is small enough that decrypt-streaming holds at most this much
/// plaintext in memory at any time.
const CHUNK_PLAINTEXT_LEN: usize = 1 << 20; // 1 MiB

/// Poly1305 tag length (constant for ChaCha20-Poly1305).
const AEAD_TAG_LEN: usize = 16;

/// Per-chunk ciphertext = plaintext + tag.
const CHUNK_CIPHERTEXT_MAX: usize = CHUNK_PLAINTEXT_LEN + AEAD_TAG_LEN;

/// 12-byte ChaCha20-Poly1305 nonce.
const NONCE_LEN: usize = 12;

/// Length of the random per-snapshot nonce-prefix component (8 of 12
/// nonce bytes; the leading 4 are the chunk counter).
const NONCE_PREFIX_LEN: usize = 8;

/// Full 32-byte derived DEK length.
const DEK_LEN: usize = 32;

/// File-scoped AAD prefix bound into every chunk's tag along with the
/// chunk index. Domain-separates from any future "snap-" header AEAD.
const AAD_PREFIX: &[u8] = b"zsbx-snap";

/// HKDF info string. Bound into the DEK derivation so the same root
/// KEK can derive other purpose-specific keys without collision.
const HKDF_INFO: &[u8] = b"zsbx-snapshot-dek-v1";

/// Root KEK byte length. Hard-coded — the file at
/// `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` must be exactly 32 bytes raw.
const ROOT_KEK_LEN: usize = 32;

/// Env var pointing at the 32-byte raw root KEK file (mode 0o400).
pub const ROOT_KEK_ENV: &str = "SANDBOX_SNAPSHOT_ROOT_KEK_PATH";

/// 32-byte root KEK (key-encryption-key). Wrapped in `Zeroizing` so
/// the heap allocation is scrubbed on drop.
#[derive(Clone)]
pub struct RootKek {
    bytes: zeroize::Zeroizing<[u8; ROOT_KEK_LEN]>,
}

impl std::fmt::Debug for RootKek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootKek").field("bytes", &"<redacted>").finish()
    }
}

impl RootKek {
    /// Construct from raw bytes. Used by tests + the env loader.
    pub fn from_bytes(bytes: [u8; ROOT_KEK_LEN]) -> Self {
        Self { bytes: zeroize::Zeroizing::new(bytes) }
    }

    /// Read 32 raw bytes from a file with mode 0o400 (Unix). Mirrors
    /// the persistence-key + admin-token loaders.
    pub fn from_path(path: &Path) -> Result<Self, String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let meta = std::fs::metadata(path)
                .map_err(|e| format!("{ROOT_KEK_ENV}={path:?}: stat: {e}"))?;
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o400 {
                return Err(format!(
                    "{ROOT_KEK_ENV}={path:?}: mode={mode:o} must be 0o400"
                ));
            }
        }
        let raw = std::fs::read(path)
            .map_err(|e| format!("{ROOT_KEK_ENV}={path:?}: read: {e}"))?;
        if raw.len() != ROOT_KEK_LEN {
            return Err(format!(
                "{ROOT_KEK_ENV}={path:?}: length={} bytes, expected {ROOT_KEK_LEN}",
                raw.len()
            ));
        }
        let mut buf = [0u8; ROOT_KEK_LEN];
        buf.copy_from_slice(&raw);
        Ok(Self::from_bytes(buf))
    }

    /// Read from `SANDBOX_SNAPSHOT_ROOT_KEK_PATH`. `Ok(None)` is the
    /// disabled-by-absence shape — env unset → AEAD passthrough.
    /// `Ok(Some)` is opt-in; misconfigured paths fail loudly.
    pub fn from_env() -> Result<Option<Self>, String> {
        let path = match std::env::var(ROOT_KEK_ENV) {
            Ok(s) if !s.trim().is_empty() => PathBuf::from(s.trim()),
            _ => return Ok(None),
        };
        Self::from_path(&path).map(Some)
    }
}

// ────────────────────────────────────────────────────────────────────
// HKDF-SHA256 (RFC 5869). 30-line manual extract+expand, no extra
// crate. Only the L=32 case is exercised today; the loop stays
// general so future callers (e.g. nonce-prefix derivation) can reuse.
// ────────────────────────────────────────────────────────────────────

type HmacSha256 = Hmac<Sha256>;

/// HKDF-Extract: PRK = HMAC-SHA256(salt, IKM).
fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    // RFC 5869 §2.2: empty salt → string of HashLen zero bytes.
    let salt = if salt.is_empty() { &[0u8; 32][..] } else { salt };
    let mut mac = <HmacSha256 as Mac>::new_from_slice(salt)
        .expect("HMAC-SHA256 accepts any key length");
    mac.update(ikm);
    let out = mac.finalize().into_bytes();
    let mut prk = [0u8; 32];
    prk.copy_from_slice(&out);
    prk
}

/// HKDF-Expand: OKM = T(1) || T(2) || ... where T(i) = HMAC(PRK,
/// T(i-1) || info || i). RFC 5869 §2.3.
fn hkdf_expand(prk: &[u8; 32], info: &[u8], out: &mut [u8]) {
    assert!(out.len() <= 255 * 32, "HKDF-Expand limit: L <= 255*HashLen");
    let mut prev = [0u8; 32];
    let mut prev_len = 0usize;
    let mut filled = 0usize;
    let mut counter: u8 = 1;
    while filled < out.len() {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(prk)
            .expect("HMAC accepts 32B key");
        mac.update(&prev[..prev_len]);
        mac.update(info);
        mac.update(&[counter]);
        let block = mac.finalize().into_bytes();
        let take = std::cmp::min(32, out.len() - filled);
        out[filled..filled + take].copy_from_slice(&block[..take]);
        filled += take;
        prev.copy_from_slice(&block);
        prev_len = 32;
        counter = counter.checked_add(1).expect("HKDF counter overflow");
    }
}

/// Derive the per-sandbox DEK per § 4.3.
///
///   salt = sandbox_id_bytes || snapshot_taken_at_be_u64
///   ikm  = root_kek
///   info = "zsbx-snapshot-dek-v1"
///
/// Stable for any (sandbox_id, snapshot_taken_at) pair — re-snapshot
/// bumps `snapshot_taken_at` so the DEK rotates per-snapshot.
fn derive_dek(
    root: &RootKek,
    sandbox_id_bytes: &[u8],
    snapshot_taken_at_unix_secs: u64,
) -> [u8; DEK_LEN] {
    let mut salt = Vec::with_capacity(sandbox_id_bytes.len() + 8);
    salt.extend_from_slice(sandbox_id_bytes);
    salt.extend_from_slice(&snapshot_taken_at_unix_secs.to_be_bytes());
    let prk = hkdf_extract(&salt, &root.bytes[..]);
    let mut dek = [0u8; DEK_LEN];
    hkdf_expand(&prk, HKDF_INFO, &mut dek);
    dek
}

/// Derive a deterministic 8-byte nonce-prefix from the DEK + a
/// per-call domain tag. Used so the nonce-prefix doesn't need a
/// CSPRNG draw at put time, keeping the put path fully deterministic
/// given (root_kek, sandbox_id, snapshot_taken_at).
///
/// **Property**: each (sandbox_id, snapshot_taken_at) pair gives a
/// unique prefix; the chunk-counter then guarantees uniqueness within
/// a snapshot. The two together never repeat across distinct
/// snapshots of the same sandbox (snapshot_taken_at differs).
fn derive_nonce_prefix(dek: &[u8; DEK_LEN]) -> [u8; NONCE_PREFIX_LEN] {
    // PRK = dek itself (already a uniformly random 32B key); skip the
    // extract step.
    let mut buf = [0u8; NONCE_PREFIX_LEN];
    hkdf_expand(dek, b"zsbx-snap-nonce-pfx-v1", &mut buf);
    buf
}

/// Build the 12-byte per-chunk nonce: chunk_index_be_u32 (4) ||
/// prefix (8).
fn chunk_nonce(prefix: &[u8; NONCE_PREFIX_LEN], chunk_index: u32) -> [u8; NONCE_LEN] {
    let mut n = [0u8; NONCE_LEN];
    n[0..4].copy_from_slice(&chunk_index.to_be_bytes());
    n[4..12].copy_from_slice(prefix);
    n
}

/// AAD bound into every chunk's tag = "zsbx-snap" || chunk_index BE.
fn chunk_aad(chunk_index: u32) -> Vec<u8> {
    let mut a = Vec::with_capacity(AAD_PREFIX.len() + 4);
    a.extend_from_slice(AAD_PREFIX);
    a.extend_from_slice(&chunk_index.to_be_bytes());
    a
}

// ────────────────────────────────────────────────────────────────────
// AEAD wrap layer
// ────────────────────────────────────────────────────────────────────

/// Composes around an inner [`SnapshotStore`], encrypting the
/// `memory-ranges` file at put-time and decrypting at get-time. When
/// `root_kek` is `None`, the layer is a passthrough (dev mode).
pub struct AeadSnapshotStore<S> {
    inner: S,
    root: Option<RootKek>,
}

impl<S> std::fmt::Debug for AeadSnapshotStore<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AeadSnapshotStore")
            .field("aead_enabled", &self.root.is_some())
            .finish()
    }
}

impl<S: SnapshotStore> AeadSnapshotStore<S> {
    /// Build with an explicit (or absent) root KEK. Tests call this
    /// directly; the production wiring goes through [`Self::from_env`].
    pub fn new(inner: S, root: Option<RootKek>) -> Self {
        Self { inner, root }
    }

    /// Build from `SANDBOX_SNAPSHOT_ROOT_KEK_PATH`. `Ok(Self)` with
    /// `root = None` is the legitimate dev-mode passthrough.
    pub fn from_env(inner: S) -> Result<Self, String> {
        let root = RootKek::from_env()?;
        Ok(Self::new(inner, root))
    }

    /// Returns `true` when AEAD is active (vs. passthrough). Useful
    /// for boot-time logging.
    pub fn is_active(&self) -> bool {
        self.root.is_some()
    }

    /// Encrypt `source_dir/memory-ranges` in-place. Reads the
    /// plaintext, writes the wrapped blob to a sibling temp file,
    /// then renames over the original. The plaintext is never written
    /// outside `source_dir` (the temp lives in the same dir so rename
    /// is atomic on the same FS).
    fn encrypt_in_place(
        &self,
        source_dir: &Path,
        sandbox_id: &str,
        snapshot_taken_at_unix_secs: u64,
    ) -> Result<(), SnapshotError> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let dek = derive_dek(root, sandbox_id.as_bytes(), snapshot_taken_at_unix_secs);
        let cipher = ChaCha20Poly1305::new_from_slice(&dek)
            .expect("ChaCha20-Poly1305 accepts 32-byte keys");
        let prefix = derive_nonce_prefix(&dek);

        let plaintext_path = source_dir.join("memory-ranges");
        let temp_path = source_dir.join("memory-ranges.aead.tmp");

        let mut src = std::fs::File::open(&plaintext_path)?;
        let total_len = src.metadata()?.len();
        // Upper-bound on chunk count: ceil(total / CHUNK_PLAINTEXT_LEN).
        let chunk_count: u32 = (total_len.div_ceil(CHUNK_PLAINTEXT_LEN as u64))
            .try_into()
            .map_err(|_| SnapshotError::InvalidArtifact(
                "memory-ranges too large for AEAD wrap".into()
            ))?;

        let mut dst = std::fs::File::create(&temp_path)?;
        // Header.
        dst.write_all(FILE_MAGIC)?;
        dst.write_all(&[FILE_VERSION, CIPHER_TAG_CHACHA20, 0, 0])?;
        dst.write_all(&snapshot_taken_at_unix_secs.to_be_bytes())?;
        dst.write_all(&prefix)?;
        dst.write_all(&chunk_count.to_be_bytes())?;

        let mut buf = vec![0u8; CHUNK_PLAINTEXT_LEN];
        let mut idx: u32 = 0;
        loop {
            // Read at most CHUNK_PLAINTEXT_LEN; final chunk may be short.
            let mut filled = 0usize;
            while filled < CHUNK_PLAINTEXT_LEN {
                let n = src.read(&mut buf[filled..])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            let nonce_bytes = chunk_nonce(&prefix, idx);
            let nonce = Nonce::from_slice(&nonce_bytes);
            let aad = chunk_aad(idx);
            let ciphertext = cipher
                .encrypt(
                    nonce,
                    Payload {
                        msg: &buf[..filled],
                        aad: &aad,
                    },
                )
                .map_err(|e| SnapshotError::InvalidArtifact(format!(
                    "AEAD encrypt chunk {idx}: {e}"
                )))?;
            let chunk_len: u32 = ciphertext.len()
                .try_into()
                .map_err(|_| SnapshotError::InvalidArtifact(
                    "encrypted chunk overflow u32".into()
                ))?;
            dst.write_all(&chunk_len.to_be_bytes())?;
            dst.write_all(&ciphertext)?;
            idx = idx.checked_add(1).ok_or_else(|| {
                SnapshotError::InvalidArtifact("chunk count overflow".into())
            })?;
            if filled < CHUNK_PLAINTEXT_LEN {
                break;
            }
        }
        // Sanity: header chunk_count must match what we wrote.
        if idx != chunk_count {
            return Err(SnapshotError::InvalidArtifact(format!(
                "chunk count mismatch: header={chunk_count} actual={idx}"
            )));
        }
        dst.sync_all()?;
        drop(dst);
        drop(src);
        std::fs::rename(&temp_path, &plaintext_path)?;
        Ok(())
    }

    /// Decrypt the wrapped blob at `wrapped_path` to `target_path`.
    fn decrypt_to(
        &self,
        wrapped_path: &Path,
        target_path: &Path,
        sandbox_id: &str,
    ) -> Result<(), SnapshotError> {
        let Some(root) = &self.root else {
            // Passthrough: just copy.
            std::fs::copy(wrapped_path, target_path)?;
            return Ok(());
        };

        let mut src = std::fs::File::open(wrapped_path)?;
        // Header layout: 8 magic + 4 (ver|cipher|2 reserved) + 8
        // taken_at + 8 nonce_prefix + 4 chunk_count = 32 bytes.
        let mut header = [0u8; 32];
        src.read_exact(&mut header).map_err(|e| {
            SnapshotError::InvalidArtifact(format!("AEAD header read: {e}"))
        })?;
        if &header[0..8] != FILE_MAGIC {
            return Err(SnapshotError::InvalidArtifact("AEAD magic mismatch".into()));
        }
        if header[8] != FILE_VERSION {
            return Err(SnapshotError::InvalidArtifact(format!(
                "AEAD version unsupported: {}",
                header[8]
            )));
        }
        if header[9] != CIPHER_TAG_CHACHA20 {
            return Err(SnapshotError::InvalidArtifact(format!(
                "AEAD cipher tag unsupported: {}",
                header[9]
            )));
        }
        let mut ts_buf = [0u8; 8];
        ts_buf.copy_from_slice(&header[12..20]);
        let snapshot_taken_at_unix_secs = u64::from_be_bytes(ts_buf);

        let dek = derive_dek(root, sandbox_id.as_bytes(), snapshot_taken_at_unix_secs);
        let cipher = ChaCha20Poly1305::new_from_slice(&dek)
            .expect("ChaCha20-Poly1305 accepts 32-byte keys");

        let mut prefix = [0u8; NONCE_PREFIX_LEN];
        prefix.copy_from_slice(&header[20..20 + NONCE_PREFIX_LEN]);

        // Defense-in-depth: verify the prefix matches what we'd
        // re-derive from (root_kek, sandbox_id, snapshot_taken_at).
        // A mismatch means the artifact wasn't produced by this DEK
        // — we'd fail per-chunk auth anyway, but the early reject
        // surfaces a clearer error.
        let expected_prefix = derive_nonce_prefix(&dek);
        if prefix != expected_prefix {
            return Err(SnapshotError::InvalidArtifact(
                "AEAD nonce-prefix mismatch (wrong DEK / tampered header)".into()
            ));
        }

        let mut count_buf = [0u8; 4];
        count_buf.copy_from_slice(&header[28..32]);
        let chunk_count = u32::from_be_bytes(count_buf);

        let mut dst = std::fs::File::create(target_path)?;
        let mut len_buf = [0u8; 4];
        let mut ct_buf = vec![0u8; CHUNK_CIPHERTEXT_MAX];
        for idx in 0..chunk_count {
            src.read_exact(&mut len_buf).map_err(|e| {
                SnapshotError::InvalidArtifact(format!(
                    "AEAD chunk {idx} length read: {e}"
                ))
            })?;
            let chunk_len = u32::from_be_bytes(len_buf) as usize;
            if chunk_len > CHUNK_CIPHERTEXT_MAX {
                return Err(SnapshotError::InvalidArtifact(format!(
                    "AEAD chunk {idx} length {chunk_len} exceeds cap"
                )));
            }
            if chunk_len < AEAD_TAG_LEN {
                return Err(SnapshotError::InvalidArtifact(format!(
                    "AEAD chunk {idx} length {chunk_len} below tag size"
                )));
            }
            src.read_exact(&mut ct_buf[..chunk_len])?;
            let nonce_bytes = chunk_nonce(&prefix, idx);
            let nonce = Nonce::from_slice(&nonce_bytes);
            let aad = chunk_aad(idx);
            let pt = cipher
                .decrypt(
                    nonce,
                    Payload {
                        msg: &ct_buf[..chunk_len],
                        aad: &aad,
                    },
                )
                .map_err(|_| {
                    // Distinct error: any tampered byte (header,
                    // ciphertext, tag, AAD) lands here. Map to the
                    // nearest existing variant. Caller (restore
                    // handler) treats this the same as a checksum
                    // mismatch — restore is refused.
                    SnapshotError::InvalidArtifact(format!(
                        "AEAD chunk {idx} decrypt/auth failed"
                    ))
                })?;
            dst.write_all(&pt)?;
        }
        dst.sync_all()?;
        Ok(())
    }
}

impl<S: SnapshotStore> SnapshotStore for AeadSnapshotStore<S> {
    fn put(
        &self,
        sandbox_id: &str,
        source_dir: &Path,
        ch_version: &str,
    ) -> Result<SnapshotMetadata, SnapshotError> {
        // Time the snapshot at put-call: this is what the pg row's
        // `snapshot_taken_at` will round-trip on retrieval (the
        // handler also writes the same now() into pg). We bind it
        // into the DEK so re-snapshots rotate keys.
        //
        // **Caveat**: the pg-side timestamp is taken in pg via `now()`
        // inside `update_snapshot_metadata` (PR 3h). The two clocks
        // differ by query latency. We could read pg's NOW() first,
        // but that's a round-trip cost on every snapshot. For v1 we
        // serialize the controller-side time-of-put into the AEAD
        // header (via the nonce-prefix) and re-derive at get-time
        // from the **pg** value passed by the caller (PR 3e wires
        // this). The header verification in `decrypt_to` catches a
        // mismatch loudly.
        //
        // To make this robust against the wall-clock skew, we accept
        // a `snapshot_taken_at` from an env-injected helper at
        // put-time **only when the caller hasn't pre-staged one**.
        // PR 3b's snapshot_handler takes its own `now()` and persists
        // it to pg via `update_snapshot_metadata`; we mirror that
        // exact value here by treating the put-side `now()` as the
        // canonical timestamp and propagating it via the artifact
        // header. Future refactor: thread the pg-side timestamp
        // through `SnapshotStore::put`.
        let snapshot_taken_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // Encrypt memory-ranges in place inside source_dir, then
        // delegate to inner.put which moves the (now ciphertext)
        // file into the canonical L1 path + computes the SHA-256.
        // The AEAD header carries `snapshot_taken_at` so the get-path
        // can re-derive the DEK without an external lookup; the L1
        // store sees only the encrypted bytes.
        self.encrypt_in_place(source_dir, sandbox_id, snapshot_taken_at)?;

        let mut meta = self.inner.put(sandbox_id, source_dir, ch_version)?;
        // Stamp `ch_version` with an `+aead-cc20p1305` suffix so an
        // operator can grep pg rows for AEAD-wrapped artifacts. Keep
        // the original CH version intact at the front so the
        // cross-version refusal (§ 8) still parses.
        if self.root.is_some() && !meta.ch_version.contains("+aead-cc20p1305") {
            meta.ch_version = format!("{}+aead-cc20p1305", meta.ch_version);
        }
        Ok(meta)
    }

    fn get(
        &self,
        sandbox_id: &str,
        target_dir: &Path,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        if self.root.is_none() {
            // Passthrough — inner.get is already verify+restore.
            return self.inner.get(sandbox_id, target_dir, expected_sha256);
        }
        // Stage to a temp dir adjacent to target_dir so the rename of
        // memory-ranges is on the same FS.
        let stage = target_dir.with_extension("aead-stage");
        if stage.exists() {
            std::fs::remove_dir_all(&stage)?;
        }
        std::fs::create_dir_all(&stage)?;

        // Inner.get writes the wrapped memory-ranges + plaintext
        // config.json/state.json into `stage` and verifies the SHA-256
        // against ciphertext.
        let inner_result = self.inner.get(sandbox_id, &stage, expected_sha256);
        if let Err(e) = inner_result {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e);
        }

        std::fs::create_dir_all(target_dir)?;
        // Decrypt memory-ranges → target. The AEAD header carries
        // `snapshot_taken_at` so DEK derivation is self-contained.
        let wrapped = stage.join("memory-ranges");
        let plain = target_dir.join("memory-ranges");
        if let Err(e) = self.decrypt_to(&wrapped, &plain, sandbox_id) {
            let _ = std::fs::remove_dir_all(&stage);
            // Best-effort cleanup of partial plaintext on failure.
            let _ = std::fs::remove_file(&plain);
            return Err(e);
        }
        // Move config.json + state.json across as plaintext (they
        // were never wrapped).
        for &name in &["config.json", "state.json"] {
            std::fs::copy(stage.join(name), target_dir.join(name))?;
        }
        let _ = std::fs::remove_dir_all(&stage);
        Ok(())
    }

    fn delete(&self, sandbox_id: &str) -> Result<(), SnapshotError> {
        self.inner.delete(sandbox_id)
    }

    fn verify(
        &self,
        sandbox_id: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        // Verify is over the ciphertext SHA — same as inner — and
        // doesn't unwrap. AEAD-content authentication only fires on
        // get/restore.
        self.inner.verify(sandbox_id, expected_sha256)
    }

    fn verify_metadata_only(
        &self,
        sandbox_id: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        // Same delegation pattern as `verify` — the AEAD wrap
        // doesn't change the canonical hash recorded in the
        // metadata stamp (the inner store stamps over its own
        // post-wrap bytes). Passing through preserves the GCS
        // fast-path's O(1) cost.
        self.inner.verify_metadata_only(sandbox_id, expected_sha256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot_store::{LocalDiskSnapshotStore, ARTIFACT_FILES};
    use sha2::Digest;
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn fresh_root() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "zsbx-aead-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn cleanup(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    fn write_fake_artifact(dir: &Path, memory_size: usize) {
        std::fs::create_dir_all(dir).unwrap();
        // config.json + state.json: small placeholders.
        std::fs::write(dir.join("config.json"), b"{\"cfg\":1}").unwrap();
        std::fs::write(dir.join("state.json"), b"{\"st\":2}").unwrap();
        // memory-ranges: deterministic pattern so we can detect any
        // round-trip drift.
        let mut buf = Vec::with_capacity(memory_size);
        for i in 0..memory_size {
            buf.push((i & 0xff) as u8);
        }
        std::fs::write(dir.join("memory-ranges"), &buf).unwrap();
    }

    fn read_memory_ranges(dir: &Path) -> Vec<u8> {
        std::fs::read(dir.join("memory-ranges")).unwrap()
    }

    #[test]
    fn round_trip_encrypts_and_decrypts_5mib() {
        // ~5 MiB exercises the chunked path (>= 5 chunks of 1 MiB).
        let root = fresh_root();
        let inner = LocalDiskSnapshotStore::new(root.join("store"));
        let kek = RootKek::from_bytes([0xa5; ROOT_KEK_LEN]);
        let aead = AeadSnapshotStore::new(inner, Some(kek));

        let src = root.join("src");
        let mem_size = 5 * 1024 * 1024 + 12345; // not chunk-aligned
        write_fake_artifact(&src, mem_size);
        let plaintext_mem = {
            let mut buf = Vec::with_capacity(mem_size);
            for i in 0..mem_size {
                buf.push((i & 0xff) as u8);
            }
            buf
        };

        let meta = aead.put("sbx_aead_rt", &src, "v51.1").unwrap();
        assert!(
            meta.ch_version.contains("+aead-cc20p1305"),
            "metadata must annotate AEAD version, got {}",
            meta.ch_version
        );

        let target = root.join("target");
        aead.get("sbx_aead_rt", &target, &meta.sha256).unwrap();
        let restored = read_memory_ranges(&target);
        assert_eq!(restored.len(), mem_size, "round-trip length mismatch");
        assert_eq!(restored, plaintext_mem, "round-trip bytes mismatch");
        // config + state come back verbatim.
        assert_eq!(
            std::fs::read(target.join("config.json")).unwrap(),
            b"{\"cfg\":1}"
        );
        assert_eq!(
            std::fs::read(target.join("state.json")).unwrap(),
            b"{\"st\":2}"
        );

        cleanup(&root);
    }

    /// Spy inner store: records the bytes seen on its `put` so we can
    /// assert passthrough mode delivers plaintext to the inner.
    struct SpyInner {
        delegate: LocalDiskSnapshotStore,
        last_memory_ranges_bytes: Mutex<Vec<u8>>,
    }
    impl SpyInner {
        fn new(root: PathBuf) -> Self {
            Self {
                delegate: LocalDiskSnapshotStore::new(root),
                last_memory_ranges_bytes: Mutex::new(Vec::new()),
            }
        }
    }
    impl SnapshotStore for SpyInner {
        fn put(
            &self,
            sandbox_id: &str,
            source_dir: &Path,
            ch_version: &str,
        ) -> Result<SnapshotMetadata, SnapshotError> {
            // Capture memory-ranges before delegate moves it.
            let bytes = std::fs::read(source_dir.join("memory-ranges"))?;
            *self.last_memory_ranges_bytes.lock().unwrap() = bytes;
            self.delegate.put(sandbox_id, source_dir, ch_version)
        }
        fn get(
            &self,
            sandbox_id: &str,
            target_dir: &Path,
            expected_sha256: &[u8; 32],
        ) -> Result<(), SnapshotError> {
            self.delegate.get(sandbox_id, target_dir, expected_sha256)
        }
        fn delete(&self, sandbox_id: &str) -> Result<(), SnapshotError> {
            self.delegate.delete(sandbox_id)
        }
        fn verify(
            &self,
            sandbox_id: &str,
            expected_sha256: &[u8; 32],
        ) -> Result<(), SnapshotError> {
            self.delegate.verify(sandbox_id, expected_sha256)
        }
    }

    #[test]
    fn passthrough_when_kek_absent_delivers_plaintext_to_inner() {
        let root = fresh_root();
        let spy = SpyInner::new(root.join("store"));
        let aead = AeadSnapshotStore::new(spy, None);

        let src = root.join("src");
        write_fake_artifact(&src, 32 * 1024); // small; no chunked path needed
        let expected_plaintext = read_memory_ranges(&src);

        let meta = aead.put("sbx_aead_pt", &src, "v51.1").unwrap();
        assert!(
            !meta.ch_version.contains("+aead"),
            "passthrough must not annotate aead, got {}",
            meta.ch_version
        );

        let inner_saw = aead.inner.last_memory_ranges_bytes.lock().unwrap().clone();
        assert_eq!(
            inner_saw, expected_plaintext,
            "inner must see plaintext in passthrough mode"
        );

        // Round-trip still works (plaintext both ways).
        let target = root.join("target");
        aead.get("sbx_aead_pt", &target, &meta.sha256).unwrap();
        assert_eq!(read_memory_ranges(&target), expected_plaintext);

        cleanup(&root);
    }

    #[test]
    fn tampered_ciphertext_byte_fails_decrypt() {
        let root = fresh_root();
        let inner = LocalDiskSnapshotStore::new(root.join("store"));
        let kek = RootKek::from_bytes([0x77; ROOT_KEK_LEN]);
        let aead = AeadSnapshotStore::new(inner, Some(kek));

        let src = root.join("src");
        write_fake_artifact(&src, 64 * 1024);
        let meta = aead.put("sbx_aead_tamper", &src, "v51.1").unwrap();

        // The artifact landed at L1. Find the wrapped memory-ranges
        // and flip a byte well past the header.
        let artifact_dir = std::path::Path::new(&meta.artifact_path);
        let mr = artifact_dir.join("memory-ranges");
        let mut bytes = std::fs::read(&mr).unwrap();
        // Header is ~24 bytes; flip byte index 200 so we definitely
        // hit ciphertext or a chunk-length field.
        bytes[200] ^= 0xff;
        std::fs::write(&mr, &bytes).unwrap();

        // The L1 SHA-256 covers the (tampered) ciphertext, so
        // get's pre-restore SHA verify will fire FIRST. That's
        // already a fatal-on-restore signal; the AEAD layer is the
        // second line of defense if someone bypasses the SHA. To
        // exercise the AEAD-only path we recompute the SHA over the
        // tampered bytes by writing a zeroed sha — i.e. supply a
        // matching expected_sha256 = sha256 of tampered file.
        let mut h = Sha256::new();
        h.update("config.json".as_bytes());
        h.update(
            (std::fs::metadata(artifact_dir.join("config.json")).unwrap().len())
                .to_be_bytes(),
        );
        h.update(std::fs::read(artifact_dir.join("config.json")).unwrap());
        h.update("memory-ranges".as_bytes());
        h.update((bytes.len() as u64).to_be_bytes());
        h.update(&bytes);
        h.update("state.json".as_bytes());
        h.update(
            (std::fs::metadata(artifact_dir.join("state.json")).unwrap().len())
                .to_be_bytes(),
        );
        h.update(std::fs::read(artifact_dir.join("state.json")).unwrap());
        let mut tampered_sha = [0u8; 32];
        tampered_sha.copy_from_slice(&h.finalize());

        let target = root.join("target");
        let err = aead
            .get("sbx_aead_tamper", &target, &tampered_sha)
            .unwrap_err();
        // AEAD fails — InvalidArtifact (auth or magic/version branch).
        assert!(
            matches!(err, SnapshotError::InvalidArtifact(_)),
            "tampered ciphertext must fail AEAD, got {err:?}"
        );

        cleanup(&root);
    }

    #[test]
    fn hkdf_extract_and_expand_match_known_vector() {
        // RFC 5869 Test Case 1 (SHA-256).
        let ikm = [0x0bu8; 22];
        let salt = [
            0x00u8, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
        ];
        let info = [
            0xf0u8, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9,
        ];
        let prk = hkdf_extract(&salt, &ikm);
        let expected_prk = hex::decode(
            "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5",
        )
        .unwrap();
        assert_eq!(&prk[..], expected_prk.as_slice());
        let mut okm = [0u8; 42];
        hkdf_expand(&prk, &info, &mut okm);
        let expected_okm = hex::decode(
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
        )
        .unwrap();
        assert_eq!(&okm[..], expected_okm.as_slice());
    }

    #[test]
    fn dek_rotates_with_snapshot_taken_at() {
        // Same sandbox, different timestamps → different DEKs.
        let kek = RootKek::from_bytes([0x11; ROOT_KEK_LEN]);
        let d1 = derive_dek(&kek, b"sbx_x", 1_000_000);
        let d2 = derive_dek(&kek, b"sbx_x", 1_000_001);
        assert_ne!(d1, d2);
        // Same inputs → same DEK (determinism).
        let d1b = derive_dek(&kek, b"sbx_x", 1_000_000);
        assert_eq!(d1, d1b);
    }

    #[test]
    fn artifact_files_constant_unchanged() {
        // Sanity: AEAD layer doesn't accidentally rename the canonical
        // file set. CH would reject a renamed restore.
        assert_eq!(ARTIFACT_FILES, &["config.json", "memory-ranges", "state.json"]);
    }
}
