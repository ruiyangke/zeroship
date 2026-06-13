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

    /// The streaming body of `put_blob_stream`: read the source in `PART_SIZE`
    /// chunks (hashing the whole object), flush full parts, verify the content
    /// address, then either single-PUT or complete the multipart. The live
    /// upload id is mirrored into `guard` the instant it is created (so a
    /// drop-cancel arms the orphan-cleanup path) and the guard is disarmed on a
    /// clean complete. No abort happens here — the caller owns the explicit
    /// error-path abort.
    ///
    /// ## Bounded-concurrency part uploads + content-address integrity
    ///
    /// The source is read STRICTLY SEQUENTIALLY, and the whole-object SHA-256 is
    /// updated in that READ order — independent of upload completion order — so
    /// the content-address check stays exactly correct even though parts upload
    /// concurrently. Each full `PART_SIZE` part's `UploadPart` PUT is dispatched
    /// as a concurrent future; at most [`crate::limits::upload_concurrency`] run
    /// at once (await one before dispatching the next → memory bounded by
    /// `N × PART_SIZE`). All in-flight uploads are drained, and the full-stream
    /// hash is verified against the caller's content address, BEFORE
    /// `complete_multipart`; a mismatch (or any in-flight PUT error) propagates
    /// without completing, and the caller's error path aborts the multipart so
    /// nothing is ever committed under the wrong key.
    #[allow(clippy::too_many_lines)] // single-pass stream → hash → concurrent parts → complete
    #[allow(clippy::future_not_send)] // BlobStore is (?Send); reader is !Send by design
    async fn put_blob_stream_inner(
        &self,
        hash: &str,
        key: &str,
        expected_size: u64,
        reader: &mut dyn std::io::Read,
        guard: &compio_s3::MultipartGuard,
    ) -> Result<PutOutcome, BlobError> {
        use futures::stream::FuturesUnordered;
        use futures::StreamExt;

        let concurrency = crate::limits::upload_concurrency();

        let mut hasher = sha2::Sha256::new();
        let mut total: u64 = 0;
        let mut parts: Vec<compio_s3::PartETag> = Vec::new();
        let mut part_number: u32 = 0;

        // Working copy of the live upload id, mirrored into `guard` (the
        // drop-safety observer) the instant it is created. `None` until the
        // first part flush lazily creates the multipart.
        let mut upload_id: Option<UploadId> = None;

        // ONE pooled HTTP client shared across this multipart session's part
        // uploads AND its finalization. Concurrent parts reuse its kept-alive
        // connections instead of each opening a fresh one — a fresh-client-
        // per-part fan-out floods the host with TIME_WAIT sockets and trips
        // transient connect failures. The trailing `CompleteMultipartUpload`
        // also runs on this warm session (`complete_multipart_on`) so it does
        // not pay the cold-connect latency a brand-new connection costs right
        // after a long upload. The client never outlives this call, so the
        // per-thread invariant holds.
        let session_client = self.client.open_upload_session();

        // In-flight part-upload futures, at most `concurrency` live at once.
        // These are PLAIN (un-spawned) futures: dropping the set on the error
        // path is then a safe cancellation of ordinary futures (spawning +
        // drop-cancelling a compio task with an in-flight io_uring op corrupts
        // the runtime). The source here is a SYNCHRONOUS `std::io::Read` (a
        // deploy artifact on local disk / in memory): a `read()` returns
        // immediately, so — unlike a slow async producer — it cannot park this
        // loop and starve the in-flight PUTs between drains. The
        // `plugin-storage` `S3::put_stream` path (a slow V8 `ReadableStream`)
        // therefore `select!`s its async producer against the in-flight drain
        // for true overlap (HIGH-2); here the read is fast and the
        // gate/backpressure drain suffices, so a `select!` over a synchronous
        // read (which cannot yield to the executor) would add nothing. The set
        // caps at `concurrency`, so memory stays bounded by
        // `concurrency × PART_SIZE`.
        let mut inflight = FuturesUnordered::new();

        // Read the source in bounded chunks; accumulate into the current part
        // buffer and dispatch whole PART_SIZE parts as they fill.
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
            // Hash in READ order — the content address is independent of upload
            // completion order.
            hasher.update(&scratch[..n]);
            part_buf.extend_from_slice(&scratch[..n]);

            while part_buf.len() >= PART_SIZE {
                // Lazily create the multipart upload on the first flush.
                if upload_id.is_none() {
                    let id = self
                        .client
                        .create_multipart(key, "application/octet-stream")
                        .await
                        .map_err(|e| map_s3(hash, e))?;
                    // Arm the drop-safety guard the INSTANT the id exists.
                    guard.set(id.clone());
                    upload_id = Some(id);
                }
                // Backpressure: keep at most `concurrency` PUTs in flight, so
                // live part memory is bounded by `concurrency × PART_SIZE`.
                // Awaiting one before dispatching the next also drives the
                // in-flight set forward.
                while inflight.len() >= concurrency {
                    match inflight.next().await {
                        Some(res) => parts.push(res?),
                        None => break,
                    }
                }
                let id = upload_id.as_ref().expect("multipart created");
                // Copy exactly PART_SIZE into a tight buffer rather than
                // `split_off`-ing the (possibly 16 MiB-grown) `part_buf`: the
                // part `Bytes` is held through the PUT + every retry, so an
                // over-capacity allocation would pin ~2× the intended memory.
                let body = Bytes::copy_from_slice(&part_buf[..PART_SIZE]);
                part_buf.drain(..PART_SIZE);
                part_number += 1;
                inflight.push(self.upload_part_owned(
                    &session_client,
                    hash,
                    key,
                    id,
                    part_number,
                    body,
                ));
            }
        }

        if total != expected_size {
            return Err(BlobError::Backend(format!(
                "size mismatch: expected {expected_size}, observed {total}"
            )));
        }

        // NB: keep the upload id live through the final part flush + complete
        // so a failure there is still abortable by the caller. Disarm the guard
        // only on a clean complete (the object now exists; aborting would be
        // wrong).
        if let Some(id) = upload_id.as_ref() {
            // Multipart path: dispatch the final (short) part too, then drain
            // every in-flight upload. An error draining drops the rest
            // (cancelling them); the caller aborts the multipart.
            if !part_buf.is_empty() {
                part_number += 1;
                let body = Bytes::from(std::mem::take(&mut part_buf));
                inflight.push(self.upload_part_owned(
                    &session_client,
                    hash,
                    key,
                    id,
                    part_number,
                    body,
                ));
            }
            while let Some(res) = inflight.next().await {
                parts.push(res?);
            }
            // Uploads finish out of order — S3 requires the parts list in
            // ascending part-number order at complete time.
            parts.sort_by_key(|p| p.part_number);

            // Verify the content address (computed in read order) BEFORE
            // completing. A mismatch aborts (caller's error path) so nothing is
            // ever committed under the wrong key.
            let computed = hex::encode(hasher.finalize());
            if computed != hash {
                return Err(BlobError::HashMismatch {
                    expected: hash.to_string(),
                    got: computed,
                });
            }

            // Finalize on the warm session client (no cold post-upload connect).
            self.client
                .complete_multipart_on(&session_client, key, id, &parts)
                .await
                .map_err(|e| map_s3(hash, e))?;
            // Completed — the object exists; disarm the guard so neither the
            // explicit error path nor its Drop aborts the now-live object.
            let _ = guard.take();
            Ok(PutOutcome::Wrote)
        } else {
            // Single-part path: object below PART_SIZE → ordinary PUT.
            // Verify the content address BEFORE committing anything.
            let computed = hex::encode(hasher.finalize());
            if computed != hash {
                return Err(BlobError::HashMismatch {
                    expected: hash.to_string(),
                    got: computed,
                });
            }
            // Durable content-address record. The client ALSO emits
            // `x-amz-checksum-sha256` (base64 body digest) when the provider
            // profile enables checksum mode; the user-meta sha256 here is the
            // hex content address, not trusted as integrity proof (we always
            // re-hash on read).
            let meta: [(&str, String); 1] = [("sha256", hash.to_string())];
            let opts = PutOptions {
                content_type: "application/octet-stream",
                if_none_match: true,
                user_meta: &meta,
                cache_control: None,
            };
            let body = std::mem::take(&mut part_buf);
            match self.client.put(key, &body, opts).await {
                Ok(_) => Ok(PutOutcome::Wrote),
                // A concurrent writer won the conditional create — the object
                // now exists under the SAME content hash, so this is a dedup.
                Err(S3Error::PreconditionFailed) => Ok(PutOutcome::Deduped),
                Err(e) => Err(map_s3(hash, e)),
            }
        }
    }

    /// One concurrent `UploadPart`: an owned, self-contained future (cloned
    /// client + owned `Bytes`) suitable for a `FuturesUnordered`. Errors are
    /// mapped to the `BlobError` channel so the caller need only `?`.
    ///
    /// Bounded retry-with-backoff on *retryable* transport/5xx errors. Part
    /// uploads are idempotent (same `part_number` + bytes), and `Bytes` is
    /// refcounted so retaining the body across attempts is cheap. N concurrent
    /// PUTs churn connections fast enough that transient connect failures
    /// (`hyper` Connect, ephemeral-port/`TIME_WAIT` pressure) are expected;
    /// without this a single blip would abort the whole blob upload.
    #[allow(clippy::future_not_send)] // cyper client is !Send by design (per-thread)
    fn upload_part_owned(
        &self,
        session: &compio_s3::UploadSession,
        hash: &str,
        key: &str,
        id: &UploadId,
        part_number: u32,
        body: Bytes,
    ) -> impl std::future::Future<Output = Result<compio_s3::PartETag, BlobError>> + '_ {
        let s3 = self.client.clone();
        let http = session.clone(); // Arc-cheap; shared pooled connections
        let key = key.to_string();
        let hash = hash.to_string();
        let id = id.clone();
        async move {
            let mut attempt: u32 = 0;
            loop {
                // Attempt 0 reuses the shared pooled session (connection reuse
                // across concurrent parts). A RETRY uses a FRESH session so a
                // prior attempt's possibly-dirty pooled connection (a send
                // future cancelled mid-body) is never reused — cyper's pool is
                // not a proven dirty-connection barrier.
                let fresh;
                let session_ref = if attempt == 0 {
                    &http
                } else {
                    fresh = compio_s3::UploadSession::fresh();
                    &fresh
                };
                match s3
                    .upload_part_on(session_ref, &key, &id, part_number, body.clone())
                    .await
                {
                    Ok(etag) => return Ok(etag),
                    Err(e) if e.is_retryable() && attempt + 1 < UPLOAD_PART_RETRIES => {
                        attempt += 1;
                        compio::time::sleep(upload_retry_backoff(attempt)).await;
                    }
                    Err(e) => return Err(map_s3(&hash, e)),
                }
            }
        }
    }
}

