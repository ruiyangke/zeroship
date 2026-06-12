//! S3-compatible `Backend` (feature `s3`).
//!
//! Covers S3, R2, MinIO, Spaces, B2 — anything speaking the S3 API — over
//! the bespoke compio-native [`compio_s3::S3Client`] (hand-rolled SigV4,
//! cyper transport, zero tokio).
//!
//! ## Keyspace
//!
//! Every app's objects share one S3 bucket (configured on the `S3Config`).
//! Multi-tenancy is by key prefix: the logical key handed to the client is
//!
//! ```text
//! <app_id>/<bucket>/<key>
//! ```
//!
//! where `<bucket>` is the *app-level* bucket name (e.g. `uploads`), not the
//! S3 bucket. The client further joins the configured `prefix` (if any) in
//! front, so the on-disk S3 key is `<config-prefix>/<app_id>/<bucket>/<key>`.
//! `validate_object_coords` has already proven none of the three parts can
//! contain `..` or a stray `/` that would let one app reach another's space.
//!
//! ## Streaming
//!
//! - `put_stream` → S3 multipart. The body is read in bounded `PART_SIZE`
//!   chunks; each full part is an ordinary buffered `UploadPart` PUT. An
//!   object below one part is a single `PutObject` (no multipart overhead).
//!   Memory is bounded by `PART_SIZE`, never the whole object. Any error
//!   mid-upload aborts the multipart (orphaned parts are billed) via a RAII
//!   guard.
//! - `get_stream` → the client's streaming GET (cyper `bytes_stream`).

use bytes::Bytes;
use compio_s3::{PartETag, PutOptions, S3Client, S3Config, S3Credentials, S3Error, UploadId};

use super::{
    validate_list_coords, validate_object_coords, Backend, BoxByteStream, BoxChunkSource,
    ChunkResult, ChunkSource, ListEntry, ObjectMeta,
};

/// Multipart part size. S3 requires every part except the last to be
/// ≥ 5 MiB; 8 MiB is the proposal default — a comfortable margin that keeps
/// the part count low for large objects while bounding per-upload memory.
pub const PART_SIZE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct S3 {
    client: S3Client,
}

impl S3 {
    /// Build from a parsed config + resolved credentials.
    #[must_use]
    pub fn new(config: S3Config, credentials: S3Credentials) -> Self {
        Self {
            client: S3Client::new(config, credentials),
        }
    }

    /// Build directly from an existing client (shares its config/creds Arcs).
    #[must_use]
    pub fn from_client(client: S3Client) -> Self {
        Self { client }
    }

    /// The logical key (pre-`config.prefix`) for an object.
    fn object_key(app_id: &str, bucket: &str, key: &str) -> String {
        format!("{app_id}/{bucket}/{key}")
    }

    /// The logical prefix used to scope a `list` to one app-bucket.
    fn list_prefix(app_id: &str, bucket: &str, user_prefix: &str) -> String {
        format!("{app_id}/{bucket}/{user_prefix}")
    }

