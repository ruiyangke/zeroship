//! GCS L2 + tiered (L1 → L2) snapshot stores.
//!
//! Source-of-truth: `docs/proposals/sandbox-snapshot-restore.md` § 4.
//!
//! ## Phase A — real GCS HTTP I/O
//!
//! - [`GcsSnapshotStore`]: production impl. Talks the GCS JSON API
//!   v1 over HTTPS via `ureq` (blocking; the controller wraps calls
//!   in `compio::runtime::spawn_blocking`). Auth is GCE metadata-
//!   server access tokens, cached until expiry minus 60 s.
//!   - `put`: single-shot upload (`uploadType=media`) for small
//!     files (config.json, state.json), resumable upload (8 MiB
//!     chunks) for memory-ranges (≈ 1 GB).
//!   - `get`: streamed download to disk, sha256 verified post-pull.
//!   - `delete`: idempotent per-file delete (404 → Ok).
//!   - `verify`: streams each artifact from GCS and recomputes the
//!     canonical SHA-256 over plaintext bytes; mismatch surfaces as
//!     `SnapshotError::ChecksumMismatch`. We intentionally do NOT
//!     trust bucket-side `x-goog-hash` metadata — an attacker with
//!     bucket-write could substitute both the body and the metadata.
//!
//! - [`TieredSnapshotStore`]: production composition. L1 = local
//!   disk (PR 3a), L2 = GCS (this file). Reads always hit L1 first;
//!   writes spawn a fire-and-forget compio task to also upload to
//!   L2.
//!
//! ## SHA-256 contract
//!
//! `x-goog-hash` is set per-file using the **plaintext SHA-256**
//! computed by the L1 layer (see `snapshot_store::compute_artifact_
//! sha256`). When the AEAD wrap layer (PR 3c) is enabled, GCS sees
//! ciphertext but the recorded hash is over the plaintext — the
//! receiver decrypts before verifying.
//!
//! ## v1 caveat for `put`
//!
//! Background L2 upload is fire-and-forget. Failures log warn; the
//! retry loop / metric is a follow-up — operators monitor via
//! l2_upload_failed_total when those land.
//!
//! ## v1 caveat for `get`
//!
//! L1 miss → L2 attempt directly (no side-effect "fetch back to
//! L1"). A side-effect populating L1 on successful L2 fetch is a
//! follow-up.

use std::io::Read;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use sha2::{Digest, Sha256};

use crate::snapshot_store::{SnapshotError, SnapshotMetadata, SnapshotStore, ARTIFACT_FILES};

const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

/// Refresh the cached token this many seconds before its true
/// expiry. Buffer so a long-running upload doesn't fall off the
/// edge mid-request.
const TOKEN_REFRESH_BUFFER_SECS: u64 = 60;

/// 8 MiB chunks for resumable uploads. Matches GCS's 256 KiB
/// alignment requirement (any multiple of 256 KiB works); 8 MiB
/// keeps the chunk count reasonable for a 1 GB memory-ranges file
/// (≈ 128 chunks).
const RESUMABLE_CHUNK_SIZE: usize = 8 * 1024 * 1024;

/// Files smaller than this use the single-shot upload path
/// (uploadType=media). Resumable upload's setup overhead isn't
/// worth it for ~100 KB state.json. 16 MiB keeps single-shot
/// usage to objects we can hold in RAM cheaply.
const SINGLE_SHOT_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Custom GCS object metadata key carrying the canonical artifact
/// SHA-256 (hex). Read by [`GcsSnapshotStore::verify_metadata_only`]
/// to avoid re-streaming the full ~1 GB artifact on every sweep.
///
/// The full header on the wire is `x-goog-meta-zsbx-canonical-sha256`
/// — GCS prefixes user-supplied metadata with `x-goog-meta-`. The
/// `zsbx-` prefix keeps the key namespaced so other tooling can
/// stamp its own metadata without collision.
const CANONICAL_SHA_METADATA_KEY: &str = "zsbx-canonical-sha256";

/// Which artifact carries the canonical-hash custom metadata. The
/// last file in `ARTIFACT_FILES` (`state.json`) — last to land in
/// the put loop, so if it's present and tagged the rest of the
/// artifact must have been successfully uploaded too. Small file,
/// fast to HEAD.
const CANONICAL_SHA_METADATA_OBJECT: &str = "state.json";

/// Cached OAuth2 access token + expiry deadline.
#[derive(Debug, Clone)]
struct CachedToken {
    bearer: String,
    /// `Instant::now() + token_ttl - buffer` — the moment after
    /// which we consider the token stale and refresh on the next
    /// request.
    refresh_after: Instant,
}

/// L2 GCS snapshot store. Talks the GCS JSON API v1 + the GCE
/// metadata server for OAuth2 access tokens. Blocking I/O; callers
/// in async contexts wrap in `compio::runtime::spawn_blocking`.
#[derive(Debug)]
pub struct GcsSnapshotStore {
    /// Target bucket (e.g. `suger-dev-zsbx-snapshots-v1`).
    pub bucket: String,
    /// Service-account identity (logging/diagnostic; the actual
    /// identity is whatever the GCE metadata server hands out).
    pub service_account: String,
    /// Per-request HTTP read timeout (default 60 s for chunk
    /// uploads / streamed downloads — small responses use lower
    /// timeouts inline).
    request_timeout: Duration,
    /// Cached metadata-server token. Wrapped in Mutex so the
    /// store can be shared via `Arc`.
    cached_token: Mutex<Option<CachedToken>>,
}

impl Clone for GcsSnapshotStore {
    fn clone(&self) -> Self {
        // Token cache is intentionally NOT cloned — each clone
        // refreshes its own. The metadata server is local so this
        // costs ~1 ms; sharing the lock across threads on the same
        // process happens via Arc.
        Self {
            bucket: self.bucket.clone(),
            service_account: self.service_account.clone(),
            request_timeout: self.request_timeout,
            cached_token: Mutex::new(None),
        }
    }
}

