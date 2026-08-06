//! `S3Client` — the request-scoped, compio-native S3 client.
//!
//! ## Thread / connection discipline
//!
//! `cyper::Client` is **per-thread** (its connector wraps I/O in
//! `SendWrapper`, which panics if dereferenced off its origin thread). The
//! cyper connection pool is also not a proven dirty-connection barrier for S3
//! responses (every PUT/DELETE error can carry a body). So every operation:
//!
//! 1. builds a **fresh** `cyper::Client` on the current compio thread,
//! 2. sends one request under a `compio::time::timeout`,
//! 3. drains / drops the response (and the client) before returning.
//!
//! `S3Client` itself stores only immutable `Arc`-wrapped config/credentials and
//! a clock, so it is `Send + Sync` and may be shared across trait objects; the
//! live HTTP client never escapes a single `async fn` call.
//!
//! ## Surface (proposal §1 + the v1 streaming/multipart full-scope section)
//!
//! - [`S3Client::head_object`] / [`get`](S3Client::get) /
//!   [`get_stream`](S3Client::get_stream) /
//!   [`get_object_to_file`](S3Client::get_object_to_file)
//! - [`put`](S3Client::put) / [`delete`](S3Client::delete) /
//!   [`list`](S3Client::list)
//! - multipart: [`create_multipart`](S3Client::create_multipart) →
//!   [`upload_part`](S3Client::upload_part) →
//!   [`complete_multipart`](S3Client::complete_multipart) /
//!   [`abort_multipart`](S3Client::abort_multipart)

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use compio::io::AsyncWriteAtExt;
use futures::StreamExt;
use http::Method;
use sha2::{Digest, Sha256};

use crate::clock::{Clock, SigningTime, SystemClock};
use crate::config::{ChecksumMode, S3Config, SseMode};
use crate::credentials::S3Credentials;
use crate::error::{S3Error, S3Result};
use crate::list_xml::{
    self, build_complete_multipart_body, check_complete_multipart_response,
    parse_list_multipart_uploads, parse_upload_id,
};
use crate::signer::{self, SignHeader, SignRequest, EMPTY_PAYLOAD_SHA256};

/// Cap on a diagnostic error body we will read/drain.
const ERROR_BODY_CAP: u64 = 16 * 1024;

/// Object metadata returned from HEAD / GET.
#[derive(Debug, Clone)]
pub struct ObjectMeta {
    /// Object length in bytes.
    pub len: u64,
    /// `Content-Type`, if present.
    pub content_type: Option<String>,
    /// `Last-Modified` (mandatory for a successful GET/HEAD).
    pub last_modified: SystemTime,
    /// User-supplied SHA-256 from `x-amz-meta-sha256`, if present.
    pub user_sha256: Option<String>,
}

/// Result of a single-object PUT.
#[derive(Debug, Clone)]
pub struct PutResult {
    /// `ETag` response header, if present.
    pub e_tag: Option<String>,
    /// `x-amz-version-id`, if present.
    pub version_id: Option<String>,
}

/// One logical list entry (internal prefix already stripped by the caller).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    /// Object key (decoded, internal prefix stripped).
    pub key: String,
    /// Size in bytes.
    pub size: u64,
    /// `LastModified`.
    pub last_modified: SystemTime,
}

/// A single page from `list_objects_v2` (raw, internal-prefixed keys).
#[derive(Debug, Clone)]
pub struct ListPage {
    /// Entries on this page (keys keep the internal prefix).
    pub entries: Vec<ListEntry>,
    /// Whether more pages follow.
    pub is_truncated: bool,
    /// Continuation token for the next page.
    pub next_continuation_token: Option<String>,
}

/// An in-progress multipart upload identifier.
#[derive(Debug, Clone)]
pub struct UploadId(pub String);

/// An opaque, pooled HTTP client scoped to a single multipart upload session.
///
/// Built by [`S3Client::open_upload_session`], handed back to
/// [`upload_part_on`](S3Client::upload_part_on) for each part. Sharing one
/// session across a multipart upload's concurrent parts lets the underlying
/// connection pool be **reused** (kept-alive connections) instead of a fresh
/// connection being opened per part — which under high concurrency would flood
/// the host with `TIME_WAIT` sockets and trip transient connect failures. The
/// inner transport type is intentionally not exposed; the session is cheap to
/// clone (an `Arc` handle) and must not outlive the call site that owns it (the
/// per-thread client invariant — see the module header).
#[derive(Debug, Clone)]
pub struct UploadSession(cyper::Client);

impl UploadSession {
    /// A fresh, single-use upload session backed by its OWN `cyper::Client`
    /// (and thus its own connection pool). Used for a part RETRY so a prior
    /// attempt's possibly-dirty pooled connection is never reused — see
    /// [`S3Client::open_upload_session`].
    #[must_use]
    pub fn fresh() -> Self {
        Self(new_client())
    }
}

/// A completed part's number + `ETag`, fed to `complete_multipart`.
#[derive(Debug, Clone)]
pub struct PartETag {
    /// 1-based part number.
    pub part_number: u32,
    /// The part's `ETag` response header.
    pub e_tag: String,
}

/// Per-PUT options.
#[derive(Debug, Clone)]
pub struct PutOptions<'a> {
    /// `Content-Type` header.
    pub content_type: &'a str,
    /// If true, send `If-None-Match: *` (conditional create).
    pub if_none_match: bool,
    /// Extra `x-amz-meta-*` user-metadata, as `(suffix, value)` (suffix without
    /// the `x-amz-meta-` prefix).
    pub user_meta: &'a [(&'a str, String)],
    /// `Cache-Control`, if any.
    pub cache_control: Option<&'a str>,
}

impl Default for PutOptions<'_> {
    fn default() -> Self {
        Self {
            content_type: "application/octet-stream",
            if_none_match: false,
            user_meta: &[],
            cache_control: None,
        }
    }
}

/// The compio-native S3 client. Cheap to clone (`Arc` handles).
#[derive(Debug, Clone)]
pub struct S3Client {
    config: Arc<S3Config>,
    credentials: Arc<S3Credentials>,
    clock: Arc<dyn Clock>,
}

impl S3Client {
    /// Construct a client from config + credentials, using the real clock.
    #[must_use]
    pub fn new(config: S3Config, credentials: S3Credentials) -> Self {
        Self {
            config: Arc::new(config),
            credentials: Arc::new(credentials),
            clock: Arc::new(SystemClock),
        }
    }

    /// Construct with an injectable clock (for deterministic signing in tests).
    #[must_use]
    pub fn with_clock(config: S3Config, credentials: S3Credentials, clock: Arc<dyn Clock>) -> Self {
        Self {
            config: Arc::new(config),
            credentials: Arc::new(credentials),
            clock,
        }
    }

