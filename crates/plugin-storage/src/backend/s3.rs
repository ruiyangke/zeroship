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
//!   mid-upload aborts the multipart EXPLICITLY (awaited, in async context) so
//!   orphaned — and billed — parts are reclaimed. A DROP-cancel mid-upload
//!   (wall-timeout cancel, client disconnect, LRU eviction) cannot await, so a
//!   `compio_s3::MultipartGuard` records the live upload id on `Drop` and a
//!   later op (or an explicit drainer) calls `drain_orphaned_uploads` to abort
//!   it in async context. An S3 `AbortIncompleteMultipartUpload` lifecycle rule
//!   remains the backstop-of-last-resort.
//! - `get_stream` → the client's streaming GET (cyper `bytes_stream`), wrapped
//!   with the per-chunk body-read timeout.

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

/// One in-flight `UploadPart` future, boxed to a concrete (nameable) type so the
/// `FuturesUnordered` can be threaded through helper methods. Boxing a future is
/// NOT spawning — the future still only advances when the owning loop polls it,
/// so dropping the set on the error path is a safe cancellation of an ordinary
/// (un-spawned) future. `'a` ties it to the borrowed session client.
type InflightPart<'a> =
    futures::future::LocalBoxFuture<'a, Result<PartETag, String>>;

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
    /// overrun at `complete`. The live upload id is mirrored into `guard` the
    /// instant it is created (so a drop-cancel arms the orphan-cleanup path),
    /// and the guard is disarmed on a clean complete; no abort happens here —
    /// the caller owns the explicit error-path abort.
    ///
    /// ## Bounded-concurrency overlap pipeline (HIGH-2)
    ///
    /// ONE task drives the producer (`body.next_chunk()`) AND the in-flight
    /// `UploadPart` PUTs CONCURRENTLY via `futures::select!`. When a part PUT
    /// completes *while the next chunk is still arriving*, the two overlap — a
    /// slow producer no longer starves the in-flight uploads (the prior
    /// gate-only-drain loop parked on the producer between dispatches, leaving
    /// in-flight PUTs unpolled while their deadlines ticked). At most
    /// [`crate::limits::upload_concurrency`] PUTs run at once: at capacity the
    /// loop drains one before reading more, so live part memory is bounded by
    /// `~(N+1) × PART_SIZE` (the in-flight set plus a transiently-buffered
    /// part). Uploads finish out of order, so `(part_number, ETag)` results are
    /// sorted by part number before `complete_multipart` (S3 requires ascending
    /// part order). Any in-flight PUT error (or reader error, or a cap breach)
    /// propagates immediately; the remaining in-flight futures are dropped
    /// (cancelled) and the caller's error path aborts the multipart.
    ///
    /// The in-flight futures are PLAIN (un-spawned) `FuturesUnordered` members:
    /// dropping the set on the error path is a safe cancellation of ordinary
    /// futures. NO `spawn` — spawning + drop-cancelling a compio task with an
    /// in-flight io_uring op corrupts the runtime; the whole point of the
    /// select pipeline is overlap WITHOUT spawn. The per-part scaled send
    /// timeout (`send_for_body`) is kept as defense-in-depth against a
    /// genuinely *stalled* (not merely slow) part.
    #[allow(clippy::future_not_send)] // Backend is (?Send); body source is !Send by design
    #[allow(clippy::too_many_lines)] // single-pass stream → concurrent parts → complete
    async fn put_stream_inner(
        &self,
        s3_key: &str,
        content_type: &str,
        mut body: BoxChunkSource,
        guard: &compio_s3::MultipartGuard,
    ) -> Result<u64, String> {
        use futures::stream::FuturesUnordered;
        use futures::{FutureExt, StreamExt};

        let max_total = crate::limits::max_stream_object_bytes();
        let concurrency = crate::limits::upload_concurrency();

        let mut total: u64 = 0;
        let mut parts: Vec<PartETag> = Vec::new();
        let mut part_number: u32 = 0;

        // Working copy of the live upload id. Mirrored into `guard` (the
        // drop-safety observer) the instant it is created, so a DROP-cancel
        // anywhere below still arms the orphan-cleanup path. `None` until the
        // first part flush lazily creates the multipart.
        let mut upload_id: Option<UploadId> = None;

        // ONE pooled HTTP client shared across this multipart session's part
        // uploads AND its finalization (Complete/Abort). Concurrent parts reuse
        // its kept-alive connections instead of each opening a fresh one — that
        // is what makes N-way concurrency safe (a fresh-client-per-part fan-out
        // floods the host with TIME_WAIT sockets and trips transient connect
        // failures). Reusing it for the trailing `CompleteMultipartUpload` also
        // avoids the cold-connect latency a brand-new connection pays right
        // after a long upload. The client never outlives this call, so the
        // per-thread client invariant holds.
        let session_client = self.client.open_upload_session();

        // In-flight part-upload futures, at most `concurrency` live at once.
        // Each is a self-contained `async move` over the cloned session client +
        // owned bytes, so it carries no borrow of `self`/`body` and the set can
        // be polled concurrently on the single compio thread.
        let mut inflight = FuturesUnordered::new();

        // The part buffer: incoming chunks accumulate here and whole PART_SIZE
        // parts are dispatched as capacity allows. It transiently holds ≤ ~2
        // parts (a full part can sit buffered for one iteration while `inflight`
        // is at capacity), so total live memory is ≈ (N+1) × PART_SIZE.
        let mut part_buf: Vec<u8> = Vec::with_capacity(PART_SIZE);
        let mut producer_done = false;

        // Single overlap loop: read the producer and drive the in-flight PUTs
        // concurrently. Each iteration makes exactly one move forward.
        loop {
            if inflight.len() >= concurrency {
                // At capacity: drain one completed PUT before reading more, so
                // memory stays bounded. A part error propagates → caller aborts.
                match inflight.next().await {
                    Some(res) => parts.push(res?),
                    None => break, // unreachable (non-empty above), but safe
                }
            } else if producer_done {
                // Producer exhausted and we have spare capacity: stop reading
                // and fall through to the final-part flush + drain below.
                break;
            } else if inflight.is_empty() {
                // Nothing to overlap. Do NOT `select!` on an empty
                // `FuturesUnordered` (its `next()` is immediately `Ready(None)`
                // and would busy-loop) — just read the next chunk.
                match body.next_chunk().await {
                    Some(chunk) => {
                        self.handle_chunk(
                            chunk?,
                            s3_key,
                            content_type,
                            &session_client,
                            guard,
                            &mut total,
                            max_total,
                            &mut part_buf,
                            &mut part_number,
                            &mut upload_id,
                            &mut inflight,
                            concurrency,
                        )
                        .await?;
                    }
                    None => producer_done = true,
                }
            } else {
                // Overlap: a PUT completing WHILE the next chunk arrives is the
                // whole point — neither starves the other.
                futures::select! {
                    chunk = body.next_chunk().fuse() => match chunk {
                        Some(c) => {
                            self.handle_chunk(
                                c?,
                                s3_key,
                                content_type,
                                &session_client,
                                guard,
                                &mut total,
                                max_total,
                                &mut part_buf,
                                &mut part_number,
                                &mut upload_id,
                                &mut inflight,
                                concurrency,
                            )
                            .await?;
                        }
                        None => producer_done = true,
                    },
                    done = inflight.next() => match done {
                        Some(res) => parts.push(res?),
                        None => {} // set drained to empty; next iter re-reads
                    },
                }
            }
        }

        if let Some(id) = upload_id.as_ref() {
            // Multipart path: dispatch the final (short) part too, then drain
            // every in-flight upload before completing. Keep the upload id live
            // through complete so a failure there is still abortable; disarm the
            // guard only on a clean complete.
            if !part_buf.is_empty() {
                if part_number >= crate::limits::MAX_MULTIPART_PARTS {
                    return Err(format!(
                        "storage: multipart upload would exceed the S3 {}-part limit",
                        crate::limits::MAX_MULTIPART_PARTS
                    ));
                }
                part_number += 1;
                let part = Bytes::from(std::mem::take(&mut part_buf));
                inflight.push(
                    self.upload_part_owned(&session_client, s3_key, id, part_number, part)
                        .boxed_local(),
                );
            }
            // Drain all remaining in-flight part uploads. An error here drops
            // the rest (cancelling those plain futures); the caller aborts the
            // multipart.
            while let Some(res) = inflight.next().await {
                parts.push(res?);
            }
            // Uploads finish out of order — S3 requires the parts list in
            // ascending part-number order at complete time.
            parts.sort_by_key(|p| p.part_number);

            // Finalize on the warm session client (no cold post-upload connect).
            self.client
                .complete_multipart_on(&session_client, s3_key, id, &parts)
                .await
                .map_err(|e| map_s3(s3_key, e))?;
            // Completed — disarm the guard so neither the explicit error path
            // nor the Drop path aborts the now-live object.
            let _ = guard.take();
        } else {
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
        }

        Ok(total)
    }

    /// Fold one incoming chunk into the running state: enforce the size cap,
    /// update the running total, buffer the bytes, and dispatch any FULL
    /// `PART_SIZE` parts whose slots are free.
    ///
    /// Parts are dispatched ONLY while `inflight.len() < concurrency`. If
    /// `part_buf` still holds ≥ `PART_SIZE` when the in-flight set is at
    /// capacity, the bytes are LEFT buffered — the caller's next
    /// capacity-drain frees a slot and the following iteration flushes them.
    /// `part_buf` therefore transiently holds ≤ ~2 parts; total live memory
    /// stays ≈ (N+1) × PART_SIZE. Lazily creates the multipart (and arms the
    /// drop-safety guard) on the first full part.
    #[allow(clippy::too_many_arguments)] // shared loop state threaded explicitly
    #[allow(clippy::future_not_send)] // !Send by design (per-thread cyper client)
    async fn handle_chunk<'a>(
        &'a self,
        chunk: Bytes,
        s3_key: &'a str,
        content_type: &str,
        session_client: &'a compio_s3::UploadSession,
        guard: &compio_s3::MultipartGuard,
        total: &mut u64,
        max_total: u64,
        part_buf: &mut Vec<u8>,
        part_number: &mut u32,
        upload_id: &mut Option<UploadId>,
        inflight: &mut futures::stream::FuturesUnordered<InflightPart<'a>>,
        concurrency: usize,
    ) -> Result<(), String> {
        use futures::FutureExt;
        if chunk.is_empty() {
            return Ok(());
        }
        *total += chunk.len() as u64;
        if *total > max_total {
            return Err(format!(
                "storage: object exceeds max stream size {max_total} bytes \
                 (set {} to raise)",
                crate::limits::MAX_STREAM_OBJECT_BYTES_ENV
            ));
        }
        part_buf.extend_from_slice(&chunk);

        // Dispatch whole parts while we have data AND a free in-flight slot.
        // Leaving a full part buffered when at capacity is intentional — the
        // overlap loop's capacity-drain frees a slot next iteration.
        while part_buf.len() >= PART_SIZE && inflight.len() < concurrency {
            if upload_id.is_none() {
                let id = self
                    .client
                    .create_multipart(s3_key, content_type)
                    .await
                    .map_err(|e| map_s3(s3_key, e))?;
                // Arm the drop-safety guard the INSTANT the id exists.
                guard.set(id.clone());
                *upload_id = Some(id);
            }
            // Refuse to exceed S3's 10,000-part hard limit: such an upload
            // can never `complete`, so fail fast (the caller aborts).
            if *part_number >= crate::limits::MAX_MULTIPART_PARTS {
                return Err(format!(
                    "storage: multipart upload would exceed the S3 {}-part limit",
                    crate::limits::MAX_MULTIPART_PARTS
                ));
            }
            let id = upload_id.as_ref().expect("multipart created");
            // Copy exactly PART_SIZE into a tight buffer rather than
            // `split_off`-ing the (possibly grown) `part_buf`: the part `Bytes`
            // is held through the PUT + every retry, so an over-capacity
            // allocation would pin ~2× the intended memory.
            let part = Bytes::copy_from_slice(&part_buf[..PART_SIZE]);
            part_buf.drain(..PART_SIZE);
            *part_number += 1;
            inflight.push(
                self.upload_part_owned(session_client, s3_key, id, *part_number, part)
                    .boxed_local(),
            );
        }
        Ok(())
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
                    .upload_part_on(session_ref, &key, &id, part_number, part.clone())
                    .await
                {
                    Ok(etag) => return Ok(etag),
                    Err(e) if e.is_retryable() && attempt + 1 < UPLOAD_PART_RETRIES => {
                        attempt += 1;
                        compio::time::sleep(upload_retry_backoff(attempt)).await;
                    }
                    Err(e) => return Err(map_s3(&key, e)),
                }
            }
        }
    }
}