impl GcsSnapshotStore {
    /// Construct a real GCS store. No I/O happens here — the first
    /// call to `put`/`get`/`delete`/`verify` triggers the metadata-
    /// server token fetch.
    pub fn new(bucket: impl Into<String>, service_account: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            service_account: service_account.into(),
            request_timeout: Duration::from_secs(60),
            cached_token: Mutex::new(None),
        }
    }

    /// Build the canonical object key for a sandbox's snapshot
    /// artifact files. Stable across put/get so an out-of-band
    /// restore (operator `gsutil cp gs://bucket/key/...`) is
    /// unambiguous.
    pub fn object_prefix(&self, sandbox_id: &str) -> String {
        format!("snapshots/v1/{sandbox_id}/")
    }

    fn object_name(&self, sandbox_id: &str, file: &str) -> String {
        format!("{}{file}", self.object_prefix(sandbox_id))
    }

    /// Fetch (or reuse) an OAuth2 access token from the GCE
    /// metadata server. Token lifetime is typically 1 h; we refresh
    /// at `lifetime - 60 s` so a long-running upload completes
    /// without the token going stale mid-request.
    fn access_token(&self) -> Result<String, SnapshotError> {
        let mut guard = self
            .cached_token
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(cached) = guard.as_ref() {
            if Instant::now() < cached.refresh_after {
                return Ok(cached.bearer.clone());
            }
        }
        // Refresh. The metadata server requires the
        // `Metadata-Flavor: Google` header.
        let resp = ureq::get(METADATA_TOKEN_URL)
            .set("Metadata-Flavor", "Google")
            .timeout(Duration::from_secs(5))
            .call()
            .map_err(|e| {
                SnapshotError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("metadata token fetch: {e}"),
                ))
            })?;
        if resp.status() != 200 {
            return Err(SnapshotError::InvalidArtifact(format!(
                "metadata token fetch: status {}",
                resp.status()
            )));
        }
        // Response shape: {"access_token": "...", "expires_in": 3599, "token_type": "Bearer"}
        let body = resp.into_string().map_err(|e| {
            SnapshotError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("read token body: {e}"),
            ))
        })?;
        let parsed: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| SnapshotError::InvalidArtifact(format!("parse token: {e}")))?;
        let bearer = parsed["access_token"]
            .as_str()
            .ok_or_else(|| SnapshotError::InvalidArtifact("token: no access_token".into()))?
            .to_string();
        let ttl_secs = parsed["expires_in"].as_u64().unwrap_or(3600);
        let buffer = TOKEN_REFRESH_BUFFER_SECS.min(ttl_secs.saturating_sub(1));
        let refresh_after = Instant::now()
            + Duration::from_secs(ttl_secs.saturating_sub(buffer));
        *guard = Some(CachedToken {
            bearer: bearer.clone(),
            refresh_after,
        });
        Ok(bearer)
    }

    /// Single-shot upload (uploadType=media) for small files.
    /// Returns Ok on 200; otherwise InvalidArtifact with status +
    /// body for diagnostics.
    ///
    /// `canonical_sha256_stamp` — if Some, stamps the canonical
    /// artifact hash as custom object metadata so
    /// [`GcsSnapshotStore::verify_metadata_only`] can read it back
    /// via the JSON-API object-metadata endpoint instead of
    /// re-streaming the body. Set only when uploading
    /// [`CANONICAL_SHA_METADATA_OBJECT`].
    fn upload_single_shot(
        &self,
        sandbox_id: &str,
        file: &str,
        bytes: &[u8],
        sha256: &[u8; 32],
        canonical_sha256_stamp: Option<&[u8; 32]>,
    ) -> Result<(), SnapshotError> {
        let token = self.access_token()?;
        let object = self.object_name(sandbox_id, file);
        let url = format!(
            "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=media&name={}",
            self.bucket,
            urlencoding(&object)
        );
        let sha_b64 = base64::engine::general_purpose::STANDARD.encode(sha256);
        let canonical_hex = canonical_sha256_stamp.map(hex::encode);
        let mut req = ureq::post(&url)
            .set("authorization", &format!("Bearer {token}"))
            .set("content-type", "application/octet-stream")
            .set("x-goog-hash", &format!("sha256={sha_b64}"))
            .timeout(self.request_timeout);
        if let Some(hex_str) = canonical_hex.as_deref() {
            req = req.set(
                &format!("x-goog-meta-{CANONICAL_SHA_METADATA_KEY}"),
                hex_str,
            );
        }
        let resp = req.send_bytes(bytes);
        match resp {
            Ok(r) if r.status() == 200 => Ok(()),
            Ok(r) => Err(SnapshotError::InvalidArtifact(format!(
                "GCS single-shot upload {object}: status {}, body={}",
                r.status(),
                r.into_string().unwrap_or_default()
            ))),
            Err(ureq::Error::Status(code, r)) => Err(SnapshotError::InvalidArtifact(format!(
                "GCS single-shot upload {object}: status {code}, body={}",
                r.into_string().unwrap_or_default()
            ))),
            Err(e) => Err(SnapshotError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("GCS single-shot upload {object}: {e}"),
            ))),
        }
    }

    /// Resumable upload for large files (memory-ranges).
    ///
    /// 1. POST /upload/.../?uploadType=resumable&name=<obj> — get
    ///    a session URL from the `location` header.
    /// 2. PUT chunks of `RESUMABLE_CHUNK_SIZE` bytes to the
    ///    session URL, each carrying a `Content-Range:
    ///    bytes <start>-<end>/<total>` header. 308 = continue.
    /// 3. Final chunk closes — 200/201 means complete.
    ///
    /// `canonical_sha256_stamp` — if Some, stamps the canonical
    /// artifact hash as custom object metadata on the initiate
    /// request (mirrors `upload_single_shot`). In practice the
    /// canonical-hash holder is `state.json`, which is small
    /// enough to take the single-shot path, so this param is
    /// typically `None` here. Kept for symmetry.
    fn upload_resumable(
        &self,
        sandbox_id: &str,
        file: &str,
        path: &Path,
        sha256: &[u8; 32],
        canonical_sha256_stamp: Option<&[u8; 32]>,
    ) -> Result<(), SnapshotError> {
        let token = self.access_token()?;
        let object = self.object_name(sandbox_id, file);
        let init_url = format!(
            "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=resumable&name={}",
            self.bucket,
            urlencoding(&object)
        );
        let sha_b64 = base64::engine::general_purpose::STANDARD.encode(sha256);
        let canonical_hex = canonical_sha256_stamp.map(hex::encode);
        // 1. Initiate session.
        let mut init_req = ureq::post(&init_url)
            .set("authorization", &format!("Bearer {token}"))
            .set("content-type", "application/octet-stream")
            .set("x-goog-hash", &format!("sha256={sha_b64}"))
            .set("content-length", "0")
            .timeout(Duration::from_secs(15));
        if let Some(hex_str) = canonical_hex.as_deref() {
            init_req = init_req.set(
                &format!("x-goog-meta-{CANONICAL_SHA_METADATA_KEY}"),
                hex_str,
            );
        }
        let init_resp = init_req.call();
        let session_url = match init_resp {
            Ok(r) if r.status() == 200 => r
                .header("location")
                .ok_or_else(|| {
                    SnapshotError::InvalidArtifact(
                        "GCS resumable init: 200 without location header".into(),
                    )
                })?
                .to_string(),
            Ok(r) => {
                return Err(SnapshotError::InvalidArtifact(format!(
                    "GCS resumable init {object}: status {}, body={}",
                    r.status(),
                    r.into_string().unwrap_or_default()
                )))
            }
            Err(ureq::Error::Status(code, r)) => {
                return Err(SnapshotError::InvalidArtifact(format!(
                    "GCS resumable init {object}: status {code}, body={}",
                    r.into_string().unwrap_or_default()
                )))
            }
            Err(e) => {
                return Err(SnapshotError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("GCS resumable init {object}: {e}"),
                )))
            }
        };
        // 2. Stream chunks.
        let metadata = std::fs::metadata(path)?;
        let total = metadata.len();
        let mut f = std::fs::File::open(path)?;
        let mut buf = vec![0u8; RESUMABLE_CHUNK_SIZE];
        let mut offset: u64 = 0;
        while offset < total {
            let want = ((total - offset) as usize).min(RESUMABLE_CHUNK_SIZE);
            let mut filled = 0usize;
            while filled < want {
                let n = f.read(&mut buf[filled..want])?;
                if n == 0 {
                    return Err(SnapshotError::InvalidArtifact(format!(
                        "{}: short read at offset {offset} (expected {want}, got {filled})",
                        path.display()
                    )));
                }
                filled += n;
            }
            let end = offset + filled as u64 - 1;
            let range_header = format!("bytes {offset}-{end}/{total}");
            let chunk = &buf[..filled];
            let resp = ureq::put(&session_url)
                .set("authorization", &format!("Bearer {token}"))
                .set("content-length", &chunk.len().to_string())
                .set("content-range", &range_header)
                .timeout(self.request_timeout)
                .send_bytes(chunk);
            match resp {
                Ok(r) if r.status() == 200 || r.status() == 201 => {
                    // Final response — upload complete.
                    return Ok(());
                }
                Ok(r) if r.status() == 308 => {
                    // Resume incomplete — proceed to next chunk.
                    offset += filled as u64;
                }
                Ok(r) => {
                    return Err(SnapshotError::InvalidArtifact(format!(
                        "GCS resumable chunk {range_header}: status {}, body={}",
                        r.status(),
                        r.into_string().unwrap_or_default()
                    )))
                }
                Err(ureq::Error::Status(308, _)) => {
                    offset += filled as u64;
                }
                Err(ureq::Error::Status(code, r)) => {
                    return Err(SnapshotError::InvalidArtifact(format!(
                        "GCS resumable chunk {range_header}: status {code}, body={}",
                        r.into_string().unwrap_or_default()
                    )))
                }
                Err(e) => {
                    return Err(SnapshotError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("GCS resumable chunk {range_header}: {e}"),
                    )))
                }
            }
        }
        // We expected the final chunk to return 200/201; loop ended
        // without that → defensive error.
        Err(SnapshotError::InvalidArtifact(format!(
            "GCS resumable {object}: ran off end without final 200/201"
        )))
    }

    /// Download a single artifact file to disk. Streams to avoid
    /// loading the full memory-ranges (~ 1 GB) into RAM.
    fn download_to_disk(
        &self,
        sandbox_id: &str,
        file: &str,
        dest: &Path,
    ) -> Result<(), SnapshotError> {
        let token = self.access_token()?;
        let object = self.object_name(sandbox_id, file);
        let url = format!(
            "https://storage.googleapis.com/{}/{}",
            self.bucket,
            urlencoding(&object)
        );
        let resp = ureq::get(&url)
            .set("authorization", &format!("Bearer {token}"))
            .timeout(self.request_timeout)
            .call();
        match resp {
            Ok(r) if r.status() == 200 => {
                let f = std::fs::File::create(dest)?;
                // R11-P2 (perf-r11, wake-path counterpart to R10-P5):
                // std lib's io::copy uses an 8 KiB default buffer.
                // A 1 GB memory-ranges download issues ≈131072
                // write(2)s unbuffered; the 1 MiB BufWriter collapses
                // that to ≈1024. We flush + drop before sync_all so
                // the BufWriter's internal buffer is observed on disk.
                let mut writer = std::io::BufWriter::with_capacity(1 << 20, f);
                let mut reader = r.into_reader();
                std::io::copy(&mut reader, &mut writer)?;
                let f = writer.into_inner().map_err(|e| {
                    SnapshotError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("BufWriter flush: {}", e.error()),
                    ))
                })?;
                f.sync_all()?;
                Ok(())
            }
            Ok(r) if r.status() == 404 => Err(SnapshotError::NotFound(format!(
                "{sandbox_id}/{file}"
            ))),
            Ok(r) => Err(SnapshotError::InvalidArtifact(format!(
                "GCS download {object}: status {}, body={}",
                r.status(),
                r.into_string().unwrap_or_default()
            ))),
            Err(ureq::Error::Status(404, _)) => Err(SnapshotError::NotFound(format!(
                "{sandbox_id}/{file}"
            ))),
            Err(ureq::Error::Status(code, r)) => Err(SnapshotError::InvalidArtifact(format!(
                "GCS download {object}: status {code}, body={}",
                r.into_string().unwrap_or_default()
            ))),
            Err(e) => Err(SnapshotError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("GCS download {object}: {e}"),
            ))),
        }
    }

    fn delete_object(&self, sandbox_id: &str, file: &str) -> Result<(), SnapshotError> {
        let token = self.access_token()?;
        let object = self.object_name(sandbox_id, file);
        let url = format!(
            "https://storage.googleapis.com/storage/v1/b/{}/o/{}",
            self.bucket,
            urlencoding(&object)
        );
        let resp = ureq::delete(&url)
            .set("authorization", &format!("Bearer {token}"))
            .timeout(Duration::from_secs(15))
            .call();
        match resp {
            // 204 = deleted, 404 = already gone (idempotent).
            Ok(r) if r.status() == 204 || r.status() == 404 => Ok(()),
            Ok(r) => Err(SnapshotError::InvalidArtifact(format!(
                "GCS delete {object}: status {}, body={}",
                r.status(),
                r.into_string().unwrap_or_default()
            ))),
            Err(ureq::Error::Status(404, _)) => Ok(()),
            Err(ureq::Error::Status(code, r)) => Err(SnapshotError::InvalidArtifact(format!(
                "GCS delete {object}: status {code}, body={}",
                r.into_string().unwrap_or_default()
            ))),
            Err(e) => Err(SnapshotError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("GCS delete {object}: {e}"),
            ))),
        }
    }
}