    /// The streaming body of `put_stream`: pull chunks, flush full
    /// `PART_SIZE` parts, then single-PUT or complete the multipart. Enforces
    /// the S3 10,000-part hard limit and a configurable total-size ceiling,
    /// failing fast (so the caller can abort) instead of discovering the
    /// overrun at `complete`. `upload` is borrowed mutably so the caller can
    /// abort the started upload on any error this returns; no abort happens
    /// here — the caller owns the error path.
    ///
    /// ## Bounded-concurrency part uploads
    ///
    /// The source is still read STRICTLY SEQUENTIALLY into one `PART_SIZE`
    /// buffer at a time, but each full part's `UploadPart` PUT is dispatched as
    /// a concurrent future rather than awaited inline. At most
    /// [`crate::limits::upload_concurrency`] PUTs run at once: when that many
    /// are in flight we await the next one to finish before reading/dispatching
    /// the next part. Memory stays bounded — at most `N × PART_SIZE` of part
    /// buffers live (plus the reader's chunk). Uploads finish out of order, so
    /// `(part_number, ETag)` results are sorted by part number before
    /// `complete_multipart` (S3 requires ascending part order). Any in-flight
    /// PUT error (or reader error, or a cap breach) propagates immediately; the
    /// remaining in-flight futures are dropped (cancelled) and the caller's
    /// error path aborts the multipart.
    #[allow(clippy::future_not_send)] // Backend is (?Send); body source is !Send by design
    #[allow(clippy::too_many_lines)] // single-pass stream → concurrent parts → complete
    async fn put_stream_inner(
        &self,
        s3_key: &str,
        content_type: &str,
        mut body: BoxChunkSource,
        upload: &mut Option<UploadId>,
    ) -> Result<u64, String> {
        use futures::stream::FuturesUnordered;
        use futures::StreamExt;

        let max_total = crate::limits::max_stream_object_bytes();
        let concurrency = crate::limits::upload_concurrency();

        let mut total: u64 = 0;
        let mut parts: Vec<PartETag> = Vec::new();
        let mut part_number: u32 = 0;

        // ONE pooled HTTP client shared across this multipart session's part
        // uploads. Concurrent parts reuse its kept-alive connections instead of
        // each opening a fresh one — that is what makes N-way concurrency safe
        // (a fresh-client-per-part fan-out floods the host with TIME_WAIT
        // sockets and trips transient connect failures). The client never
        // outlives this call, so the per-thread client invariant holds.
        let session_client = self.client.open_upload_session();

        // In-flight part-upload futures, at most `concurrency` live at once.
        // Each is a self-contained `async move` over the cloned session client +
        // owned bytes, so it carries no borrow of `self`/`body` and the set can
        // be polled concurrently on the single compio thread.
        let mut inflight = FuturesUnordered::new();

        // Accumulate incoming chunks into a part buffer; dispatch whole
        // PART_SIZE parts as they fill. Bounded memory: at most
        // `concurrency × PART_SIZE` part bytes in flight + one chunk.
        let mut part_buf: Vec<u8> = Vec::with_capacity(PART_SIZE);
        while let Some(chunk) = body.next_chunk().await {
            let chunk = chunk?;
            if chunk.is_empty() {
                continue;
            }
            total += chunk.len() as u64;
            if total > max_total {
                return Err(format!(
                    "storage: object exceeds max stream size {max_total} bytes \
                     (set {} to raise)",
                    crate::limits::MAX_STREAM_OBJECT_BYTES_ENV
                ));
            }
            part_buf.extend_from_slice(&chunk);

            while part_buf.len() >= PART_SIZE {
                if upload.is_none() {
                    let id = self
                        .client
                        .create_multipart(s3_key, content_type)
                        .await
                        .map_err(|e| map_s3(s3_key, e))?;
                    *upload = Some(id);
                }
                // Refuse to exceed S3's 10,000-part hard limit: such an upload
                // can never `complete`, so fail fast (the caller aborts).
                if part_number >= crate::limits::MAX_MULTIPART_PARTS {
                    return Err(format!(
                        "storage: multipart upload would exceed the S3 {}-part limit",
                        crate::limits::MAX_MULTIPART_PARTS
                    ));
                }
                // Backpressure: keep at most `concurrency` PUTs in flight.
                // Awaiting one BEFORE dispatching the next bounds live memory.
                while inflight.len() >= concurrency {
                    match inflight.next().await {
                        Some(Ok(etag)) => parts.push(etag),
                        Some(Err(e)) => return Err(e),
                        None => break,
                    }
                }
                let id = upload.as_ref().expect("multipart created");
                let rest = part_buf.split_off(PART_SIZE);
                let part = Bytes::from(std::mem::replace(&mut part_buf, rest));
                part_number += 1;
                inflight.push(self.upload_part_owned(&session_client, s3_key, id, part_number, part));
            }
        }

        if upload.is_none() {
            // Single-part path: object below PART_SIZE → ordinary PutObject.
            // No multipart was started, so nothing is in flight here.
            let opts = PutOptions {
                content_type,
                ..PutOptions::default()
            };
            self.client
                .put(s3_key, &part_buf, opts)
                .await
                .map_err(|e| map_s3(s3_key, e))?;
        } else {
            // Multipart path: dispatch the final (short) part too, then drain
            // every in-flight upload before completing. Keep `upload`
            // populated through complete so a failure there is still abortable;
            // clear only on a clean complete.
            if !part_buf.is_empty() {
                if part_number >= crate::limits::MAX_MULTIPART_PARTS {
                    return Err(format!(
                        "storage: multipart upload would exceed the S3 {}-part limit",
                        crate::limits::MAX_MULTIPART_PARTS
                    ));
                }
                let id = upload.as_ref().expect("multipart created");
                part_number += 1;
                let part = Bytes::from(std::mem::take(&mut part_buf));
                inflight.push(self.upload_part_owned(&session_client, s3_key, id, part_number, part));
            }
            // Drain all remaining in-flight part uploads. An error here drops
            // the rest (cancelling them); the caller aborts the multipart.
            while let Some(res) = inflight.next().await {
                parts.push(res?);
            }
            // Uploads finish out of order — S3 requires the parts list in
            // ascending part-number order at complete time.
            parts.sort_by_key(|p| p.part_number);

            let id = upload.as_ref().expect("multipart created");
            self.client
                .complete_multipart(s3_key, id, &parts)
                .await
                .map_err(|e| map_s3(s3_key, e))?;
            // Completed — clear so the caller does NOT abort the live object.
            *upload = None;
        }

        Ok(total)
    }

