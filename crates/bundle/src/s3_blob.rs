//! `S3BlobStore` — a [`BlobStore`](crate::blob::BlobStore) over `compio-s3`.
//!
//! Two keyspaces under the configured S3 prefix (the prefix is owned by the
//! `S3Config`; the keys here are *logical* keys handed to the client, which
//! joins them with the prefix):
//!
//! ```text
//! blobs/<sha256>                          content-addressed deploy blobs
//! manifests/<app_id>/<deploy_hash>.json   immutable per-deploy manifests
//! ```
//!
//! ## Streaming + content-addressed integrity
//!
//! The whole point of multipart here is bounded memory: `put_blob_stream`
//! reads the single-pass reader in `PART_SIZE` chunks, feeding each chunk to
//! BOTH a whole-object SHA-256 hasher and the current part buffer. When a
//! part fills it is uploaded (an ordinary buffered `Bytes` PUT, never a
//! streaming request body, so cyper's `Send`-bound streaming body never comes
//! into play). After the reader is exhausted the whole-object hash is verified
//! against the caller's content address BEFORE `complete_multipart`; a
//! mismatch aborts the upload so nothing is ever completed under the wrong
//! key. Objects that fit in a single part skip multipart entirely.
//!
//! S3's own multipart `ETag` is not a usable content hash, so client-side
//! full-stream hashing is the integrity source of truth — the same guarantee
//! `LocalDiskBlobStore` gives, preserved across parts.

use bytes::Bytes;
use compio_s3::{PutOptions, S3Client, S3Config, S3Credentials, S3Error, UploadId};
use sha2::Digest;
use uuid::Uuid;

use crate::blob::{validate_hash_format, BlobError, BlobStore, PutOutcome};
use crate::limits::{MAX_BLOB_BYTES, MAX_MANIFEST_BYTES};

/// Multipart part size. S3's minimum part size is 5 MiB (except the last
/// part); 8 MiB is the default, bounding per-part memory while keeping the
/// part count low for large blobs.
pub const PART_SIZE: usize = 8 * 1024 * 1024;

/// Bounded idempotent retry budget for list/delete during app-manifest purge.
const PURGE_RETRY_ATTEMPTS: u32 = 4;

/// `BlobStore` backed by S3 (or any S3-compatible endpoint) via `compio-s3`.
#[derive(Debug, Clone)]
pub struct S3BlobStore {
    client: S3Client,
}

impl S3BlobStore {
    /// Build a store from a parsed `S3Config` plus resolved credentials.
    #[must_use]
    pub fn new(config: S3Config, credentials: S3Credentials) -> Self {
        Self {
            client: S3Client::new(config, credentials),
        }
    }

    /// Build from an already-constructed client (tests / shared client).
    #[must_use]
    pub const fn from_client(client: S3Client) -> Self {
        Self { client }
    }

    /// Logical key for a content-addressed blob.
    fn blob_key(hash: &str) -> String {
        format!("blobs/{hash}")
    }

    /// Logical key for a per-deploy manifest.
    fn manifest_key(app_id: &Uuid, deploy_hash: &str) -> String {
        format!("manifests/{app_id}/{deploy_hash}.json")
    }

    /// Logical prefix under which an app's manifests live.
    fn manifest_prefix(app_id: &Uuid) -> String {
        format!("manifests/{app_id}/")
    }
}

/// RAII guard that aborts an in-progress multipart upload on drop unless it
/// was explicitly disarmed (by clearing `upload`). S3 bills orphaned parts,
/// so every error/panic path must abort.
struct MultipartGuard<'a> {
    client: &'a S3Client,
    key: &'a str,
    upload: Option<UploadId>,
}

impl Drop for MultipartGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.upload.take() {
            let client = self.client.clone();
            let key = self.key.to_string();
            // Best-effort abort. Detached so `Drop` stays sync; the happy
            // path disarms the guard before drop, so this only fires on
            // error/panic.
            compio::runtime::spawn(async move {
                let _ = client.abort_multipart(&key, &id).await;
            })
            .detach();
        }
    }
}