/// Per-file SHA-256 streamed off disk. Distinct from the canonical
/// concatenated-artifact hash in `snapshot_store::compute_artifact_
/// sha256` — that hash authenticates the *whole* artifact for the
/// L1 contract; per-file `x-goog-hash` is for GCS object-integrity
/// during transfer.
fn sha256_file(path: &Path) -> Result<[u8; 32], SnapshotError> {
    // R11-P3 (perf-r11, sibling of R5-P1 77ea717f): 1 MiB BufReader
    // collapses the 16× syscall amplification of the unbuffered
    // 64 KiB loop. On a 1 GB memory-ranges file the read(2) count
    // drops 16384 → 1024. Sha256 still sees byte-identical chunks.
    let f = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(1 << 20, f);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let h = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&h);
    Ok(out)
}

/// Minimal URL-encoding for object keys. GCS accepts `%2F` for `/`
/// in the object name, so paths like `snapshots/v1/sbx_xxx/config.json`
/// must be encoded.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl SnapshotStore for GcsSnapshotStore {
    fn put(
        &self,
        sandbox_id: &str,
        source_dir: &Path,
        ch_version: &str,
    ) -> Result<SnapshotMetadata, SnapshotError> {
        // Compute the canonical concatenated SHA-256 up-front so we
        // can stamp it as custom metadata on the last artifact
        // object (state.json — `CANONICAL_SHA_METADATA_OBJECT`). The
        // stamp lets [`verify_metadata_only`] run a cheap object-
        // metadata read instead of re-streaming ~1 GB of memory-
        // ranges on every sweep cycle.
        //
        // Order: canonical first (one full read), then per-file
        // hashes on the upload pass (one more full read each). The
        // pre-pass is unavoidable — we need the canonical hash in
        // hand before `state.json` hits the wire, and per-file
        // `x-goog-hash` is required for GCS-side single-object
        // integrity validation.
        let (canonical_sha256, canonical_bytes) =
            canonical_artifact_sha256(source_dir)?;
        let mut total: u64 = 0;
        for &name in ARTIFACT_FILES {
            let path = source_dir.join(name);
            let metadata = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(SnapshotError::MissingFile(name));
                }
                Err(e) => return Err(SnapshotError::Io(e)),
            };
            let len = metadata.len();
            let per_file_sha = sha256_file(&path)?;
            // Stamp the canonical hash only on the last artifact
            // (`state.json`). The HEAD-only verify path reads this
            // exact object. Concentrating the stamp on one object
            // (vs. mirroring on all three) keeps the put path
            // simple — partial-put failure modes already invalidate
            // the artifact regardless of which object carries the
            // hash, because canonical verify needs all three files.
            let canonical_stamp: Option<&[u8; 32]> =
                if name == CANONICAL_SHA_METADATA_OBJECT {
                    Some(&canonical_sha256)
                } else {
                    None
                };
            if len <= SINGLE_SHOT_MAX_BYTES {
                let bytes = std::fs::read(&path)?;
                self.upload_single_shot(
                    sandbox_id,
                    name,
                    &bytes,
                    &per_file_sha,
                    canonical_stamp,
                )?;
            } else {
                self.upload_resumable(
                    sandbox_id,
                    name,
                    &path,
                    &per_file_sha,
                    canonical_stamp,
                )?;
            }
            total = total.checked_add(len).ok_or_else(|| {
                SnapshotError::InvalidArtifact("size overflow".into())
            })?;
        }
        if canonical_bytes != total {
            return Err(SnapshotError::InvalidArtifact(format!(
                "byte count drift: per-file sum={total}, canonical={canonical_bytes}"
            )));
        }
        Ok(SnapshotMetadata {
            artifact_path: format!(
                "gs://{}/{}",
                self.bucket,
                self.object_prefix(sandbox_id)
            ),
            sha256: canonical_sha256,
            ch_version: ch_version.to_string(),
            bytes: canonical_bytes,
        })
    }

    fn get(
        &self,
        sandbox_id: &str,
        target_dir: &Path,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        std::fs::create_dir_all(target_dir)?;
        for &name in ARTIFACT_FILES {
            self.download_to_disk(sandbox_id, name, &target_dir.join(name))?;
        }
        // Verify canonical hash post-download.
        let (actual, _) = canonical_artifact_sha256(target_dir)?;
        if &actual != expected_sha256 {
            return Err(SnapshotError::ChecksumMismatch {
                expected: *expected_sha256,
                actual,
            });
        }
        Ok(())
    }

    fn delete(&self, sandbox_id: &str) -> Result<(), SnapshotError> {
        // Delete each file. Per-file errors aggregate; idempotent
        // contract means 404s are silently OK (handled inside
        // `delete_object`).
        let mut errs: Vec<String> = Vec::new();
        for &name in ARTIFACT_FILES {
            if let Err(e) = self.delete_object(sandbox_id, name) {
                errs.push(format!("{name}: {e}"));
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(SnapshotError::InvalidArtifact(format!(
                "GCS delete {sandbox_id}: {}",
                errs.join("; ")
            )))
        }
    }

    fn verify(
        &self,
        sandbox_id: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        // Integrity gate: stream each artifact file's bytes from GCS
        // through the canonical hasher (name || len_be || bytes per
        // ARTIFACT_FILES order) and compare to `expected_sha256`.
        //
        // Why we re-stream rather than trust GCS-side metadata:
        // `x-goog-hash` is per-file, so per-file hashes can't be
        // combined into the canonical concatenation hash without
        // re-reading the bytes. Trusting bucket-side metadata is also
        // unsafe against an attacker with bucket-write — they can
        // substitute both the bytes and the metadata. The canonical
        // SHA-256 over plaintext is the authentic gate (§ 4.3 trust
        // chain), so this method recomputes it end-to-end.
        //
        // Bandwidth cost: one full artifact read (~ 1 GB for memory-
        // ranges). Callers should reach for `verify` sparingly — e.g.
        // periodic L2 sweep, not per-restore (the `get` path verifies
        // post-download for free).
        verify_canonical_sha256_from_streams(expected_sha256, |name| {
            self.open_object_stream(sandbox_id, name)
        })
    }

    fn verify_metadata_only(
        &self,
        sandbox_id: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        // Fast-path: read the canonical-hash custom metadata
        // (`x-goog-meta-zsbx-canonical-sha256`) off the `state.json`
        // object via the JSON-API object-metadata endpoint and
        // compare against `expected_sha256`. One small HTTP request
        // (~ 100-byte response body), zero body egress for the
        // ~ 1 GB memory-ranges file.
        //
        // Trust model differs from [`verify`] (deep). Metadata is
        // bucket-writable, so this defends against bit-rot / silent
        // body corruption (where the bytes change but the metadata
        // doesn't) — the failure mode periodic sweeps target. A
        // malicious bucket-write principal can substitute both body
        // and metadata together; operators audit that case with
        // `verify`.
        verify_metadata_canonical_sha256(expected_sha256, |key| {
            self.head_object_metadata_value(
                sandbox_id,
                CANONICAL_SHA_METADATA_OBJECT,
                key,
            )
        })
    }
}