    /// One concurrent `UploadPart`: an owned, self-contained future (cloned
    /// client + owned `Bytes`) suitable for a `FuturesUnordered`. Errors are
    /// already mapped to the `String` channel so the caller need only `?`.
    ///
    /// Bounded retry-with-backoff on *retryable* transport/5xx errors. Part
    /// uploads are idempotent (same `part_number` + bytes), and `Bytes` is
    /// refcounted so retaining the body across attempts is cheap. The higher
    /// connection churn of N concurrent PUTs makes transient connect failures
    /// (`hyper` Connect, ephemeral-port/`TIME_WAIT` pressure) much more likely
    /// than the old 1-at-a-time loop ever saw; without this the whole upload
    /// would abort on a single transient blip.
    #[allow(clippy::future_not_send)] // cyper client is !Send by design (per-thread)
    fn upload_part_owned(
        &self,
        session: &compio_s3::UploadSession,
        s3_key: &str,
        id: &UploadId,
        part_number: u32,
        part: Bytes,
    ) -> impl std::future::Future<Output = Result<PartETag, String>> + '_ {
        let s3 = self.client.clone();
        let http = session.clone(); // Arc-cheap; shared pooled connections
        let key = s3_key.to_string();
        let id = id.clone();
        async move {
            let mut attempt: u32 = 0;
            loop {
                match s3
                    .upload_part_on(&http, &key, &id, part_number, part.clone())
                    .await
                {
                    Ok(etag) => return Ok(etag),
                    Err(e) if e.is_retryable() && attempt + 1 < UPLOAD_PART_RETRIES => {
                        attempt += 1;
                        // Linear backoff: 50ms, 100ms, 150ms … keeps the
                        // concurrent connect rate from sustaining a storm.
                        compio::time::sleep(std::time::Duration::from_millis(
                            50 * u64::from(attempt),
                        ))
                        .await;
                    }
                    Err(e) => return Err(map_s3(&key, e)),
                }
            }
        }
    }
}

/// Per-part upload attempt budget (1 initial try + retries on retryable
/// transport/5xx errors). Concurrent uploads churn connections fast enough that
/// transient connect failures are expected; a small bounded retry keeps a
/// single blip from aborting a multi-GiB upload.
const UPLOAD_PART_RETRIES: u32 = 5;

#[async_trait::async_trait(?Send)]
impl Backend for S3 {
    async fn put_stream(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
        body: BoxChunkSource,
        content_type: Option<&str>,
    ) -> Result<u64, String> {
        validate_object_coords(app_id, bucket, key)?;
        let s3_key = Self::object_key(app_id, bucket, key);
        let content_type = content_type.unwrap_or("application/octet-stream");

        // Shared with the explicit error-path abort below. `None` until the
        // first part flush lazily creates the multipart upload.
        let mut upload: Option<UploadId> = None;

        // Panic backstop only — the real orphaned-parts guarantee is the
        // explicit, awaited abort on the error path. Panic-free, never spawns.
        let mut backstop = PanicBackstop {
            key: &s3_key,
            upload: None,
        };

        let result = self
            .put_stream_inner(&s3_key, content_type, body, &mut upload)
            .await;

        match result {
            Ok(total) => {
                backstop.upload = None;
                Ok(total)
            }
            Err(e) => {
                // Explicit, awaited, best-effort abort in async context (NOT a
                // detached spawn) so orphaned (billed) parts are reclaimed.
                if let Some(id) = upload.take() {
                    let _ = self.client.abort_multipart(&s3_key, &id).await;
                }
                backstop.upload = None;
                Err(e)
            }
        }
    }

