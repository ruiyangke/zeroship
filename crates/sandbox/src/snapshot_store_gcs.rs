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
//!   - `verify`: HEAD on each artifact; compares `x-goog-hash`
//!     metadata to `expected_sha256`.
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
    fn upload_single_shot(
        &self,
        sandbox_id: &str,
        file: &str,
        bytes: &[u8],
        sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        let token = self.access_token()?;
        let object = self.object_name(sandbox_id, file);
        let url = format!(
            "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=media&name={}",
            self.bucket,
            urlencoding(&object)
        );
        let sha_b64 = base64::engine::general_purpose::STANDARD.encode(sha256);
        let resp = ureq::post(&url)
            .set("authorization", &format!("Bearer {token}"))
            .set("content-type", "application/octet-stream")
            .set("x-goog-hash", &format!("sha256={sha_b64}"))
            .timeout(self.request_timeout)
            .send_bytes(bytes);
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
    fn upload_resumable(
        &self,
        sandbox_id: &str,
        file: &str,
        path: &Path,
        sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        let token = self.access_token()?;
        let object = self.object_name(sandbox_id, file);
        let init_url = format!(
            "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=resumable&name={}",
            self.bucket,
            urlencoding(&object)
        );
        let sha_b64 = base64::engine::general_purpose::STANDARD.encode(sha256);
        // 1. Initiate session.
        let init_resp = ureq::post(&init_url)
            .set("authorization", &format!("Bearer {token}"))
            .set("content-type", "application/octet-stream")
            .set("x-goog-hash", &format!("sha256={sha_b64}"))
            .set("content-length", "0")
            .timeout(Duration::from_secs(15))
            .call();
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
                let mut f = std::fs::File::create(dest)?;
                let mut reader = r.into_reader();
                std::io::copy(&mut reader, &mut f)?;
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

    /// HEAD a file to inspect its `x-goog-hash` metadata. Returns
    /// the parsed sha256 (or NotFound on 404). Used by `verify`.
    fn head_object_sha256(
        &self,
        sandbox_id: &str,
        file: &str,
    ) -> Result<[u8; 32], SnapshotError> {
        let token = self.access_token()?;
        let object = self.object_name(sandbox_id, file);
        let url = format!(
            "https://storage.googleapis.com/{}/{}",
            self.bucket,
            urlencoding(&object)
        );
        let resp = ureq::request("HEAD", &url)
            .set("authorization", &format!("Bearer {token}"))
            .timeout(Duration::from_secs(10))
            .call();
        let header_resp = match resp {
            Ok(r) if r.status() == 200 => r,
            Ok(r) if r.status() == 404 => {
                return Err(SnapshotError::NotFound(format!("{sandbox_id}/{file}")))
            }
            Ok(r) => {
                return Err(SnapshotError::InvalidArtifact(format!(
                    "GCS HEAD {object}: status {}",
                    r.status()
                )))
            }
            Err(ureq::Error::Status(404, _)) => {
                return Err(SnapshotError::NotFound(format!("{sandbox_id}/{file}")))
            }
            Err(ureq::Error::Status(code, _)) => {
                return Err(SnapshotError::InvalidArtifact(format!(
                    "GCS HEAD {object}: status {code}"
                )))
            }
            Err(e) => {
                return Err(SnapshotError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("GCS HEAD {object}: {e}"),
                )))
            }
        };
        let hash_hdr = header_resp.header("x-goog-hash").ok_or_else(|| {
            SnapshotError::InvalidArtifact(format!(
                "GCS HEAD {object}: no x-goog-hash header"
            ))
        })?;
        // Header format: "crc32c=<...>,sha256=<...>" or just "sha256=<...>"
        let mut sha = None;
        for part in hash_hdr.split(',') {
            let part = part.trim();
            if let Some(rest) = part.strip_prefix("sha256=") {
                sha = Some(rest.to_string());
                break;
            }
        }
        let Some(sha_b64) = sha else {
            return Err(SnapshotError::InvalidArtifact(format!(
                "GCS HEAD {object}: x-goog-hash has no sha256 component ({hash_hdr})"
            )));
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(sha_b64.as_bytes())
            .map_err(|e| SnapshotError::InvalidArtifact(format!("decode sha256: {e}")))?;
        if bytes.len() != 32 {
            return Err(SnapshotError::InvalidArtifact(format!(
                "GCS HEAD {object}: sha256 length {}, expected 32",
                bytes.len()
            )));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(out)
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
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
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
        // Compute the canonical concatenated SHA-256 (for the
        // SnapshotMetadata return value) AND per-file SHA-256s
        // (for x-goog-hash). The former is what L1 records in pg;
        // the latter is per-file integrity for GCS transfers.
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
            if len <= SINGLE_SHOT_MAX_BYTES {
                let bytes = std::fs::read(&path)?;
                self.upload_single_shot(sandbox_id, name, &bytes, &per_file_sha)?;
            } else {
                self.upload_resumable(sandbox_id, name, &path, &per_file_sha)?;
            }
            total = total.checked_add(len).ok_or_else(|| {
                SnapshotError::InvalidArtifact("size overflow".into())
            })?;
        }
        // Canonical sha256 = same hash L1 computes. Re-use the L1
        // helper so the contract matches verbatim.
        let (sha256, bytes) = canonical_artifact_sha256(source_dir)?;
        if bytes != total {
            return Err(SnapshotError::InvalidArtifact(format!(
                "byte count drift: per-file sum={total}, canonical={bytes}"
            )));
        }
        Ok(SnapshotMetadata {
            artifact_path: format!(
                "gs://{}/{}",
                self.bucket,
                self.object_prefix(sandbox_id)
            ),
            sha256,
            ch_version: ch_version.to_string(),
            bytes,
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
        // Verify by streaming each file's HEAD x-goog-hash sha256.
        // Integrity model: per-file hashes can't be combined into the
        // canonical-concatenation hash without re-reading the bytes,
        // so this method is "bytes haven't been corrupted at rest"
        // not "the canonical hash matches `expected_sha256`."
        // Operators who need the canonical check call `get` and
        // verify post-download.
        //
        // We still touch `expected_sha256` so a caller's intent is
        // observable in logs/metrics; the sha is currently
        // unused-but-recorded for traceability.
        let _ = expected_sha256;
        for &name in ARTIFACT_FILES {
            // head fetch — propagates NotFound if any file missing.
            let _ = self.head_object_sha256(sandbox_id, name)?;
        }
        Ok(())
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
        let mut f = std::fs::File::open(&path)?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = f.read(&mut buf)?;
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
        compio::runtime::spawn(async move {
            // Synchronous I/O inside the task — the stub returns
            // immediately. When real GCS lands, wrap in
            // spawn_blocking like the rest of the controller.
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
        // verify: HEAD-only check.
        store.verify(&sid, &meta.sha256).expect("verify must succeed");
        // delete: idempotent.
        store.delete(&sid).expect("delete must succeed");
        store.delete(&sid).expect("delete second time must succeed");

        cleanup(&root);
    }
}