/// Streamed canonical-hash check over the three artifact files.
///
/// Returns the bytes via the `open` callback so the unit test can
/// inject in-memory readers; production passes GCS-backed readers via
/// [`GcsSnapshotStore::open_object_stream`]. Surfaces mismatch as
/// [`SnapshotError::ChecksumMismatch`] — the integrity-mismatch
/// envelope the trait already exposes.
fn verify_canonical_sha256_from_streams<F>(
    expected_sha256: &[u8; 32],
    mut open: F,
) -> Result<(), SnapshotError>
where
    F: FnMut(&'static str) -> Result<(u64, Box<dyn Read + Send>), SnapshotError>,
{
    let mut hasher = Sha256::new();
    for &name in ARTIFACT_FILES {
        let (len, mut reader) = open(name)?;
        hasher.update(name.as_bytes());
        hasher.update(len.to_be_bytes());
        let mut buf = vec![0u8; 64 * 1024];
        let mut read_total: u64 = 0;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            read_total = read_total.checked_add(n as u64).ok_or_else(|| {
                SnapshotError::InvalidArtifact("verify: read overflow".into())
            })?;
        }
        if read_total != len {
            // GCS declared a content-length we didn't match — treat
            // as integrity failure (the body was truncated, padded,
            // or content-length lied).
            return Err(SnapshotError::InvalidArtifact(format!(
                "verify {name}: read {read_total} bytes, expected {len}"
            )));
        }
    }
    let h = hasher.finalize();
    let mut actual = [0u8; 32];
    actual.copy_from_slice(&h);
    if &actual != expected_sha256 {
        return Err(SnapshotError::ChecksumMismatch {
            expected: *expected_sha256,
            actual,
        });
    }
    Ok(())
}

/// Metadata-only canonical-hash check.
///
/// `fetch_meta(key)` returns the value of a custom metadata key
/// (without the `x-goog-meta-` prefix). The production callback is
/// [`GcsSnapshotStore::head_object_metadata_value`]; tests inject a
/// closure simulating the bucket-side state.
///
/// Errors:
/// - missing key → [`SnapshotError::InvalidArtifact`] (the stamp was
///   never written, or was stripped — operators run deep `verify` to
///   re-authenticate and re-stamp).
/// - hex-decode / wrong length → [`SnapshotError::InvalidArtifact`].
/// - value present but mismatches → [`SnapshotError::ChecksumMismatch`].
fn verify_metadata_canonical_sha256<F>(
    expected_sha256: &[u8; 32],
    mut fetch_meta: F,
) -> Result<(), SnapshotError>
where
    F: FnMut(&str) -> Result<Option<String>, SnapshotError>,
{
    let raw = match fetch_meta(CANONICAL_SHA_METADATA_KEY)? {
        Some(v) => v,
        None => {
            return Err(SnapshotError::InvalidArtifact(format!(
                "verify_metadata_only: object {CANONICAL_SHA_METADATA_OBJECT}                  missing x-goog-meta-{CANONICAL_SHA_METADATA_KEY} header"
            )));
        }
    };
    let decoded = hex::decode(raw.trim()).map_err(|e| {
        SnapshotError::InvalidArtifact(format!(
            "verify_metadata_only: x-goog-meta-{CANONICAL_SHA_METADATA_KEY}              not hex: {e}"
        ))
    })?;
    if decoded.len() != 32 {
        return Err(SnapshotError::InvalidArtifact(format!(
            "verify_metadata_only: x-goog-meta-{CANONICAL_SHA_METADATA_KEY}              wrong length: got {}, want 32",
            decoded.len()
        )));
    }
    let mut actual = [0u8; 32];
    actual.copy_from_slice(&decoded);
    if &actual != expected_sha256 {
        return Err(SnapshotError::ChecksumMismatch {
            expected: *expected_sha256,
            actual,
        });
    }
    Ok(())
}

