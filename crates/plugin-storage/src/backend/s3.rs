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
}

#[async_trait::async_trait(?Send)]
impl Backend for S3 {
    async fn put_stream(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
        mut body: BoxChunkSource,
        content_type: Option<&str>,
    ) -> Result<u64, String> {
        validate_object_coords(app_id, bucket, key)?;
        let s3_key = Self::object_key(app_id, bucket, key);
        let content_type = content_type.unwrap_or("application/octet-stream");

        let mut total: u64 = 0;
        let mut parts: Vec<PartETag> = Vec::new();
        let mut upload: Option<UploadId> = None;
        let mut part_number: u32 = 0;

        // RAII guard: abort any started multipart on early return / drop.
        // S3 bills orphaned parts, so an abort on every error path is
        // mandatory. The happy path disarms it before drop.
        let mut guard = MultipartGuard {
            client: &self.client,
            key: &s3_key,
            upload: None,
        };

        // Accumulate incoming chunks into a part buffer; flush whole
        // PART_SIZE parts as they fill. Bounded memory, unbounded total.
        let mut part_buf: Vec<u8> = Vec::with_capacity(PART_SIZE);
        while let Some(chunk) = body.next_chunk().await {
            let chunk = chunk?;
            if chunk.is_empty() {
                continue;
            }
            total += chunk.len() as u64;
            part_buf.extend_from_slice(&chunk);

            while part_buf.len() >= PART_SIZE {
                if upload.is_none() {
                    let id = self
                        .client
                        .create_multipart(&s3_key, content_type)
                        .await
                        .map_err(|e| map_s3(&s3_key, e))?;
                    upload = Some(id.clone());
                    guard.upload = Some(id);
                }
                let id = upload.as_ref().expect("multipart created");
                let rest = part_buf.split_off(PART_SIZE);
                let part = Bytes::from(std::mem::replace(&mut part_buf, rest));
                part_number += 1;
                let etag = self
                    .client
                    .upload_part(&s3_key, id, part_number, part)
                    .await
                    .map_err(|e| map_s3(&s3_key, e))?;
                parts.push(etag);
            }
        }

        match upload.take() {
            // Single-part path: object below PART_SIZE → ordinary PutObject.
            None => {
                let opts = PutOptions {
                    content_type,
                    ..PutOptions::default()
                };
                self.client
                    .put(&s3_key, &part_buf, opts)
                    .await
                    .map_err(|e| map_s3(&s3_key, e))?;
                guard.upload = None;
            }
            // Multipart path: flush the final (short) part, then complete.
            Some(id) => {
                if !part_buf.is_empty() {
                    part_number += 1;
                    let part = Bytes::from(std::mem::take(&mut part_buf));
                    let etag = self
                        .client
                        .upload_part(&s3_key, &id, part_number, part)
                        .await
                        .map_err(|e| map_s3(&s3_key, e))?;
                    parts.push(etag);
                }
                self.client
                    .complete_multipart(&s3_key, &id, &parts)
                    .await
                    .map_err(|e| map_s3(&s3_key, e))?;
                guard.upload = None;
            }
        }

        Ok(total)
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

/// RAII abort guard for an in-progress multipart upload. Mirrors the
/// `bundle::s3_blob` guard: a detached best-effort `abort_multipart` fires
/// on any early return / drop so orphaned (billed) parts are reclaimed. The
/// happy path disarms it (`guard.upload = None`) before drop.
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
            compio::runtime::spawn(async move {
                let _ = client.abort_multipart(&key, &id).await;
            })
            .detach();
        }
    }
}

/// Flatten an `S3Error` into the `String` channel the `Backend` trait uses.
fn map_s3(key: &str, e: S3Error) -> String {
    format!("storage: s3 op on '{key}': {e}")
}