    /// The underlying config.
    #[must_use]
    pub fn config(&self) -> &S3Config {
        &self.config
    }

    // --------------------------------------------------------------------
    // URL / header / signing assembly
    // --------------------------------------------------------------------

    /// Build the request URL, the signed `host` value, and the canonical URI
    /// path for a stored object key.
    fn object_url(&self, stored_key: &str) -> (String, String, String) {
        let parts = self.config.endpoint_parts();
        // Canonical URI path: optional /bucket + /key, each segment encoded
        // with S3 path rules (slashes preserved).
        let key_path = signer::encode_path(&format!("/{stored_key}"));
        let canonical_uri = if parts.bucket_in_path {
            format!("/{}{}", encode_segment(&self.config.bucket), key_path)
        } else {
            key_path
        };
        let url = format!("{}://{}{}", parts.scheme, parts.host, canonical_uri);
        (url, parts.host, canonical_uri)
    }

    /// Build a bucket-level URL (for `ListObjectsV2`) with query params.
    fn bucket_url(&self, query: &[(String, String)]) -> (String, String, String, String) {
        let parts = self.config.endpoint_parts();
        let canonical_uri = if parts.bucket_in_path {
            format!("/{}", encode_segment(&self.config.bucket))
        } else {
            "/".to_string()
        };
        let canonical_query = signer::canonical_query(query);
        let url = if canonical_query.is_empty() {
            format!("{}://{}{}", parts.scheme, parts.host, canonical_uri)
        } else {
            format!(
                "{}://{}{}?{}",
                parts.scheme, parts.host, canonical_uri, canonical_query
            )
        };
        (url, parts.host, canonical_uri, canonical_query)
    }

    /// Assemble the full signed header set and produce header `(name, value)`
    /// pairs ready to attach to a cyper request.
    ///
    /// `host`, `x-amz-date`, `x-amz-content-sha256`, and (when present)
    /// `x-amz-security-token` are always included and signed alongside the
    /// caller's `extra` headers.
    fn signed_headers(
        &self,
        method: &str,
        host: &str,
        canonical_uri: &str,
        query: &[(String, String)],
        payload_sha256: &str,
        extra: &[(String, String)],
    ) -> Vec<(String, String)> {
        let now = SigningTime::from_clock(self.clock.as_ref());

        let mut to_sign: Vec<SignHeader> = Vec::with_capacity(extra.len() + 4);
        to_sign.push(SignHeader::new("host", host));
        to_sign.push(SignHeader::new("x-amz-date", now.amz_date()));
        to_sign.push(SignHeader::new("x-amz-content-sha256", payload_sha256));
        if let Some(token) = &self.credentials.session_token {
            to_sign.push(SignHeader::new("x-amz-security-token", token.clone()));
        }
        for (k, v) in extra {
            to_sign.push(SignHeader::new(k.clone(), v.clone()));
        }

        let region = self.config.region.as_str();
        let signed = SignRequest {
            method,
            canonical_uri,
            query,
            headers: &to_sign,
            payload_sha256,
            amz_date: now.amz_date(),
            scope_date: now.scope_date(),
            region,
            service: "s3",
        }
        .sign(&self.credentials);

        // Emit every header we signed plus the Authorization header.
        let mut out: Vec<(String, String)> = to_sign
            .into_iter()
            .map(|h| (h.name, h.value))
            .collect();
        out.push(("authorization".to_string(), signed.authorization));
        out
    }

    /// Apply checksum + SSE headers to a PUT's `extra` header list.
    fn put_meta_headers(
        &self,
        body: &[u8],
        opts: &PutOptions<'_>,
    ) -> Vec<(String, String)> {
        let mut extra: Vec<(String, String)> = Vec::new();
        extra.push(("content-type".to_string(), opts.content_type.to_string()));
        if opts.if_none_match {
            extra.push(("if-none-match".to_string(), "*".to_string()));
        }
        if let Some(cc) = opts.cache_control {
            extra.push(("cache-control".to_string(), cc.to_string()));
        }
        for (suffix, value) in opts.user_meta {
            extra.push((format!("x-amz-meta-{suffix}"), value.clone()));
        }
        if self.config.checksum == ChecksumMode::Sha256 {
            let mut h = Sha256::new();
            h.update(body);
            let digest = h.finalize();
            extra.push((
                "x-amz-checksum-sha256".to_string(),
                base64_std(&digest),
            ));
        }
        match &self.config.sse {
            SseMode::None => {}
            SseMode::SseS3 => {
                extra.push((
                    "x-amz-server-side-encryption".to_string(),
                    "AES256".to_string(),
                ));
            }
            SseMode::SseKms(key) => {
                extra.push((
                    "x-amz-server-side-encryption".to_string(),
                    "aws:kms".to_string(),
                ));
                extra.push((
                    "x-amz-server-side-encryption-aws-kms-key-id".to_string(),
                    key.clone(),
                ));
            }
        }
        extra
    }

    // --------------------------------------------------------------------
    // Operations
    // --------------------------------------------------------------------

    /// `HEAD` an object. Returns `None` on 404.
    pub async fn head_object(&self, key: &str) -> S3Result<Option<ObjectMeta>> {
        let stored = self.config.object_key(key);
        let (url, host, canonical_uri) = self.object_url(&stored);
        let headers = self.signed_headers(
            "HEAD",
            &host,
            &canonical_uri,
            &[],
            EMPTY_PAYLOAD_SHA256,
            &[],
        );

        let client = new_client();
        let resp = self
            .send(client.request(Method::HEAD, &url)?, headers)
            .await?;
        let status = resp.status().as_u16();
        match status {
            200 => {
                let meta = parse_object_meta(&resp)?;
                // HEAD has no body; drop the response.
                drop(resp);
                Ok(Some(meta))
            }
            404 => {
                drop(resp);
                Ok(None)
            }
            other => Err(self.error_from_response(other, resp).await),
        }
    }

    /// `GET` an object fully into memory, capped at `max_bytes`.
    pub async fn get(&self, key: &str, max_bytes: u64) -> S3Result<(Bytes, ObjectMeta)> {
        let stored = self.config.object_key(key);
        let (url, host, canonical_uri) = self.object_url(&stored);
        let headers = self.signed_headers(
            "GET",
            &host,
            &canonical_uri,
            &[],
            EMPTY_PAYLOAD_SHA256,
            &[],
        );
        let client = new_client();
        let resp = self
            .send(client.request(Method::GET, &url)?, headers)
            .await?;
        let status = resp.status().as_u16();
        match status {
            200 => {
                let meta = parse_object_meta(&resp)?;
                if let Some(cl) = resp.content_length()
                    && cl > max_bytes
                {
                    drop(resp);
                    return Err(S3Error::TooLarge {
                        limit: max_bytes,
                        observed: Some(cl),
                    });
                }
                let bytes = self.read_body_capped(resp, max_bytes).await?;
                Ok((bytes, meta))
            }
            404 => {
                drop(resp);
                Err(S3Error::NotFound)
            }
            other => Err(self.error_from_response(other, resp).await),
        }
    }