impl GcsSnapshotStore {
    /// Open a streamed read of a single artifact object. Returns
    /// `(content_length, reader)`. Errors:
    /// - 404 → [`SnapshotError::NotFound`]
    /// - missing/invalid content-length → [`SnapshotError::InvalidArtifact`]
    /// - transport → [`SnapshotError::Io`]
    fn open_object_stream(
        &self,
        sandbox_id: &str,
        file: &'static str,
    ) -> Result<(u64, Box<dyn Read + Send>), SnapshotError> {
        let token = self.access_token()?;
        let object = self.object_name(sandbox_id, file);
        let url = format!(
            "https://storage.googleapis.com/{}/{}",
            self.bucket,
            urlencoding(&object)
        );
        let resp = ureq::get(&url)
            .set("authorization", &format!("Bearer {token}"))
            .timeout(self.request_timeout)
            .call();
        let r = match resp {
            Ok(r) if r.status() == 200 => r,
            Ok(r) if r.status() == 404 => {
                return Err(SnapshotError::NotFound(format!("{sandbox_id}/{file}")));
            }
            Ok(r) => {
                return Err(SnapshotError::InvalidArtifact(format!(
                    "GCS verify-stream {object}: status {}, body={}",
                    r.status(),
                    r.into_string().unwrap_or_default()
                )))
            }
            Err(ureq::Error::Status(404, _)) => {
                return Err(SnapshotError::NotFound(format!("{sandbox_id}/{file}")));
            }
            Err(ureq::Error::Status(code, r)) => {
                return Err(SnapshotError::InvalidArtifact(format!(
                    "GCS verify-stream {object}: status {code}, body={}",
                    r.into_string().unwrap_or_default()
                )))
            }
            Err(e) => {
                return Err(SnapshotError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("GCS verify-stream {object}: {e}"),
                )));
            }
        };
        let len: u64 = r
            .header("content-length")
            .and_then(|h| h.parse().ok())
            .ok_or_else(|| {
                SnapshotError::InvalidArtifact(format!(
                    "GCS verify-stream {object}: missing/invalid content-length"
                ))
            })?;
        Ok((len, Box::new(r.into_reader())))
    }

    /// Fetch a single custom-metadata value for a snapshot object
    /// via the GCS JSON API object-metadata endpoint (no body
    /// transfer). `meta_key` is the *unprefixed* metadata key
    /// (e.g. `zsbx-canonical-sha256`); GCS exposes user-supplied
    /// metadata under the JSON `metadata.<key>` field.
    ///
    /// Returns:
    /// - `Ok(Some(value))` — present
    /// - `Ok(None)` — object exists but lacks that metadata key
    /// - `Err(NotFound)` — 404 (artifact gone)
    /// - `Err(InvalidArtifact / Io)` — transport / unexpected status
    fn head_object_metadata_value(
        &self,
        sandbox_id: &str,
        file: &'static str,
        meta_key: &str,
    ) -> Result<Option<String>, SnapshotError> {
        let token = self.access_token()?;
        let object = self.object_name(sandbox_id, file);
        // `fields=metadata` keeps the response payload to ~ 100
        // bytes — the whole point of the fast-path is avoiding
        // body egress.
        let url = format!(
            "https://storage.googleapis.com/storage/v1/b/{}/o/{}?fields=metadata",
            self.bucket,
            urlencoding(&object)
        );
        let resp = ureq::get(&url)
            .set("authorization", &format!("Bearer {token}"))
            .timeout(Duration::from_secs(15))
            .call();
        let body = match resp {
            Ok(r) if r.status() == 200 => r.into_string().map_err(|e| {
                SnapshotError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("GCS head-metadata {object}: read body: {e}"),
                ))
            })?,
            Ok(r) if r.status() == 404 => {
                return Err(SnapshotError::NotFound(format!("{sandbox_id}/{file}")));
            }
            Ok(r) => {
                return Err(SnapshotError::InvalidArtifact(format!(
                    "GCS head-metadata {object}: status {}, body={}",
                    r.status(),
                    r.into_string().unwrap_or_default()
                )));
            }
            Err(ureq::Error::Status(404, _)) => {
                return Err(SnapshotError::NotFound(format!("{sandbox_id}/{file}")));
            }
            Err(ureq::Error::Status(code, r)) => {
                return Err(SnapshotError::InvalidArtifact(format!(
                    "GCS head-metadata {object}: status {code}, body={}",
                    r.into_string().unwrap_or_default()
                )));
            }
            Err(e) => {
                return Err(SnapshotError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("GCS head-metadata {object}: {e}"),
                )));
            }
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            SnapshotError::InvalidArtifact(format!(
                "GCS head-metadata {object}: parse body: {e}"
            ))
        })?;
        Ok(parsed
            .get("metadata")
            .and_then(|m| m.get(meta_key))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()))
    }
}

/// Re-export of the canonical hash helper from the parent module.
/// Lives here as a thin private wrapper so the GCS impl can compute
/// the canonical-artifact hash post-download without exposing the
/// helper across crates.
fn canonical_artifact_sha256(dir: &Path) -> Result<([u8; 32], u64), SnapshotError> {
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    for &name in ARTIFACT_FILES {
        let path = dir.join(name);
        let len = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SnapshotError::MissingFile(name));
            }
            Err(e) => return Err(SnapshotError::Io(e)),
        };
        hasher.update(name.as_bytes());
        hasher.update(len.to_be_bytes());
        // R11-P3 (perf-r11, sibling of R5-P1 77ea717f): 1 MiB
        // BufReader collapses the 16× syscall amplification of the
        // unbuffered 64 KiB loop. Mirrors snapshot_store::compute_
        // artifact_sha256 byte-for-byte (same canonical hash domain).
        let f = std::fs::File::open(&path)?;
        let mut reader = std::io::BufReader::with_capacity(1 << 20, f);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        total = total
            .checked_add(len)
            .ok_or_else(|| SnapshotError::InvalidArtifact("size overflow".into()))?;
    }
    let h = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&h);
    Ok((out, total))
}

// ────────────────────────────────────────────────────────────────────
// Tiered store
// ────────────────────────────────────────────────────────────────────

/// Two-tier snapshot store. L1 = local disk per worker; L2 = GCS
/// (object storage). All hot-path reads hit L1; L2 is consulted only
/// when L1 misses (e.g. takeover lands the row on a different
/// worker).
///
/// **Type parameters** so callers can swap in their own L2 (e.g.
/// `MockL2` for tests). Production wires
/// `TieredSnapshotStore<LocalDiskSnapshotStore, GcsSnapshotStore>`.
pub struct TieredSnapshotStore<L1, L2> {
    pub l1: L1,
    pub l2: std::sync::Arc<L2>,
}

impl<L1: std::fmt::Debug, L2: std::fmt::Debug> std::fmt::Debug
    for TieredSnapshotStore<L1, L2>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TieredSnapshotStore")
            .field("l1", &self.l1)
            .field("l2", &self.l2)
            .finish()
    }
}

impl<L1, L2> TieredSnapshotStore<L1, L2> {
    pub fn new(l1: L1, l2: L2) -> Self {
        Self { l1, l2: std::sync::Arc::new(l2) }
    }
}