/// Map a `compio-s3` error into the `BlobStore` taxonomy. `NotFound` is
/// preserved; everything else collapses to `Backend` with the source text so
/// callers keep a faithful diagnostic.
fn map_s3(hash_or_key: &str, e: S3Error) -> BlobError {
    match e {
        S3Error::NotFound => BlobError::NotFound(hash_or_key.to_string()),
        S3Error::Integrity { expected, computed } => BlobError::HashMismatch {
            expected,
            got: computed,
        },
        other => BlobError::Backend(format!("{hash_or_key}: {other}")),
    }
}

#[async_trait::async_trait(?Send)]
impl BlobStore for S3BlobStore {
    async fn get_blob(&self, hash: &str) -> Result<Bytes, BlobError> {
        if !validate_hash_format(hash) {
            return Err(BlobError::Backend(format!(
                "malformed blob hash {hash:?}: expected 64-char lowercase hex"
            )));
        }
        let key = Self::blob_key(hash);
        let (bytes, _meta) = self
            .client
            .get(&key, MAX_BLOB_BYTES)
            .await
            .map_err(|e| map_s3(hash, e))?;
        // Re-verify the content address on the way out — the backend's
        // metadata/checksum is not trusted proof of integrity.
        let actual = crate::blob::sha256_hex(&bytes);
        if actual != hash {
            return Err(BlobError::HashMismatch {
                expected: hash.to_string(),
                got: actual,
            });
        }
        Ok(bytes)
    }

    fn local_path(&self, _hash: &str) -> Option<std::path::PathBuf> {
        // Pure remote backend — gateway hot-path lives in the disk-cache
        // refill (reserve_temp + get_blob_to_file + publish_temp), not here.
        None
    }

    #[allow(clippy::too_many_lines)] // single-pass stream → hash → parts → complete
    async fn put_blob_stream(
        &self,
        hash: &str,
        expected_size: u64,
        reader: &mut dyn std::io::Read,
    ) -> Result<PutOutcome, BlobError> {
        if !validate_hash_format(hash) {
            return Err(BlobError::Backend(format!(
                "malformed blob hash {hash:?}: expected 64-char lowercase hex"
            )));
        }
        if expected_size > MAX_BLOB_BYTES {
            return Err(BlobError::Backend(format!(
                "declared size {expected_size} exceeds MAX_BLOB_BYTES {MAX_BLOB_BYTES}"
            )));
        }
        let key = Self::blob_key(hash);

        // Idempotent dedup: HEAD first. Presence is enough to dedup for the
        // remote store — content-addressing guarantees the stored bytes ARE
        // the bytes for this hash (the original writer verified the
        // whole-object hash before completing). Drain the reader so the
        // caller's stream cursor advances past the entry.
        if self
            .client
            .head_object(&key)
            .await
            .map_err(|e| map_s3(hash, e))?
            .is_some()
        {
            std::io::copy(reader, &mut std::io::sink()).map_err(BlobError::Io)?;
            return Ok(PutOutcome::Deduped);
        }

        // Stream the reader in PART_SIZE chunks, hashing the whole object.
        let mut hasher = sha2::Sha256::new();
        let mut total: u64 = 0;
        let mut parts: Vec<compio_s3::PartETag> = Vec::new();
        let mut upload: Option<UploadId> = None;
        let mut part_number: u32 = 0;

        // RAII guard: abort any started multipart upload on early return /
        // drop. S3 bills orphaned parts, so an abort on every error path is
        // mandatory. The happy path disarms it before drop.
        let mut guard = MultipartGuard {
            client: &self.client,
            key: &key,
            upload: None,
        };

        // Read the source in bounded chunks; accumulate into the current
        // part buffer and flush whole PART_SIZE parts.
        let mut part_buf: Vec<u8> = Vec::with_capacity(PART_SIZE);
        let mut scratch = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut scratch).map_err(BlobError::Io)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > expected_size {
                return Err(BlobError::Backend(format!(
                    "blob exceeds declared size {expected_size}"
                )));
            }
            if total > MAX_BLOB_BYTES {
                return Err(BlobError::Backend(format!(
                    "blob exceeds MAX_BLOB_BYTES {MAX_BLOB_BYTES}"
                )));
            }
            hasher.update(&scratch[..n]);
            part_buf.extend_from_slice(&scratch[..n]);