/// Per-part upload attempt budget (1 initial try + retries on retryable
/// transport/5xx errors). Concurrent part PUTs churn connections fast enough
/// that transient connect failures (`hyper` Connect, ephemeral-port/`TIME_WAIT`
/// pressure on a busy host) are expected on multi-GiB uploads; a generous
/// bounded retry keeps such blips from aborting the whole upload.
const UPLOAD_PART_RETRIES: u32 = 8;

/// Capped exponential backoff for a part-upload retry: 100ms, 200ms, 400ms …
/// up to ~2s. Backing off (rather than hammering) lets the host recycle
/// ephemeral ports / `TIME_WAIT` sockets and lets the endpoint drain its accept
/// backlog before the next connect attempt.
fn upload_retry_backoff(attempt: u32) -> std::time::Duration {
    let ms = 100u64.saturating_mul(1u64 << attempt.min(5)); // cap shift at 32×
    std::time::Duration::from_millis(ms.min(2000))
}

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

        // Best-effort drain of any orphan recorded by a PRIOR drop-cancelled
        // upload on this thread. Cheap when the queue is empty (no network).
        self.client.drain_orphaned_uploads().await;

        // The drop-safety guard. The inner fn mirrors the live upload id into
        // it the instant `create_multipart` returns, and disarms it on a clean
        // complete. If THIS future is drop-cancelled mid-flight (wall-timeout
        // cancel, client disconnect, LRU eviction), the guard's sync `Drop`
        // warns + enqueues the orphaned upload for a later async abort — the
        // abort a `Drop` cannot itself perform (C1: no spawn/await in Drop).
        let guard = compio_s3::MultipartGuard::new(s3_key.clone());

        let result = self
            .put_stream_inner(&s3_key, content_type, body, &guard)
            .await;

        match result {
            Ok(total) => Ok(total),
            Err(e) => {
                // Explicit, awaited, best-effort abort in async context (NOT a
                // detached spawn) so orphaned (billed) parts are reclaimed. The
                // abort failing must NOT mask the original error, but it MUST be
                // surfaced — a silently-failed abort leaves billed parts behind.
                // `guard.take()` disarms the guard so its Drop does NOT also
                // enqueue this upload (we are handling it here).
                if let Some(id) = guard.take() {
                    if let Err(abort_err) = self.client.abort_multipart(&s3_key, &id).await {
                        tracing::warn!(
                            key = %s3_key,
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
        //
        // NB: the returned boolean is BEST-EFFORT under concurrency. The
        // HEAD→DELETE pair is not atomic, so a racing writer/deleter can change
        // the object's existence between the two requests (a classic TOCTOU):
        // we may report `true` for an object a concurrent deleter already
        // removed, or `false`/`true` inconsistently if a writer creates it in
        // the window. The DELETE itself is always idempotent and safe; only the
        // existed-or-not signal is racy.
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

/// Flatten an `S3Error` into the `String` channel the `Backend` trait uses.
fn map_s3(key: &str, e: S3Error) -> String {
    format!("storage: s3 op on '{key}': {e}")
}