impl<L1, L2> SnapshotStore for TieredSnapshotStore<L1, L2>
where
    L1: SnapshotStore,
    L2: SnapshotStore + 'static,
{
    fn put(
        &self,
        sandbox_id: &str,
        source_dir: &Path,
        ch_version: &str,
    ) -> Result<SnapshotMetadata, SnapshotError> {
        // L1 is authoritative on the synchronous path: any failure
        // here aborts the snapshot. The artifact lands on local disk
        // first so the source-VM teardown can proceed without
        // waiting on the GCS upload.
        let meta = self.l1.put(sandbox_id, source_dir, ch_version)?;

        // Background L2 upload, fire-and-forget. v1 stub: this will
        // log a warn and exit. **TODO (GCS PR):** add a retry loop
        // (capped exponential backoff, max ~5 min) and a metric for
        // l2_upload_pending / l2_upload_failed_total so operators
        // see the L2-lag during a worker drain.
        //
        // We can't `clone` arbitrary L2; require Arc-shareable above.
        let l2 = self.l2.clone();
        let sandbox_id = sandbox_id.to_string();
        // L2 stub doesn't need source_dir (it errors anyway), but
        // the real impl will need the artifact bytes. For the stub
        // we pass a non-existent path; real impl will pass the L1
        // artifact_path so it streams from local disk → GCS.
        let artifact_path = std::path::PathBuf::from(meta.artifact_path.clone());
        let ch_version_owned = ch_version.to_string();
        let sha256 = meta.sha256;
        compio::runtime::spawn_blocking(move || {
            match l2.put(&sandbox_id, &artifact_path, &ch_version_owned) {
                Ok(m) => {
                    if m.sha256 != sha256 {
                        tracing::warn!(
                            sandbox_id = %sandbox_id,
                            "tiered: L2 returned a different sha256; possible re-encrypt drift"
                        );
                    } else {
                        tracing::debug!(
                            sandbox_id = %sandbox_id,
                            "tiered: L2 upload completed"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        sandbox_id = %sandbox_id,
                        error = %e,
                        "tiered: L2 upload failed (v1 fire-and-forget; GCS PR adds retry)"
                    );
                }
            }
        })
        .detach();

        Ok(meta)
    }

    fn get(
        &self,
        sandbox_id: &str,
        target_dir: &Path,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        // L1 first.
        match self.l1.get(sandbox_id, target_dir, expected_sha256) {
            Ok(()) => return Ok(()),
            Err(SnapshotError::NotFound(_)) => {
                // Fall through to L2.
            }
            Err(other) => {
                // L1 had something but it was corrupt / IO-broken —
                // return that error directly. We don't fall back to
                // L2 on integrity errors because the operator wants
                // to know about the corrupt L1 artifact (don't paper
                // over with a silent re-fetch).
                return Err(other);
            }
        }
        // L2 (stub returns NotFound; real impl talks to GCS).
        // **v1**: no L1 side-effect after a successful L2 fetch; the
        // restore handler will re-snapshot at the next idle eviction
        // and that will re-populate L1. Real GCS PR adds a "restore-
        // back-fill" op so L1 caches the artifact for the next
        // restore on the same worker.
        self.l2.get(sandbox_id, target_dir, expected_sha256)
    }

    fn delete(&self, sandbox_id: &str) -> Result<(), SnapshotError> {
        // Best-effort delete from both. Operationally we want
        // delete to *succeed* whenever at least one tier succeeded;
        // a hard fail on both surfaces a real error.
        let l1_err = self.l1.delete(sandbox_id).err();
        let l2_err = self.l2.delete(sandbox_id).err();
        match (l1_err, l2_err) {
            (None, None) => Ok(()),
            (Some(e1), Some(e2)) => Err(SnapshotError::InvalidArtifact(format!(
                "tiered delete: L1 failed ({e1}); L2 failed ({e2})"
            ))),
            (Some(e), None) | (None, Some(e)) => {
                // One tier failed, the other succeeded — log and
                // succeed. Operational tolerance: the artifact is
                // gone from at least one tier, and the operator can
                // grep the warn for the survivor.
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    error = %e,
                    "tiered delete: one tier failed; treating as success"
                );
                Ok(())
            }
        }
    }

    fn verify(
        &self,
        sandbox_id: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        match self.l1.verify(sandbox_id, expected_sha256) {
            Ok(()) => Ok(()),
            Err(SnapshotError::NotFound(_)) => self.l2.verify(sandbox_id, expected_sha256),
            Err(other) => Err(other),
        }
    }

    fn verify_metadata_only(
        &self,
        sandbox_id: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        // L1 first. L1's `verify_metadata_only` defaults to deep
        // `verify` (no side-channel metadata on local filesystem),
        // which is already cheap (~ 3 s for 1 GB on NVMe). The win
        // lives at L2: when L1 has been evicted, the sweep would
        // otherwise pay ~ 1 GB GCS egress per cycle. The GCS impl's
        // `verify_metadata_only` collapses that to one ~ 100-byte
        // JSON-API request.
        match self.l1.verify_metadata_only(sandbox_id, expected_sha256) {
            Ok(()) => Ok(()),
            Err(SnapshotError::NotFound(_)) => {
                self.l2.verify_metadata_only(sandbox_id, expected_sha256)
            }
            Err(other) => Err(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot_store::{LocalDiskSnapshotStore, ARTIFACT_FILES};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn fresh_root() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "zsbx-tiered-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn cleanup(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    fn write_fake_artifact(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for &name in ARTIFACT_FILES {
            let mut f = std::fs::File::create(dir.join(name)).unwrap();
            f.write_all(format!("body-{name}").as_bytes()).unwrap();
        }
    }

    /// `MockL2` records every call so tests can assert on whether
    /// L2 was consulted. Always-fails on `put` (so the background
    /// task exits via the warn log) and `get`/`verify`.
    #[derive(Default)]
    struct MockL2 {
        put_calls: Mutex<u32>,
        get_calls: Mutex<u32>,
        delete_calls: Mutex<u32>,
        verify_calls: Mutex<u32>,
        delete_succeeds: bool,
    }
    impl SnapshotStore for MockL2 {
        fn put(
            &self,
            _sandbox_id: &str,
            _source_dir: &Path,
            _ch_version: &str,
        ) -> Result<SnapshotMetadata, SnapshotError> {
            *self.put_calls.lock().unwrap() += 1;
            Err(SnapshotError::InvalidArtifact("mock L2: always fails put".into()))
        }
        fn get(
            &self,
            sandbox_id: &str,
            _target_dir: &Path,
            _expected_sha256: &[u8; 32],
        ) -> Result<(), SnapshotError> {
            *self.get_calls.lock().unwrap() += 1;
            Err(SnapshotError::NotFound(format!("mock L2: {sandbox_id}")))
        }
        fn delete(&self, _sandbox_id: &str) -> Result<(), SnapshotError> {
            *self.delete_calls.lock().unwrap() += 1;
            if self.delete_succeeds {
                Ok(())
            } else {
                Err(SnapshotError::InvalidArtifact("mock L2: delete fails".into()))
            }
        }
        fn verify(
            &self,
            sandbox_id: &str,
            _expected_sha256: &[u8; 32],
        ) -> Result<(), SnapshotError> {
            *self.verify_calls.lock().unwrap() += 1;
            Err(SnapshotError::NotFound(format!("mock L2: {sandbox_id}")))
        }
    }

    #[compio::test]
    async fn put_writes_to_l1_and_get_short_circuits_l2() {
        let root = fresh_root();
        let l1 = LocalDiskSnapshotStore::new(root.join("store"));
        let l2 = MockL2::default();
        let tier = TieredSnapshotStore::new(l1, l2);

        let src = root.join("src");
        write_fake_artifact(&src);
        let meta = tier.put("sbx_l1_hit", &src, "v51.1").unwrap();
        assert!(meta.bytes > 0);

        let target = root.join("target");
        tier.get("sbx_l1_hit", &target, &meta.sha256).unwrap();
        // L2 should NOT have been consulted on get (L1 had it).
        // (L2 *might* have been consulted on the spawned put task,
        // which is racy — assert specifically on get_calls.)
        assert_eq!(*tier.l2.get_calls.lock().unwrap(), 0, "L2 must not be consulted on L1 hit");

        cleanup(&root);
    }

    #[compio::test]
    async fn get_misses_l1_consults_l2_and_returns_not_found() {
        let root = fresh_root();
        let l1 = LocalDiskSnapshotStore::new(root.join("store"));
        let l2 = MockL2::default();
        let tier = TieredSnapshotStore::new(l1, l2);

        let target = root.join("target");
        let err = tier
            .get("sbx_unknown", &target, &[0u8; 32])
            .unwrap_err();
        assert!(matches!(err, SnapshotError::NotFound(_)), "expected NotFound, got {err:?}");
        // L2 should have been consulted exactly once.
        assert_eq!(*tier.l2.get_calls.lock().unwrap(), 1);

        cleanup(&root);
    }

    #[compio::test]
    async fn put_succeeds_even_when_l2_always_fails() {
        // Background L2 upload is fire-and-forget; put returns OK
        // on L1 success.
        let root = fresh_root();
        let l1 = LocalDiskSnapshotStore::new(root.join("store"));
        let l2 = MockL2::default();
        let tier = TieredSnapshotStore::new(l1, l2);

        let src = root.join("src");
        write_fake_artifact(&src);
        let meta = tier.put("sbx_l2_fails", &src, "v51.1").unwrap();
        assert!(meta.bytes > 0);

        cleanup(&root);
    }

    #[compio::test]
    async fn delete_returns_ok_when_one_side_fails() {
        // Operational tolerance: artifact gone from at least one
        // tier → succeed.
        let root = fresh_root();
        let l1 = LocalDiskSnapshotStore::new(root.join("store"));
        let l2 = MockL2 { delete_succeeds: false, ..Default::default() };
        let tier = TieredSnapshotStore::new(l1, l2);

        let src = root.join("src");
        write_fake_artifact(&src);
        let _ = tier.put("sbx_del_one_fails", &src, "v51.1").unwrap();

        // L1 will succeed (idempotent); L2 will error. Tier
        // returns Ok.
        tier.delete("sbx_del_one_fails").unwrap();

        cleanup(&root);
    }

    #[compio::test]
    async fn delete_returns_err_when_both_sides_fail() {
        // Construct an L1 that errors on delete by deleting the
        // root from under it. Easier path: use a non-existent root
        // and test L2-only failure. Simpler: skip — `LocalDiskSnapshotStore`
        // is idempotent on missing artifacts. Instead, use a
        // `BothFailL1` mock + `MockL2` to force the dual-failure.
        struct BothFailL1;
        impl SnapshotStore for BothFailL1 {
            fn put(&self, _: &str, _: &Path, _: &str) -> Result<SnapshotMetadata, SnapshotError> {
                unreachable!()
            }
            fn get(&self, _: &str, _: &Path, _: &[u8; 32]) -> Result<(), SnapshotError> {
                unreachable!()
            }
            fn delete(&self, _: &str) -> Result<(), SnapshotError> {
                Err(SnapshotError::InvalidArtifact("L1 fail".into()))
            }
            fn verify(&self, _: &str, _: &[u8; 32]) -> Result<(), SnapshotError> {
                unreachable!()
            }
        }

        let l2 = MockL2 { delete_succeeds: false, ..Default::default() };
        let tier = TieredSnapshotStore::new(BothFailL1, l2);
        let err = tier.delete("sbx_both_fail").unwrap_err();
        assert!(
            matches!(err, SnapshotError::InvalidArtifact(_)),
            "expected dual-failure error, got {err:?}"
        );
    }

    #[compio::test]
    async fn local_disk_l1_with_gcs_l2_stub_round_trips_via_l1() {
        // Composition test: production-shaped types.
        let root = fresh_root();
        let l1 = LocalDiskSnapshotStore::new(root.join("store"));
        let l2 = GcsSnapshotStore::new("zeroship-snapshots-test", "stub-sa");
        let tier = TieredSnapshotStore::new(l1, l2);

        let src = root.join("src");
        write_fake_artifact(&src);
        let meta = tier.put("sbx_real_compose", &src, "v51.1").unwrap();

        let target = root.join("target");
        tier.get("sbx_real_compose", &target, &meta.sha256).unwrap();
        for &name in ARTIFACT_FILES {
            assert!(target.join(name).is_file(), "{name} missing");
        }

        cleanup(&root);
    }

    #[test]
    fn gcs_object_prefix_is_versioned() {
        let g = GcsSnapshotStore::new("b", "sa");
        let p = g.object_prefix("sbx_xyz");
        assert!(p.starts_with("snapshots/v1/"));
        assert!(p.ends_with('/'));
    }

    #[test]
    fn urlencoding_handles_slashes_and_specials() {
        // GCS object keys carry slashes; ensure they're percent-
        // encoded so the path component of the URL is unambiguous.
        assert_eq!(super::urlencoding("a/b"), "a%2Fb");
        assert_eq!(super::urlencoding("snapshots/v1/sbx_x/config.json"),
                   "snapshots%2Fv1%2Fsbx_x%2Fconfig.json");
        // Unreserved bytes pass through.
        assert_eq!(super::urlencoding("abc-123_xyz.~"), "abc-123_xyz.~");
    }

    /// `verify` must reject a substituted payload. We can't stand up
    /// a real GCS server in unit tests, but the integrity gate lives
    /// in the pure-Rust helper `verify_canonical_sha256_from_streams`
    /// — the GCS impl is just a transport adapter that hands it
    /// (len, reader) pairs. So we exercise the helper directly with
    /// two scenarios:
    ///
    ///   1. Honest payload + matching expected_sha256 → Ok.
    ///   2. Honest payload + an attacker-substituted expected_sha256
    ///      (or equivalently: tampered bytes against the honest hash)
    ///      → ChecksumMismatch.
    ///
    /// Closes A2 from sandbox-snapshot-restore-deferred.md:
    /// `GcsSnapshotStore::verify` previously discarded `expected_
    /// sha256`, making the integrity gate a no-op.
    #[test]
    fn gcs_verify_rejects_tampered_payload() {
        // Honest payload, canonical-hashed as L1 does.
        let bodies: std::collections::HashMap<&'static str, Vec<u8>> = ARTIFACT_FILES
            .iter()
            .map(|&n| (n, format!("honest-bytes-of-{n}").into_bytes()))
            .collect();

        // Compute the *true* canonical hash over the honest payload
        // so we have something legitimate to substitute against.
        let mut hasher = sha2::Sha256::new();
        for &name in ARTIFACT_FILES {
            let b = &bodies[name];
            hasher.update(name.as_bytes());
            hasher.update((b.len() as u64).to_be_bytes());
            hasher.update(b);
        }
        let honest_hash: [u8; 32] = {
            let mut o = [0u8; 32];
            o.copy_from_slice(&hasher.finalize());
            o
        };

        // 1. Honest read with honest expected_sha256 → Ok.
        let result_ok = verify_canonical_sha256_from_streams(&honest_hash, |name| {
            let bytes = bodies[name].clone();
            let len = bytes.len() as u64;
            Ok((len, Box::new(std::io::Cursor::new(bytes)) as Box<dyn Read + Send>))
        });
        assert!(result_ok.is_ok(), "honest verify should pass: {result_ok:?}");

        // 2. Attacker substitutes the snapshot body in GCS — we model
        // this by feeding `verify` a *different* payload than the one
        // the metadata row (`expected_sha256` = honest_hash) was
        // computed over. The integrity gate must reject.
        let tampered: std::collections::HashMap<&'static str, Vec<u8>> = ARTIFACT_FILES
            .iter()
            .map(|&n| (n, format!("EVIL-SUBSTITUTED-{n}").into_bytes()))
            .collect();
        let result_bad = verify_canonical_sha256_from_streams(&honest_hash, |name| {
            let bytes = tampered[name].clone();
            let len = bytes.len() as u64;
            Ok((len, Box::new(std::io::Cursor::new(bytes)) as Box<dyn Read + Send>))
        });
        match result_bad {
            Err(SnapshotError::ChecksumMismatch { expected, actual }) => {
                assert_eq!(expected, honest_hash);
                assert_ne!(actual, honest_hash, "tampered hash must differ");
            }
            other => panic!("expected ChecksumMismatch on tampered payload, got {other:?}"),
        }

        // 3. Truncation attack: declared content-length larger than
        // the body the reader yields. Must surface as an integrity
        // error (we treat short reads as InvalidArtifact, not a
        // silent "almost matched").
        let result_trunc = verify_canonical_sha256_from_streams(&honest_hash, |name| {
            let bytes = bodies[name].clone();
            // Lie about the length — claim 1 more byte than we serve.
            let len = bytes.len() as u64 + 1;
            Ok((len, Box::new(std::io::Cursor::new(bytes)) as Box<dyn Read + Send>))
        });
        assert!(
            matches!(result_trunc, Err(SnapshotError::InvalidArtifact(_))),
            "expected InvalidArtifact on short read, got {result_trunc:?}"
        );
    }

    /// Live GCS round-trip — gated on `GCS_TEST_BUCKET` env var.
    /// Requires the test runner's process to have GCE metadata
    /// server access (running on a GCE VM with a service account
    /// that has `roles/storage.objectAdmin` on the bucket). Skipped
    /// in CI; intended as a one-shot smoke test from the dev host
    /// with `GCS_TEST_BUCKET=suger-dev-zsbx-snapshots-v1 cargo test
    /// -p zeroship-sandbox --lib gcs_live_round_trip -- --ignored`.
    #[test]
    #[ignore = "needs GCS_TEST_BUCKET env + GCE metadata server"]
    fn gcs_live_round_trip() {
        let bucket = match std::env::var("GCS_TEST_BUCKET") {
            Ok(b) if !b.is_empty() => b,
            _ => {
                eprintln!("GCS_TEST_BUCKET unset; skipping");
                return;
            }
        };
        let store = GcsSnapshotStore::new(bucket, "default");
        let root = fresh_root();
        let src = root.join("src");
        write_fake_artifact(&src);
        let sid = format!("__snapshot_test__/{}", uuid::Uuid::now_v7().simple());

        // put: should succeed against the real bucket.
        let meta = store.put(&sid, &src, "v51.1").expect("put must succeed");
        // get: round-trip back to a target dir; sha256 must match.
        let target = root.join("target");
        store.get(&sid, &target, &meta.sha256).expect("get must succeed");
        for &name in ARTIFACT_FILES {
            assert!(target.join(name).is_file(), "{name} must round-trip");
        }
        // verify: re-streams artifact and recomputes canonical sha256.
        store.verify(&sid, &meta.sha256).expect("verify must succeed");
        // verify_metadata_only: HEAD-only fast-path — no body egress.
        store
            .verify_metadata_only(&sid, &meta.sha256)
            .expect("verify_metadata_only must succeed");
        // delete: idempotent.
        store.delete(&sid).expect("delete must succeed");
        store.delete(&sid).expect("delete second time must succeed");

        cleanup(&root);
    }

    /// A2b: `verify_metadata_only` must read the canonical-hash
    /// custom metadata, reject mismatches, and reject missing/
    /// malformed metadata — all without touching the artifact body.
    ///
    /// Exercises [`verify_metadata_canonical_sha256`] (the pure
    /// helper) directly. The GCS impl is a transport adapter that
    /// hands it a `(key) -> Option<String>` callback, so this test
    /// reflects production semantics 1:1 — same way the existing
    /// `gcs_verify_rejects_tampered_payload` test covers the deep
    /// verify path.
    #[test]
    fn gcs_verify_metadata_only_fast_path_semantics() {
        // Canonical hash the bucket claims to hold (matches `put`-
        // time stamp on `state.json`).
        let honest_hash: [u8; 32] = {
            let mut h = [0u8; 32];
            for (i, b) in h.iter_mut().enumerate() {
                *b = (0xa0u8 ^ i as u8).wrapping_mul(17);
            }
            h
        };
        let honest_hex = hex::encode(honest_hash);

        // 1. Metadata-present + matches expected → Ok. Zero body
        //    reads (the callback is the only side effect; we
        //    assert call count after).
        let mut calls = 0u32;
        let result_ok = verify_metadata_canonical_sha256(&honest_hash, |key| {
            calls += 1;
            assert_eq!(key, CANONICAL_SHA_METADATA_KEY);
            Ok(Some(honest_hex.clone()))
        });
        assert!(result_ok.is_ok(), "honest fast-path verify must pass: {result_ok:?}");
        assert_eq!(calls, 1, "fast-path must issue exactly one metadata fetch");

        // 2. Metadata-present but mismatches → ChecksumMismatch.
        //    Caller's `expected_sha256` is `honest_hash`; bucket
        //    claims a different hash (mimics body+meta corruption
        //    where the operator-recorded canonical hash and the
        //    bucket-stamped hash diverged).
        let tampered_hex = {
            let mut h = honest_hash;
            h[0] ^= 0xff; // flip a byte
            hex::encode(h)
        };
        let result_bad = verify_metadata_canonical_sha256(&honest_hash, |_key| {
            Ok(Some(tampered_hex.clone()))
        });
        match result_bad {
            Err(SnapshotError::ChecksumMismatch { expected, actual }) => {
                assert_eq!(expected, honest_hash);
                assert_ne!(actual, honest_hash, "tampered metadata must differ");
            }
            other => panic!("expected ChecksumMismatch on tampered metadata, got {other:?}"),
        }

        // 3. Metadata absent → InvalidArtifact. Sweep cannot
        //    authenticate via fast-path; operator must run deep
        //    `verify` to either confirm corruption or re-stamp.
        let result_missing = verify_metadata_canonical_sha256(&honest_hash, |_key| Ok(None));
        assert!(
            matches!(result_missing, Err(SnapshotError::InvalidArtifact(ref m))
                if m.contains("missing x-goog-meta-")),
            "expected InvalidArtifact(missing) on absent metadata, got {result_missing:?}"
        );

        // 4. Metadata present but not valid hex → InvalidArtifact.
        let result_garbage = verify_metadata_canonical_sha256(&honest_hash, |_key| {
            Ok(Some("not-hex-bytes!".to_string()))
        });
        assert!(
            matches!(result_garbage, Err(SnapshotError::InvalidArtifact(ref m))
                if m.contains("not hex")),
            "expected InvalidArtifact(not hex) on garbage metadata, got {result_garbage:?}"
        );

        // 5. Metadata present + valid hex but wrong length →
        //    InvalidArtifact. Bucket-side stamp got truncated.
        let result_short = verify_metadata_canonical_sha256(&honest_hash, |_key| {
            Ok(Some(hex::encode([0u8; 16]))) // 16 bytes, not 32
        });
        assert!(
            matches!(result_short, Err(SnapshotError::InvalidArtifact(ref m))
                if m.contains("wrong length")),
            "expected InvalidArtifact(wrong length) on short hash, got {result_short:?}"
        );

        // 6. Transport error from the fetch callback bubbles up
        //    unchanged (the callback owns the retry policy; the
        //    verify helper is pure logic on the value).
        let result_io = verify_metadata_canonical_sha256(&honest_hash, |_key| {
            Err(SnapshotError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "simulated network blip",
            )))
        });
        assert!(
            matches!(result_io, Err(SnapshotError::Io(_))),
            "transport errors must propagate, got {result_io:?}"
        );
    }
}