            while part_buf.len() >= PART_SIZE {
                // Lazily create the multipart upload on the first flush.
                if upload.is_none() {
                    let id = self
                        .client
                        .create_multipart(&key, "application/octet-stream")
                        .await
                        .map_err(|e| map_s3(hash, e))?;
                    upload = Some(id.clone());
                    guard.upload = Some(id);
                }
                let id = upload.as_ref().expect("multipart created");
                let rest = part_buf.split_off(PART_SIZE);
                let body = Bytes::from(std::mem::replace(&mut part_buf, rest));
                part_number += 1;
                let etag = self
                    .client
                    .upload_part(&key, id, part_number, body)
                    .await
                    .map_err(|e| map_s3(hash, e))?;
                parts.push(etag);
            }
        }

        if total != expected_size {
            return Err(BlobError::Backend(format!(
                "size mismatch: expected {expected_size}, observed {total}"
            )));
        }
        // Verify the content address BEFORE committing anything.
        let computed = hex::encode(hasher.finalize());
        if computed != hash {
            return Err(BlobError::HashMismatch {
                expected: hash.to_string(),
                got: computed,
            });
        }

        match upload.take() {
            // Single-part path: object below PART_SIZE → ordinary PUT.
            None => {
                // Durable content-address record. The client ALSO emits
                // `x-amz-checksum-sha256` (base64 body digest) when the
                // provider profile enables checksum mode; the user-meta
                // sha256 here is the hex content address, not trusted as
                // integrity proof (we always re-hash on read).
                let meta: [(&str, String); 1] = [("sha256", hash.to_string())];
                let opts = PutOptions {
                    content_type: "application/octet-stream",
                    if_none_match: true,
                    user_meta: &meta,
                    cache_control: None,
                };
                let body = std::mem::take(&mut part_buf);
                match self.client.put(&key, &body, opts).await {
                    Ok(_) => {
                        guard.upload = None;
                        Ok(PutOutcome::Wrote)
                    }
                    // A concurrent writer won the conditional create — the
                    // object now exists under the SAME content hash, so this
                    // is a dedup, not a failure.
                    Err(S3Error::PreconditionFailed) => {
                        guard.upload = None;
                        Ok(PutOutcome::Deduped)
                    }
                    Err(e) => Err(map_s3(hash, e)),
                }
            }
            // Multipart path: flush the final (short) part, then complete.
            Some(id) => {
                if !part_buf.is_empty() {
                    part_number += 1;
                    let body = Bytes::from(std::mem::take(&mut part_buf));
                    let etag = self
                        .client
                        .upload_part(&key, &id, part_number, body)
                        .await
                        .map_err(|e| map_s3(hash, e))?;
                    parts.push(etag);
                }
                self.client
                    .complete_multipart(&key, &id, &parts)
                    .await
                    .map_err(|e| map_s3(hash, e))?;
                // Completed successfully — disarm the abort guard.
                guard.upload = None;
                Ok(PutOutcome::Wrote)
            }
        }
    }

    async fn has_blob(&self, hash: &str) -> Result<bool, BlobError> {
        if !validate_hash_format(hash) {
            return Ok(false);
        }
        let key = Self::blob_key(hash);
        Ok(self
            .client
            .head_object(&key)
            .await
            .map_err(|e| map_s3(hash, e))?
            .is_some())
    }

    async fn get_blob_to_file(
        &self,
        hash: &str,
        out: &compio::fs::File,
        expected_size: Option<u64>,
        max_bytes: u64,
    ) -> Result<u64, BlobError> {
        if !validate_hash_format(hash) {
            return Err(BlobError::Backend(format!(
                "malformed blob hash {hash:?}: expected 64-char lowercase hex"
            )));
        }
        let key = Self::blob_key(hash);
        let meta = self
            .client
            .get_object_to_file(&key, out, expected_size, max_bytes, Some(hash))
            .await
            .map_err(|e| map_s3(hash, e))?;
        Ok(meta.len)
    }

    async fn put_manifest(
        &self,
        app_id: &Uuid,
        deploy_hash: &str,
        json: &[u8],
    ) -> Result<(), BlobError> {
        if json.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(BlobError::Backend(format!(
                "manifest size {} exceeds MAX_MANIFEST_BYTES {MAX_MANIFEST_BYTES}",
                json.len()
            )));
        }
        let key = Self::manifest_key(app_id, deploy_hash);
        let opts = PutOptions {
            content_type: "application/json",
            if_none_match: true,
            user_meta: &[],
            // The key embeds deploy_hash, so the object is immutable.
            cache_control: Some("public, max-age=31536000, immutable"),
        };
        match self.client.put(&key, json, opts).await {
            Ok(_) => Ok(()),
            // Conditional create lost the race / replay: the key already
            // exists. Resolve by capped GET — identical JSON is success,
            // divergent content is a backend error.
            Err(S3Error::PreconditionFailed | S3Error::Conflict) => {
                let (existing, _meta) = self
                    .client
                    .get(&key, MAX_MANIFEST_BYTES)
                    .await
                    .map_err(|e| map_s3(&key, e))?;
                if existing.as_ref() == json {
                    Ok(())
                } else {
                    Err(BlobError::Backend(format!(
                        "manifest {key} already exists with divergent content"
                    )))
                }
            }
            Err(e) => Err(map_s3(&key, e)),
        }
    }

    async fn get_manifest(
        &self,
        app_id: &Uuid,
        deploy_hash: &str,
    ) -> Result<Bytes, BlobError> {
        let key = Self::manifest_key(app_id, deploy_hash);
        let (bytes, _meta) = self
            .client
            .get(&key, MAX_MANIFEST_BYTES)
            .await
            .map_err(|e| map_s3(&key, e))?;
        Ok(bytes)
    }

    async fn delete_app_manifests(&self, app_id: &Uuid) -> Result<(), BlobError> {
        let prefix = Self::manifest_prefix(app_id);
        // List every manifest object under the app prefix, paging until the
        // listing is exhausted, then delete each with bounded idempotent
        // retries. `S3Client::list` already loops continuation tokens and
        // strips the internal config prefix, returning logical keys.
        let entries = self
            .client
            .list(&prefix)
            .await
            .map_err(|e| map_s3(&prefix, e))?;

        let mut deleted: u64 = 0;
        for entry in entries {
            // `list` strips the config prefix but keeps our logical
            // `manifests/<app_id>/...` key, which is exactly what `delete`
            // expects (it re-applies the config prefix).
            let mut attempt = 0;
            loop {
                match self.client.delete(&entry.key).await {
                    Ok(()) => {
                        deleted += 1;
                        break;
                    }
                    Err(e) if e.is_retryable() && attempt + 1 < PURGE_RETRY_ATTEMPTS => {
                        attempt += 1;
                    }
                    Err(e) => {
                        return Err(BlobError::Backend(format!(
                            "delete_app_manifests({app_id}): deleted {deleted} then failed on \
                             {}: {e}",
                            entry.key
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_key_layout() {
        let h = "a".repeat(64);
        assert_eq!(S3BlobStore::blob_key(&h), format!("blobs/{h}"));
    }

    #[test]
    fn manifest_key_layout() {
        let id = Uuid::nil();
        assert_eq!(
            S3BlobStore::manifest_key(&id, "deadbeef"),
            format!("manifests/{id}/deadbeef.json")
        );
        assert_eq!(
            S3BlobStore::manifest_prefix(&id),
            format!("manifests/{id}/")
        );
    }

    #[test]
    fn map_s3_preserves_not_found() {
        assert!(matches!(
            map_s3("h", S3Error::NotFound),
            BlobError::NotFound(_)
        ));
        assert!(matches!(
            map_s3(
                "h",
                S3Error::Integrity {
                    expected: "a".into(),
                    computed: "b".into()
                }
            ),
            BlobError::HashMismatch { .. }
        ));
        assert!(matches!(
            map_s3("h", S3Error::Conflict),
            BlobError::Backend(_)
        ));
    }
}