/// Per-part upload attempt budget (1 initial try + retries on retryable
/// transport/5xx errors). Concurrent part PUTs churn connections fast enough
/// that transient connect failures (`hyper` Connect, ephemeral-port/`TIME_WAIT`
/// pressure on a busy host) are expected; a generous bounded retry keeps such
/// blips from aborting a multi-part blob upload.
const UPLOAD_PART_RETRIES: u32 = 8;

/// Capped exponential backoff for a part-upload retry: 100ms, 200ms, 400ms …
/// up to ~2s. Backing off lets the host recycle ephemeral ports / `TIME_WAIT`
/// sockets and lets the endpoint drain its accept backlog before retrying.
fn upload_retry_backoff(attempt: u32) -> std::time::Duration {
    let ms = 100u64.saturating_mul(1u64 << attempt.min(5));
    std::time::Duration::from_millis(ms.min(2000))
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

        // Best-effort drain of any orphan recorded by a PRIOR drop-cancelled
        // upload on this thread. Cheap when the queue is empty (no network).
        self.client.drain_orphaned_uploads().await;

        // The drop-safety guard. The inner fn mirrors the live upload id into
        // it the instant `create_multipart` returns, and disarms it on a clean
        // complete. If THIS future is drop-cancelled mid-flight (wall-timeout
        // cancel, client disconnect, LRU eviction), the guard's sync `Drop`
        // warns + enqueues the orphaned upload for a later async abort — the
        // abort a `Drop` cannot itself perform (C1: no spawn/await in Drop).
        let guard = compio_s3::MultipartGuard::new(key.clone());

        // Run the stream → parts → complete body. On ANY error we must abort
        // the multipart (if one was started) BEFORE propagating, so orphaned
        // (billed) parts are reclaimed deterministically in async context.
        let outcome = self
            .put_blob_stream_inner(hash, &key, expected_size, reader, &guard)
            .await;

        match outcome {
            Ok(out) => Ok(out),
            Err(e) => {
                // Explicit, awaited, best-effort abort. Guaranteed to run in
                // async context (unlike a detached spawn). Its failure must NOT
                // mask the original error (that is what the caller must see),
                // but it MUST be surfaced — a silently-failed abort leaves
                // billed parts behind. `guard.take()` disarms the guard so its
                // Drop does NOT also enqueue this upload.
                if let Some(id) = guard.take() {
                    if let Err(abort_err) = self.client.abort_multipart(&key, &id).await {
                        tracing::warn!(
                            key = %key,
                            upload_id = %id.0,
                            error = %abort_err,
                            "failed to abort multipart upload after an error — parts may be \
                             orphaned (billed) until an S3 lifecycle rule reclaims them",
                        );
                    }
                }
                Err(e)
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
                        // Capped backoff (same shape as the part-upload retry):
                        // don't hammer a 429/503 with back-to-back DELETEs.
                        compio::time::sleep(upload_retry_backoff(attempt)).await;
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