    /// `GET` an object as a streaming body. Returns the metadata plus a
    /// `Stream<Item = S3Result<Bytes>>` (cyper `bytes_stream()`).
    ///
    /// ## Early-cancel connection safety + per-chunk timeout
    ///
    /// The returned stream owns (via its `unfold` state — see
    /// [`TimeoutStreamState`]) the *fresh, per-operation* `cyper::Client` built
    /// for this GET. That client's connection pool therefore lives and dies
    /// with the stream: dropping the stream before EOF (e.g. the V8
    /// `ReadableStream` consumer calls `cancelStream` mid-body) drops the
    /// half-read response AND the owning `Client`, destroying the pool. A
    /// half-read (dirty) HTTP/1.1 connection is consequently never returned to
    /// any pool a *later* request could draw from — there is no shared,
    /// longer-lived client to desync. This is the same fresh-client-per-op
    /// discipline the module header describes, extended to the streaming path.
    ///
    /// Each chunk pull is wrapped in the configured `timeouts.body` per-chunk
    /// timeout (the SAME guard `get_object_to_file` applies), so a stalled
    /// upstream connection can never wedge the downstream consumer forever.
    pub async fn get_stream(
        &self,
        key: &str,
    ) -> S3Result<(ObjectMeta, impl futures::Stream<Item = S3Result<Bytes>> + 'static)> {
        let stored = self.config.object_key(key);
        let (url, host, canonical_uri) = self.object_url(&stored);
        let headers = self.signed_headers(
            "GET",
            &host,
            &canonical_uri,
            &[],
            EMPTY_PAYLOAD_SHA256,
            &[],
        );
        let client = new_client();
        let resp = self
            .send(client.request(Method::GET, &url)?, headers)
            .await?;
        let status = resp.status().as_u16();
        match status {
            200 => {
                let meta = parse_object_meta(&resp)?;
                // Per-chunk body-read timeout — the SAME `timeouts.body` guard
                // `get_object_to_file` applies. Without it a stalled S3/R2
                // connection wedges the downstream consumer (the V8
                // `ReadableStream` reader, or the buffered `get` drain) forever
                // — on the unlimited plan there is no outer wall clock to
                // rescue it. `unfold` lets each pull await `next()` under a
                // fresh `compio::time::timeout`; a stall yields one terminal
                // timeout error and then ends the stream.
                let body_to = self.config.timeouts.body;
                let inner = resp.bytes_stream().map(|r| r.map_err(S3Error::from));
                // The fresh per-op `client` must outlive the body stream (its
                // connection pool dies with it — H1 invariant). It is carried
                // in the unfold state so it drops only when the stream ends.
                let stream = timeout_body_stream(inner, client, body_to);
                Ok((meta, stream))
            }
            404 => {
                drop(resp);
                Err(S3Error::NotFound)
            }
            other => Err(self.error_from_response(other, resp).await),
        }
    }

    /// `GET` an object, streaming the body into an already-open temp file while
    /// hashing. Enforces `Content-Length`/`expected_size`/`max_bytes` and an
    /// optional `expected_sha256`. Returns the parsed metadata.
    pub async fn get_object_to_file(
        &self,
        key: &str,
        out: &compio::fs::File,
        expected_size: Option<u64>,
        max_bytes: u64,
        expected_sha256: Option<&str>,
    ) -> S3Result<ObjectMeta> {
        let stored = self.config.object_key(key);
        let (url, host, canonical_uri) = self.object_url(&stored);
        let headers = self.signed_headers(
            "GET",
            &host,
            &canonical_uri,
            &[],
            EMPTY_PAYLOAD_SHA256,
            &[],
        );
        let client = new_client();
        let resp = self
            .send(client.request(Method::GET, &url)?, headers)
            .await?;
        let status = resp.status().as_u16();
        match status {
            200 => {}
            404 => {
                drop(resp);
                return Err(S3Error::NotFound);
            }
            other => return Err(self.error_from_response(other, resp).await),
        }

        let meta = parse_object_meta(&resp)?;
        if let Some(cl) = resp.content_length() {
            if cl > max_bytes {
                drop(resp);
                return Err(S3Error::TooLarge {
                    limit: max_bytes,
                    observed: Some(cl),
                });
            }
            if let Some(exp) = expected_size
                && cl != exp
            {
                drop(resp);
                return Err(S3Error::InvalidResponse(format!(
                    "content-length {cl} != expected {exp}"
                )));
            }
        }

        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        let mut offset: u64 = 0;
        let mut stream = resp.bytes_stream();
        let body_to = self.config.timeouts.body;
        loop {
            let next = compio::time::timeout(body_to, stream.next()).await;
            let chunk = match next {
                Err(_) => return Err(S3Error::timeout("body read timeout")),
                Ok(None) => break,
                Ok(Some(Ok(c))) => c,
                Ok(Some(Err(e))) => return Err(S3Error::from(e)),
            };
            total += chunk.len() as u64;
            if total > max_bytes {
                return Err(S3Error::TooLarge {
                    limit: max_bytes,
                    observed: Some(total),
                });
            }
            if let Some(exp) = expected_size
                && total > exp
            {
                return Err(S3Error::InvalidResponse(format!(
                    "stream exceeds expected size {exp}"
                )));
            }
            hasher.update(&chunk);
            let owned = chunk.to_vec();
            // `AsyncWriteAtExt::write_all_at` is implemented for `&File` and
            // takes `&mut self`; bind a fresh `&File` and borrow it mutably.
            let mut fref: &compio::fs::File = out;
            let compio::BufResult(res, _) = fref.write_all_at(owned, offset).await;
            res.map_err(|e| S3Error::Transport(format!("write temp: {e}")))?;
            offset += chunk.len() as u64;
        }
        drop(stream);

        if let Some(exp) = expected_size
            && total != exp
        {
            return Err(S3Error::InvalidResponse(format!(
                "size mismatch: expected {exp}, observed {total}"
            )));
        }
        if let Some(exp_hash) = expected_sha256 {
            let computed = hex::encode(hasher.finalize());
            if !computed.eq_ignore_ascii_case(exp_hash) {
                return Err(S3Error::Integrity {
                    expected: exp_hash.to_string(),
                    computed,
                });
            }
        }
        let mut meta = meta;
        meta.len = total;
        Ok(meta)
    }