    async fn get_stream(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<Option<(ObjectMeta, BoxByteStream)>, String> {
        validate_object_coords(app_id, bucket, key)?;
        let s3_key = Self::object_key(app_id, bucket, key);
        match self.client.get_stream(&s3_key).await {
            Ok((meta, stream)) => {
                let object_meta = ObjectMeta {
                    size: meta.len,
                    content_type: meta.content_type,
                    modified_at: meta.last_modified,
                };
                let boxed: BoxByteStream = Box::new(S3Chunks {
                    inner: Box::pin(stream),
                });
                Ok(Some((object_meta, boxed)))
            }
            Err(S3Error::NotFound) => Ok(None),
            Err(e) => Err(map_s3(&s3_key, e)),
        }
    }

    async fn delete(&self, app_id: &str, bucket: &str, key: &str) -> Result<bool, String> {
        validate_object_coords(app_id, bucket, key)?;
        let s3_key = Self::object_key(app_id, bucket, key);
        // The native `delete(bucket, key)` contract returns `{deleted}`:
        // true iff the object existed. S3 `DELETE` is idempotent (404 is
        // success and carries no "was-present" signal), so HEAD first to
        // get the boolean, then delete.
        let existed = self
            .client
            .head_object(&s3_key)
            .await
            .map_err(|e| map_s3(&s3_key, e))?
            .is_some();
        if !existed {
            return Ok(false);
        }
        self.client
            .delete(&s3_key)
            .await
            .map_err(|e| map_s3(&s3_key, e))?;
        Ok(true)
    }

    async fn list(
        &self,
        app_id: &str,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<ListEntry>, String> {
        validate_list_coords(app_id, bucket)?;
        let scope = Self::list_prefix(app_id, bucket, prefix);
        // The client loops continuation tokens internally and strips the
        // configured `config.prefix`, returning logical keys that still
        // carry our `<app_id>/<bucket>/...` scope.
        let entries = self
            .client
            .list(&scope)
            .await
            .map_err(|e| map_s3(&scope, e))?;

        let strip = format!("{app_id}/{bucket}/");
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let Some(user_key) = e.key.strip_prefix(&strip) else {
                // Defensive: the list was scoped by `scope`, so every key
                // must begin with `<app_id>/<bucket>/`. Skip anything that
                // doesn't rather than surface a cross-tenant key.
                continue;
            };
            out.push(ListEntry {
                key: user_key.to_string(),
                size: e.size,
                modified_at: e.last_modified,
            });
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }
}

/// A [`ChunkSource`] over the client's streaming GET body.
struct S3Chunks {
    inner: std::pin::Pin<Box<dyn futures::Stream<Item = compio_s3::S3Result<Bytes>>>>,
}

#[async_trait::async_trait(?Send)]
impl ChunkSource for S3Chunks {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        use futures::StreamExt;
        match self.inner.next().await {
            Some(Ok(b)) => Some(Ok(b)),
            Some(Err(e)) => Some(Err(format!("storage: s3 get body: {e}"))),
            None => None,
        }
    }
}

/// Synchronous panic backstop for an in-progress multipart upload. Mirrors the
/// `bundle::s3_blob::PanicBackstop`: the real orphaned-parts guarantee is the
/// EXPLICIT, AWAITED `abort_multipart` on the error path in `put_stream`. This
/// guard only fires if the future unwinds (panics) mid-upload. It MUST NOT
/// panic and MUST NOT spawn — spawning in `Drop` panics off-runtime, and a
/// panic in `Drop` while already unwinding aborts the whole process. So it only
/// logs that parts may be orphaned (S3 lifecycle rules reclaim them).
struct PanicBackstop<'a> {
    key: &'a str,
    upload: Option<UploadId>,
}

impl Drop for PanicBackstop<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.upload.take() {
            tracing::warn!(
                key = %self.key,
                upload_id = %id.0,
                "multipart upload dropped without explicit abort (likely a panic mid-upload); \
                 parts may be orphaned until S3 lifecycle reclaims them",
            );
        }
    }
}

/// Flatten an `S3Error` into the `String` channel the `Backend` trait uses.
fn map_s3(key: &str, e: S3Error) -> String {
    format!("storage: s3 op on '{key}': {e}")
}