    /// Single-object `PUT` of a buffered body.
    pub async fn put(&self, key: &str, body: &[u8], opts: PutOptions<'_>) -> S3Result<PutResult> {
        let stored = self.config.object_key(key);
        let (url, host, canonical_uri) = self.object_url(&stored);
        let payload = signer::sha256_hex(body);
        let extra = self.put_meta_headers(body, &opts);
        let headers =
            self.signed_headers("PUT", &host, &canonical_uri, &[], &payload, &extra);
        let client = new_client();
        let mut req = client.request(Method::PUT, &url)?;
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str()).map_err(map_build_err)?;
        }
        req = req.body(Bytes::copy_from_slice(body));
        let resp = self.send_built(req).await?;
        let status = resp.status().as_u16();
        match status {
            200 | 201 | 204 => {
                let e_tag = header(&resp, "etag");
                let version_id = header(&resp, "x-amz-version-id");
                drop(resp);
                Ok(PutResult { e_tag, version_id })
            }
            other => Err(self.error_from_response(other, resp).await),
        }
    }

    /// `DELETE` an object. A 404 is treated as success (already absent).
    pub async fn delete(&self, key: &str) -> S3Result<()> {
        let stored = self.config.object_key(key);
        let (url, host, canonical_uri) = self.object_url(&stored);
        let headers = self.signed_headers(
            "DELETE",
            &host,
            &canonical_uri,
            &[],
            EMPTY_PAYLOAD_SHA256,
            &[],
        );
        let client = new_client();
        let resp = self
            .send(client.request(Method::DELETE, &url)?, headers)
            .await?;
        let status = resp.status().as_u16();
        match status {
            200 | 204 | 404 => {
                drop(resp);
                Ok(())
            }
            other => Err(self.error_from_response(other, resp).await),
        }
    }

    /// One `ListObjectsV2` page (raw, internal-prefixed keys).
    pub async fn list_objects_v2(
        &self,
        prefix: &str,
        continuation: Option<&str>,
    ) -> S3Result<ListPage> {
        let mut query: Vec<(String, String)> = vec![
            ("list-type".to_string(), "2".to_string()),
            ("encoding-type".to_string(), "url".to_string()),
        ];
        if !prefix.is_empty() {
            query.push(("prefix".to_string(), prefix.to_string()));
        }
        if let Some(token) = continuation {
            query.push(("continuation-token".to_string(), token.to_string()));
        }

        let (url, host, canonical_uri, _cq) = self.bucket_url(&query);
        let headers = self.signed_headers(
            "GET",
            &host,
            &canonical_uri,
            &query,
            EMPTY_PAYLOAD_SHA256,
            &[],
        );
        let client = new_client();
        let resp = self
            .send(client.request(Method::GET, &url)?, headers)
            .await?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(self.error_from_response(status, resp).await);
        }
        // Cap the XML body.
        let xml = self
            .read_body_capped(resp, self.config.max_list_entries as u64 * 4096 + 65_536)
            .await?;
        let raw = list_xml::parse_list_objects_v2(&xml)?;
        let mut entries = Vec::with_capacity(raw.entries.len());
        for e in raw.entries {
            let lm = parse_rfc3339(&e.last_modified)?;
            entries.push(ListEntry {
                key: e.key,
                size: e.size,
                last_modified: lm,
            });
        }
        Ok(ListPage {
            entries,
            is_truncated: raw.is_truncated,
            next_continuation_token: raw.next_continuation_token,
        })
    }

    /// Loop `ListObjectsV2` pages internally, returning all entries with the
    /// internal prefix stripped. Caps at `max_list_entries`.
    pub async fn list(&self, prefix: &str) -> S3Result<Vec<ListEntry>> {
        let stored_prefix = match &self.config.prefix {
            Some(p) if prefix.is_empty() => format!("{p}/"),
            Some(p) => format!("{p}/{prefix}"),
            None => prefix.to_string(),
        };
        // The internal prefix to strip from each returned key, as a string
        // (`<prefix>/`). Stripping by `strip_prefix` rather than a byte index
        // is REQUIRED: `e.key` is server-supplied and a raw byte-index slice
        // (`e.key[strip_len..]`) panics if `strip_len` lands mid-UTF-8 — a
        // remote-controllable worker crash.
        let internal_prefix: Option<String> =
            self.config.prefix.as_ref().map(|p| format!("{p}/"));

        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self
                .list_objects_v2(&stored_prefix, token.as_deref())
                .await?;
            for e in page.entries {
                let logical = strip_internal_prefix(&e.key, internal_prefix.as_deref())?;
                out.push(ListEntry {
                    key: logical,
                    size: e.size,
                    last_modified: e.last_modified,
                });
                if out.len() > self.config.max_list_entries {
                    return Err(S3Error::TooLarge {
                        limit: self.config.max_list_entries as u64,
                        observed: None,
                    });
                }
            }
            if page.is_truncated {
                token = page.next_continuation_token;
                if token.is_none() {
                    return Err(S3Error::InvalidResponse(
                        "truncated list without continuation token".into(),
                    ));
                }
            } else {
                break;
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    // --------------------------------------------------------------------
    // Multipart
    // --------------------------------------------------------------------

    /// Initiate a multipart upload; returns the `UploadId`.
    pub async fn create_multipart(&self, key: &str, content_type: &str) -> S3Result<UploadId> {
        let stored = self.config.object_key(key);
        let (base_url, host, canonical_uri) = self.object_url(&stored);
        let query = vec![("uploads".to_string(), String::new())];
        let mut extra: Vec<(String, String)> =
            vec![("content-type".to_string(), content_type.to_string())];
        self.append_sse_headers(&mut extra);
        let headers = self.signed_headers(
            "POST",
            &host,
            &canonical_uri,
            &query,
            EMPTY_PAYLOAD_SHA256,
            &extra,
        );
        let url = format!("{base_url}?uploads=");
        let client = new_client();
        let mut req = client.request(Method::POST, &url)?;
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str()).map_err(map_build_err)?;
        }
        // Multipart CONTROL op → generous control-op send timeout.
        let resp = self
            .send_built_with_timeout(req, self.config.timeouts.send_control)
            .await?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(self.error_from_response(status, resp).await);
        }
        let xml = self.read_body_capped(resp, ERROR_BODY_CAP * 4).await?;
        Ok(UploadId(parse_upload_id(&xml)?))
    }

    /// Upload one part (buffered `Bytes`, ≤ part size) on a **fresh**
    /// per-operation client. Returns its `ETag`. Use
    /// [`upload_part_on`](S3Client::upload_part_on) with an
    /// [`open_upload_session`](S3Client::open_upload_session) handle to share
    /// one pooled client across a multipart session's concurrent part uploads.
    pub async fn upload_part(
        &self,
        key: &str,
        upload_id: &UploadId,
        part_number: u32,
        body: Bytes,
    ) -> S3Result<PartETag> {
        let session = self.open_upload_session();
        self.upload_part_on(&session, key, upload_id, part_number, body)
            .await
    }

    /// Open a pooled [`UploadSession`] for a multipart upload — one client to be
    /// shared across that upload's concurrent part PUTs so connections are
    /// reused instead of a fresh one opened per part.
    ///
    /// This is the key to safe *concurrent* part uploads: N parts in flight
    /// over ONE pooled session reuse ~N kept-alive connections, rather than each
    /// `upload_part` opening (and tearing down) its own — which under high
    /// concurrency floods the host with `TIME_WAIT` sockets and trips transient
    /// connect failures. Like every other op the session never outlives the
    /// call site, so the per-thread `SendWrapper` invariant holds. Buffered
    /// PUTs have no early-cancel/dirty-connection concern (unlike streaming
    /// GET, which deliberately keeps its own fresh client — see `get_stream`).
    ///
    /// NB: the shared session is for the *happy path*. A part RETRY (after a
    /// transport/timeout failure on a prior attempt) must NOT reuse this pooled
    /// connection — a send future cancelled mid-body can leave the connection
    /// dirty, and cyper's pool is not a proven dirty-connection barrier. Retries
    /// open a [`UploadSession::fresh`] per attempt instead.
    #[must_use]
    pub fn open_upload_session(&self) -> UploadSession {
        UploadSession(new_client())
    }

    /// Upload one part on a caller-provided [`UploadSession`]. Identical to
    /// [`upload_part`](S3Client::upload_part) except the pooled HTTP client (and
    /// thus its connection pool) is supplied by the caller, letting a multipart
    /// session reuse one pooled client across all its concurrent parts.
    pub async fn upload_part_on(
        &self,
        session: &UploadSession,
        key: &str,
        upload_id: &UploadId,
        part_number: u32,
        body: Bytes,
    ) -> S3Result<PartETag> {
        let client = &session.0;
        let stored = self.config.object_key(key);
        let (base_url, host, canonical_uri) = self.object_url(&stored);
        let query = vec![
            ("partNumber".to_string(), part_number.to_string()),
            ("uploadId".to_string(), upload_id.0.clone()),
        ];
        let body_len = body.len() as u64;
        let payload = signer::sha256_hex(&body);
        let headers =
            self.signed_headers("PUT", &host, &canonical_uri, &query, &payload, &[]);
        let cq = signer::canonical_query(&query);
        let url = format!("{base_url}?{cq}");
        let mut req = client.request(Method::PUT, &url)?;
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str()).map_err(map_build_err)?;
        }
        req = req.body(body);
        // Scale the send deadline with the part size: a fixed 30s wall clock
        // would fail an 8 MiB part on any uplink under ~270 KB/s even though it
        // is making steady progress. `send_for_body` adds headroom proportional
        // to the body at an assumed minimum throughput.
        let send_to = self.config.timeouts.send_for_body(body_len);
        let resp = self.send_built_with_timeout(req, send_to).await?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(self.error_from_response(status, resp).await);
        }
        let e_tag = header(&resp, "etag")
            .ok_or_else(|| S3Error::InvalidResponse("UploadPart missing ETag".into()))?;
        drop(resp);
        Ok(PartETag { part_number, e_tag })
    }

    /// Complete a multipart upload on a **fresh** per-operation client. Parses
    /// the XML response for in-200 errors.
    ///
    /// Prefer [`complete_multipart_on`](S3Client::complete_multipart_on) with
    /// the upload session's already-warm pooled client: opening a *cold*
    /// connection for `CompleteMultipartUpload` right after a long-running
    /// upload is slow to establish on some endpoints (`cyper`/`MinIO` over
    /// `io_uring`), and the warm-session variant avoids that connect latency.
    pub async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &UploadId,
        parts: &[PartETag],
    ) -> S3Result<()> {
        let session = self.open_upload_session();
        self.complete_multipart_on(&session, key, upload_id, parts)
            .await
    }

    /// Complete a multipart upload, reusing the caller's warm pooled
    /// [`UploadSession`] (the same client the part PUTs used) instead of opening
    /// a fresh, cold connection. After a long upload a brand-new connection is
    /// slow to establish on this `cyper`/`MinIO`/`io_uring` stack; reusing the warm
    /// kept-alive pool removes that post-upload connect latency.
    pub async fn complete_multipart_on(
        &self,
        session: &UploadSession,
        key: &str,
        upload_id: &UploadId,
        parts: &[PartETag],
    ) -> S3Result<()> {
        let client = &session.0;
        let stored = self.config.object_key(key);
        let (base_url, host, canonical_uri) = self.object_url(&stored);
        let query = vec![("uploadId".to_string(), upload_id.0.clone())];
        let part_pairs: Vec<(u32, String)> = parts
            .iter()
            .map(|p| (p.part_number, p.e_tag.clone()))
            .collect();
        let body = build_complete_multipart_body(&part_pairs);
        let body_bytes = body.into_bytes();
        let payload = signer::sha256_hex(&body_bytes);
        let extra = vec![("content-type".to_string(), "application/xml".to_string())];
        let headers =
            self.signed_headers("POST", &host, &canonical_uri, &query, &payload, &extra);
        let cq = signer::canonical_query(&query);
        let url = format!("{base_url}?{cq}");
        let mut req = client.request(Method::POST, &url)?;
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str()).map_err(map_build_err)?;
        }
        req = req.body(Bytes::from(body_bytes));
        // Multipart CONTROL op (finalizes a possibly long-running upload) →
        // generous control-op send timeout.
        let resp = self
            .send_built_with_timeout(req, self.config.timeouts.send_control)
            .await?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(self.error_from_response(status, resp).await);
        }
        let xml = self.read_body_capped(resp, ERROR_BODY_CAP * 4).await?;
        check_complete_multipart_response(&xml)
    }

    /// Abort a multipart upload on a **fresh** per-operation client. MUST be
    /// called on any mid-upload error to free orphaned (billed) parts. A 404 is
    /// treated as success.
    ///
    /// The error-path abort on a still-open upload session should prefer
    /// [`abort_multipart_on`](S3Client::abort_multipart_on) with the warm
    /// session client (same rationale as `complete_multipart_on`); this
    /// fresh-client variant is for the orphan-drainer (which has no session).
    pub async fn abort_multipart(&self, key: &str, upload_id: &UploadId) -> S3Result<()> {
        let session = self.open_upload_session();
        self.abort_multipart_on(&session, key, upload_id).await
    }

    /// Abort a multipart upload, reusing the caller's warm pooled
    /// [`UploadSession`] instead of opening a cold connection. Used by the
    /// upload path's error-cleanup so the abort that frees billed parts does not
    /// itself pay the post-upload cold-connect latency.
    pub async fn abort_multipart_on(
        &self,
        session: &UploadSession,
        key: &str,
        upload_id: &UploadId,
    ) -> S3Result<()> {
        let client = &session.0;
        let stored = self.config.object_key(key);
        let (base_url, host, canonical_uri) = self.object_url(&stored);
        let query = vec![("uploadId".to_string(), upload_id.0.clone())];
        let headers = self.signed_headers(
            "DELETE",
            &host,
            &canonical_uri,
            &query,
            EMPTY_PAYLOAD_SHA256,
            &[],
        );
        let cq = signer::canonical_query(&query);
        let url = format!("{base_url}?{cq}");
        let mut req = client.request(Method::DELETE, &url)?;
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str()).map_err(map_build_err)?;
        }
        // Multipart CONTROL op → generous control-op send timeout.
        let resp = self
            .send_built_with_timeout(req, self.config.timeouts.send_control)
            .await?;
        let status = resp.status().as_u16();
        match status {
            200 | 204 | 404 => {
                drop(resp);
                Ok(())
            }
            other => Err(self.error_from_response(other, resp).await),
        }
    }

    /// Drain the process-thread-local orphaned-multipart queue, issuing a
    /// best-effort `abort_multipart` for each entry.
    ///
    /// The queue is populated by a `Drop` guard when a multipart upload future
    /// is DROP-cancelled mid-flight (wall-timeout cancel, client disconnect,
    /// LRU eviction) — a `Drop` cannot await, so it only enqueues. This method
    /// is the async drainer that performs the abort the `Drop` could not. Call
    /// it from a background task or at the top of the next storage op.
    ///
    /// The queued key is the LOGICAL key (pre-`config.prefix`), the same key
    /// the guard's owner passed to `create_multipart`; the abort re-joins the
    /// configured prefix exactly as the original upload did. Returns the number
    /// of uploads for which the abort succeeded; failures are logged and left
    /// for an S3 lifecycle rule (the queue is best-effort, not a durable log).
    pub async fn drain_orphaned_uploads(&self) -> usize {
        let orphans = crate::orphan::take_orphans();
        let mut aborted = 0;
        for (logical_key, upload_id) in orphans {
            match self.abort_multipart(&logical_key, &upload_id).await {
                Ok(()) => aborted += 1,
                Err(e) => {
                    tracing::warn!(
                        key = %logical_key,
                        upload_id = %upload_id.0,
                        error = %e,
                        "best-effort abort of an orphaned multipart upload failed; \
                         leaving it for an S3 lifecycle rule",
                    );
                }
            }
        }
        aborted
    }

    /// List in-progress multipart uploads under `key_prefix`, returning their
    /// `(stored_key, upload_id)` pairs. Primarily a test/diagnostic aid for
    /// asserting that an aborted upload leaves no orphaned parts. The prefix is
    /// joined with the configured `config.prefix` like any object key.
    pub async fn list_multipart_uploads(
        &self,
        key_prefix: &str,
    ) -> S3Result<Vec<(String, String)>> {
        let stored_prefix = self.config.object_key(key_prefix);
        let query = vec![
            ("uploads".to_string(), String::new()),
            ("prefix".to_string(), stored_prefix),
        ];
        let (url, host, canonical_uri, _cq) = self.bucket_url(&query);
        let headers = self.signed_headers(
            "GET",
            &host,
            &canonical_uri,
            &query,
            EMPTY_PAYLOAD_SHA256,
            &[],
        );
        let client = new_client();
        let resp = self
            .send(client.request(Method::GET, &url)?, headers)
            .await?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(self.error_from_response(status, resp).await);
        }
        let xml = self
            .read_body_capped(resp, self.config.max_list_entries as u64 * 4096 + 65_536)
            .await?;
        parse_list_multipart_uploads(&xml)
    }

    fn append_sse_headers(&self, extra: &mut Vec<(String, String)>) {
        match &self.config.sse {
            SseMode::None => {}
            SseMode::SseS3 => extra.push((
                "x-amz-server-side-encryption".to_string(),
                "AES256".to_string(),
            )),
            SseMode::SseKms(key) => {
                extra.push((
                    "x-amz-server-side-encryption".to_string(),
                    "aws:kms".to_string(),
                ));
                extra.push((
                    "x-amz-server-side-encryption-aws-kms-key-id".to_string(),
                    key.clone(),
                ));
            }
        }
    }

    // --------------------------------------------------------------------
    // Request execution helpers
    // --------------------------------------------------------------------

    /// Attach headers to a builder and send under the send timeout.
    async fn send(
        &self,
        mut req: cyper::RequestBuilder,
        headers: Vec<(String, String)>,
    ) -> S3Result<cyper::Response> {
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str()).map_err(map_build_err)?;
        }
        self.send_built(req).await
    }

    /// Send a fully-built request under the base send timeout.
    async fn send_built(&self, req: cyper::RequestBuilder) -> S3Result<cyper::Response> {
        self.send_built_with_timeout(req, self.config.timeouts.send)
            .await
    }

    /// Send a fully-built request under an explicit send timeout. The
    /// body-carrying `UploadPart` path passes a size-scaled deadline (see
    /// [`S3Timeouts::send_for_body`](crate::config::S3Timeouts::send_for_body));
    /// every other op uses the base `send`.
    async fn send_built_with_timeout(
        &self,
        req: cyper::RequestBuilder,
        send_to: Duration,
    ) -> S3Result<cyper::Response> {
        match compio::time::timeout(send_to, req.send()).await {
            Err(_) => Err(S3Error::timeout("send timeout")),
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => Err(S3Error::from(e)),
        }
    }

    /// Read a response body fully, enforcing a cap, under the total body
    /// timeout. On cap breach / timeout / read error the response is dropped.
    async fn read_body_capped(&self, resp: cyper::Response, cap: u64) -> S3Result<Bytes> {
        let body_to = self.config.timeouts.body;
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut total: u64 = 0;
        loop {
            let next = compio::time::timeout(body_to, stream.next()).await;
            let chunk = match next {
                Err(_) => return Err(S3Error::timeout("body read timeout")),
                Ok(None) => break,
                Ok(Some(Ok(c))) => c,
                Ok(Some(Err(e))) => return Err(S3Error::from(e)),
            };
            total += chunk.len() as u64;
            if total > cap {
                return Err(S3Error::TooLarge {
                    limit: cap,
                    observed: Some(total),
                });
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(Bytes::from(buf))
    }

    /// Map a non-success response to a typed error, draining a capped error
    /// body for diagnostics, then dropping the response.
    async fn error_from_response(&self, status: u16, resp: cyper::Response) -> S3Error {
        let body = self
            .read_body_capped(resp, ERROR_BODY_CAP)
            .await
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        S3Error::from_status(status, &body)
    }
}

// ------------------------------------------------------------------------
// free helpers
// ------------------------------------------------------------------------

/// A fresh request-scoped cyper client on the current compio thread.
fn new_client() -> cyper::Client {
    cyper::Client::new()
}

/// State carried through the `get_stream` body `unfold`: the inner body stream,
/// an opaque "keep-alive" payload the stream must outlive (the fresh per-op
/// `cyper::Client`, whose connection pool dies with the body — the H1
/// invariant), and a latch that ends the stream after the first error/timeout.
struct TimeoutStreamState<S, K> {
    inner: S,
    _keep_alive: K,
    done: bool,
}

/// Wrap a body stream so each chunk pull is bounded by `body_to`, carrying
/// `keep_alive` (the fresh per-op client) alive for the body's lifetime.
///
/// A stall longer than `body_to` yields exactly one terminal
/// `Retryable`/timeout item, after which the stream ends — it can never wedge a
/// downstream consumer indefinitely. Generic over the keep-alive payload so the
/// adapter is unit-testable without a live `cyper::Client`.
fn timeout_body_stream<S, K>(
    inner: S,
    keep_alive: K,
    body_to: Duration,
) -> impl futures::Stream<Item = S3Result<Bytes>> + 'static
where
    S: futures::Stream<Item = S3Result<Bytes>> + Unpin + 'static,
    K: 'static,
{
    let state = TimeoutStreamState {
        inner,
        _keep_alive: keep_alive,
        done: false,
    };
    futures::stream::unfold(state, move |mut st| async move {
        if st.done {
            return None;
        }
        match compio::time::timeout(body_to, st.inner.next()).await {
            Err(_) => {
                st.done = true;
                Some((Err(S3Error::timeout("body read timeout")), st))
            }
            Ok(None) => None,
            Ok(Some(Ok(b))) => Some((Ok(b), st)),
            Ok(Some(Err(e))) => {
                st.done = true;
                Some((Err(e), st))
            }
        }
    })
}

/// Strip the configured internal key prefix (`<prefix>/`) from a server-supplied
/// list key, returning the logical key.
///
/// `key` is **server-controlled**, so this MUST NOT byte-index-slice it: a raw
/// `key[n..]` panics (worker crash) when `n` lands mid-UTF-8. `strip_prefix`
/// only ever splits on a real prefix boundary. A key that does not begin with
/// the expected prefix is a protocol violation (the listing was scoped by that
/// prefix), surfaced as a terminal `InvalidResponse` rather than a panic.
fn strip_internal_prefix(key: &str, internal_prefix: Option<&str>) -> S3Result<String> {
    match internal_prefix {
        None => Ok(key.to_string()),
        Some(pfx) => key.strip_prefix(pfx).map(str::to_string).ok_or_else(|| {
            S3Error::InvalidResponse(format!("listed key {key} lacks internal prefix"))
        }),
    }
}

fn map_build_err(e: cyper::Error) -> S3Error {
    // A request-build failure (bad header value, bad URL) is deterministic —
    // it will recur on every retry — so it is a *terminal* transport error,
    // not a transient one. Classifying it retryable would burn the whole
    // attempt budget + backoff for nothing.
    S3Error::TransportTerminal(format!("request build: {e}"))
}

/// Read a single header value as an owned `String`.
fn header(resp: &cyper::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Parse the mandatory object metadata from a successful GET/HEAD response.
fn parse_object_meta(resp: &cyper::Response) -> S3Result<ObjectMeta> {
    let len = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| resp.content_length())
        .ok_or_else(|| S3Error::InvalidResponse("missing/invalid Content-Length".into()))?;
    let content_type = header(resp, "content-type");
    let lm_raw = header(resp, "last-modified")
        .ok_or_else(|| S3Error::InvalidResponse("missing Last-Modified".into()))?;
    let last_modified = httpdate::parse_http_date(&lm_raw)
        .map_err(|_| S3Error::InvalidResponse(format!("invalid Last-Modified: {lm_raw}")))?;
    let user_sha256 = header(resp, "x-amz-meta-sha256");
    Ok(ObjectMeta {
        len,
        content_type,
        last_modified,
        user_sha256,
    })
}

/// Parse an RFC3339 / ISO-8601 UTC timestamp (S3 `LastModified` in LIST) into a
/// `SystemTime`.
fn parse_rfc3339(s: &str) -> S3Result<SystemTime> {
    use time::format_description::well_known::Rfc3339;
    let odt = time::OffsetDateTime::parse(s, &Rfc3339)
        .map_err(|_| S3Error::InvalidResponse(format!("invalid LastModified: {s}")))?;
    let unix_nanos = odt.unix_timestamp_nanos();
    let nanos = u64::try_from(unix_nanos).map_err(|_| {
        S3Error::InvalidResponse(format!("out-of-range LastModified: {s}"))
    })?;
    Ok(SystemTime::UNIX_EPOCH + Duration::from_nanos(nanos))
}

/// Encode a single path segment (bucket name) with S3 path rules (no slash
/// inside a bucket name, but encode any unsafe bytes).
fn encode_segment(s: &str) -> String {
    signer::encode_query_component(s)
}

/// Standard-alphabet, padded base64 (for `x-amz-checksum-sha256`).
fn base64_std(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::S3Config;

    fn aws_client() -> S3Client {
        let cfg = S3Config::parse_url("s3://bucket/data?region=us-east-1").unwrap();
        S3Client::new(cfg, S3Credentials::new("AKID", "secret", None))
    }

    #[test]
    fn object_url_virtual_style() {
        let c = aws_client();
        let (url, host, canon) = c.object_url(&c.config.object_key("a/b c"));
        assert_eq!(host, "bucket.s3.us-east-1.amazonaws.com");
        assert_eq!(canon, "/data/a/b%20c");
        assert_eq!(url, "https://bucket.s3.us-east-1.amazonaws.com/data/a/b%20c");
    }

    #[test]
    fn object_url_path_style_minio() {
        let cfg = S3Config::parse_url(
            "s3://bucket/data?provider=minio&endpoint=http://127.0.0.1:9000&region=us-east-1&style=path&dev_http=true",
        )
        .unwrap();
        let c = S3Client::new(cfg, S3Credentials::new("AKID", "secret", None));
        let (url, host, canon) = c.object_url(&c.config.object_key("k"));
        assert_eq!(host, "127.0.0.1:9000");
        assert_eq!(canon, "/bucket/data/k");
        assert_eq!(url, "http://127.0.0.1:9000/bucket/data/k");
    }

    #[test]
    fn pct2f_key_stays_literal() {
        let c = aws_client();
        let (_, _, canon) = c.object_url(&c.config.object_key("key%2Fname"));
        // %2F in the logical key must become %252F, not a slash.
        assert_eq!(canon, "/data/key%252Fname");
    }

    #[test]
    fn list_url_has_canonical_query() {
        let c = aws_client();
        let query = vec![
            ("list-type".to_string(), "2".to_string()),
            ("encoding-type".to_string(), "url".to_string()),
            ("prefix".to_string(), "data/".to_string()),
        ];
        let (url, _host, canon, cq) = c.bucket_url(&query);
        assert_eq!(canon, "/");
        // Sorted by encoded key.
        assert_eq!(cq, "encoding-type=url&list-type=2&prefix=data%2F");
        assert!(url.ends_with(&cq));
    }

    #[test]
    fn put_headers_include_checksum_for_aws() {
        let c = aws_client();
        let extra = c.put_meta_headers(b"hello", &PutOptions::default());
        assert!(extra.iter().any(|(k, _)| k == "x-amz-checksum-sha256"));
        assert!(extra.iter().any(|(k, v)| k == "content-type" && v == "application/octet-stream"));
    }

    #[test]
    fn put_headers_sse_s3() {
        let cfg = S3Config::parse_url("s3://bucket/d?region=us-east-1&sse=sse-s3").unwrap();
        let c = S3Client::new(cfg, S3Credentials::new("a", "b", None));
        let extra = c.put_meta_headers(b"x", &PutOptions::default());
        assert!(extra
            .iter()
            .any(|(k, v)| k == "x-amz-server-side-encryption" && v == "AES256"));
    }

    #[test]
    fn put_headers_sse_kms() {
        let cfg =
            S3Config::parse_url("s3://bucket/d?region=us-east-1&sse=sse-kms:alias/k").unwrap();
        let c = S3Client::new(cfg, S3Credentials::new("a", "b", None));
        let extra = c.put_meta_headers(b"x", &PutOptions::default());
        assert!(extra
            .iter()
            .any(|(k, v)| k == "x-amz-server-side-encryption" && v == "aws:kms"));
        assert!(extra
            .iter()
            .any(|(k, v)| k == "x-amz-server-side-encryption-aws-kms-key-id" && v == "alias/k"));
    }

    #[test]
    fn no_checksum_header_for_r2() {
        let cfg = S3Config::parse_url(
            "s3://bucket/d?provider=r2&endpoint=https://x.r2.cloudflarestorage.com&region=auto&style=path",
        )
        .unwrap();
        let c = S3Client::new(cfg, S3Credentials::new("a", "b", None));
        let extra = c.put_meta_headers(b"x", &PutOptions::default());
        assert!(!extra.iter().any(|(k, _)| k == "x-amz-checksum-sha256"));
    }

    #[test]
    fn signed_headers_include_authorization_and_amz_date() {
        let c = aws_client();
        let (_, host, canon) = c.object_url(&c.config.object_key("k"));
        let headers =
            c.signed_headers("GET", &host, &canon, &[], EMPTY_PAYLOAD_SHA256, &[]);
        assert!(headers.iter().any(|(k, _)| k == "authorization"));
        assert!(headers.iter().any(|(k, _)| k == "x-amz-date"));
        assert!(headers.iter().any(|(k, _)| k == "x-amz-content-sha256"));
    }

    #[test]
    fn parse_rfc3339_ok() {
        let t = parse_rfc3339("2015-08-30T12:36:00.000Z").unwrap();
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, 1_440_938_160);
    }

    #[test]
    fn base64_std_padded() {
        assert_eq!(base64_std(b"foo"), "Zm9v");
        assert_eq!(base64_std(&[0u8; 32]).len(), 44);
    }

    #[test]
    fn strip_internal_prefix_no_prefix_passthrough() {
        assert_eq!(strip_internal_prefix("a/b", None).unwrap(), "a/b");
    }

    #[test]
    fn strip_internal_prefix_normal() {
        assert_eq!(
            strip_internal_prefix("data/app/obj", Some("data/")).unwrap(),
            "app/obj"
        );
    }

    #[compio::test]
    async fn timeout_body_stream_fires_on_stall() {
        use futures::StreamExt;
        // A producer that delivers one chunk, then STALLS far longer than the
        // per-chunk body timeout. Without the timeout wrapper the consumer
        // would hang forever; with it, the second pull yields a terminal
        // timeout error and the stream then ends.
        let slow = futures::stream::unfold(0u32, |i| async move {
            match i {
                0 => Some((Ok(Bytes::from_static(b"first")), 1u32)),
                1 => {
                    compio::time::sleep(Duration::from_secs(60)).await;
                    Some((Ok(Bytes::from_static(b"never")), 2u32))
                }
                _ => None,
            }
        });
        let slow = Box::pin(slow); // make it Unpin for the adapter bound
        // Consumers (e.g. plugin-storage `S3Chunks`) pin the returned stream;
        // do the same here.
        let mut s = Box::pin(timeout_body_stream(slow, (), Duration::from_millis(50)));
        // First chunk arrives promptly.
        let first = s.next().await.expect("first item");
        assert_eq!(first.unwrap(), Bytes::from_static(b"first"));
        // Second pull stalls past the 50ms deadline → terminal timeout.
        let second = s.next().await.expect("timeout item");
        match second {
            Err(S3Error::Retryable { status: None, .. }) => {}
            other => panic!("expected a body-read timeout, got {other:?}"),
        }
        // Stream ends after the terminal error (the `done` latch).
        assert!(s.next().await.is_none(), "stream must end after a timeout");
    }

    #[test]
    fn strip_internal_prefix_multibyte_non_boundary_does_not_panic() {
        // Regression: a server-supplied key whose byte at `prefix.len()` is in
        // the MIDDLE of a multi-byte UTF-8 char would panic a raw
        // `key[strip_len..]` slice. `strip_prefix` splits on a real boundary,
        // so a key that begins with the prefix is stripped cleanly...
        let key = "p/é-data"; // 'é' is 2 bytes; raw index would have been unsafe
        assert_eq!(strip_internal_prefix(key, Some("p/")).unwrap(), "é-data");

        // ...and a key that does NOT begin with the prefix (e.g. a malicious /
        // buggy server returning an out-of-scope key) yields a terminal error,
        // never a panic, even when the divergence is mid-multibyte.
        let rogue = "☃other/obj";
        let err = strip_internal_prefix(rogue, Some("p/")).unwrap_err();
        assert!(matches!(err, S3Error::InvalidResponse(_)));
    }
}
