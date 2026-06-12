//! `S3Config` and `s3://bucket/prefix?…` URL parsing.
//!
//! Parses the storage-location URL into a validated, non-secret configuration:
//! bucket, internal prefix, provider profile, endpoint, region, addressing
//! style, `dev_http`, checksum mode, SSE mode, and list caps. Credentials and
//! timeout defaults are layered on separately by the caller.
//!
//! The full grammar is the proposal "Config / Shared parser" section. This PR
//! ships the `compio-s3`-local parser; the shared `zeroship_core::object_store`
//! wrapper is wired in a later PR and delegates here.

use std::time::Duration;

use url::Url;

use crate::error::S3Error;

/// Object-addressing style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressingStyle {
    /// `https://bucket.endpoint/key` — default for AWS.
    Virtual,
    /// `https://endpoint/bucket/key` — default for custom endpoints / `MinIO`.
    Path,
}

/// Provider profile. Drives defaults (checksum, addressing, SSE validation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Amazon S3.
    Aws,
    /// Cloudflare R2.
    R2,
    /// `MinIO` (typically local, path-style, `dev_http`).
    Minio,
    /// Any other S3-compatible endpoint.
    Generic,
}

/// Content-checksum mode for PUT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumMode {
    /// No `x-amz-checksum-*` header.
    None,
    /// Send `x-amz-checksum-sha256` (base64 of the body SHA-256).
    Sha256,
}

/// Server-side-encryption mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseMode {
    /// Send no SSE headers; rely on bucket/provider default.
    None,
    /// `x-amz-server-side-encryption: AES256` (SSE-S3).
    SseS3,
    /// `x-amz-server-side-encryption: aws:kms` with the given key id/alias.
    SseKms(String),
}

/// Connection / timeout / cap knobs. Resolved with sane defaults.
#[derive(Debug, Clone)]
pub struct S3Timeouts {
    /// Wall timeout wrapping `send()` (DNS/connect/TLS/upload/response headers).
    pub send: Duration,
    /// Total response-body read timeout.
    pub body: Duration,
}

impl Default for S3Timeouts {
    fn default() -> Self {
        Self {
            send: Duration::from_secs(30),
            body: Duration::from_secs(300),
        }
    }
}

/// Fully-resolved, non-secret S3 configuration.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// Bucket name.
    pub bucket: String,
    /// Internal key prefix (no leading/trailing slash); `None` if empty.
    pub prefix: Option<String>,
    /// Provider profile.
    pub provider: Provider,
    /// Endpoint base URL (scheme + host + optional port); `None` => AWS default
    /// `https://s3.<region>.amazonaws.com`.
    pub endpoint: Option<String>,
    /// AWS region (or `auto` for R2).
    pub region: String,
    /// Addressing style.
    pub style: AddressingStyle,
    /// Whether plaintext HTTP is permitted (loopback `MinIO` only).
    pub dev_http: bool,
    /// Checksum mode.
    pub checksum: ChecksumMode,
    /// SSE mode.
    pub sse: SseMode,
    /// Cap on aggregated `plugin-storage` list entries.
    pub max_list_entries: usize,
    /// Timeouts.
    pub timeouts: S3Timeouts,
}

impl S3Config {
    /// Parse an `s3://bucket/prefix?query` URL into a validated config.
    ///
    /// # Errors
    /// Returns `S3Error::InvalidResponse` (reused as the config-parse error
    /// channel in this PR) on any malformed/contradictory input.
    pub fn parse_url(input: &str) -> Result<Self, S3Error> {
        let url = Url::parse(input).map_err(|e| cfg_err(format!("invalid url: {e}")))?;
        if url.scheme() != "s3" {
            return Err(cfg_err(format!(
                "expected s3:// scheme, got {}://",
                url.scheme()
            )));
        }

        let bucket = url
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| cfg_err("missing bucket (s3://<bucket>/…)"))?
            .to_string();

        let prefix = parse_prefix(input, &bucket)?;
        let params = parse_query(&url)?;

        // provider (may be inferred from endpoint).
        let endpoint = params.get("endpoint").cloned();
        let provider = match params.get("provider").map(String::as_str) {
            Some("aws") => Provider::Aws,
            Some("r2") => Provider::R2,
            Some("minio") => Provider::Minio,
            Some("generic") => Provider::Generic,
            Some(other) => return Err(cfg_err(format!("unknown provider: {other}"))),
            None => infer_provider(endpoint.as_deref(), &params),
        };

        let dev_http = match params.get("dev_http").map(String::as_str) {
            None | Some("false") => false,
            Some("true") => true,
            Some(other) => return Err(cfg_err(format!("invalid dev_http: {other}"))),
        };

        // region: required unless region=auto (R2 only).
        let region = match params.get("region").map(String::as_str) {
            Some("auto") => {
                if provider != Provider::R2 {
                    return Err(cfg_err("region=auto is only valid for provider=r2"));
                }
                "auto".to_string()
            }
            Some(r) if !r.is_empty() => r.to_string(),
            _ => return Err(cfg_err("region is required (e.g. region=us-east-1)")),
        };

        // endpoint requirements.
        if endpoint.is_none() && matches!(provider, Provider::R2 | Provider::Minio) {
            return Err(cfg_err("endpoint is required for provider=r2/minio"));
        }
        if let Some(ep) = &endpoint {
            validate_endpoint(ep, dev_http)?;
        } else if dev_http {
            return Err(cfg_err("dev_http=true requires a http:// endpoint"));
        }

        // addressing style.
        let style = match params.get("style").map(String::as_str) {
            Some("virtual") => AddressingStyle::Virtual,
            Some("path") => AddressingStyle::Path,
            Some(other) => return Err(cfg_err(format!("invalid style: {other}"))),
            None => {
                if endpoint.is_some() {
                    AddressingStyle::Path
                } else {
                    AddressingStyle::Virtual
                }
            }
        };
        if style == AddressingStyle::Virtual && bucket.contains('.') {
            return Err(cfg_err(
                "bucket contains '.'; virtual-host style is unsafe — use style=path",
            ));
        }

        // checksum (provider default).
        let checksum = match params.get("checksum").map(String::as_str) {
            Some("none") => ChecksumMode::None,
            Some("sha256") => ChecksumMode::Sha256,
            Some(other) => return Err(cfg_err(format!("invalid checksum: {other}"))),
            None => match provider {
                Provider::Aws | Provider::Minio => ChecksumMode::Sha256,
                Provider::R2 | Provider::Generic => ChecksumMode::None,
            },
        };

        // sse (validated against provider).
        let sse = parse_sse(params.get("sse").map(String::as_str), provider)?;

        let max_list_entries = match params.get("max_list_entries") {
            Some(v) => v
                .parse::<usize>()
                .map_err(|_| cfg_err(format!("invalid max_list_entries: {v}")))?,
            None => 10_000,
        };

        Ok(Self {
            bucket,
            prefix,
            provider,
            endpoint,
            region,
            style,
            dev_http,
            checksum,
            sse,
            max_list_entries,
            timeouts: S3Timeouts::default(),
        })
    }

    /// Build the host and base URL (<scheme://host>[:port]) plus the object-key
    /// path prefix for a given storage key, honouring addressing style.
    ///
    /// Returns `(scheme, host, base_path)` where `base_path` already contains a
    /// leading `/` and, for path-style, the bucket segment.
    ///
    /// # Panics
    /// Never panics in practice: the stored `endpoint` is validated as a URL by
    /// [`S3Config::parse_url`] before it reaches here. The internal `expect`
    /// upholds that invariant.
    #[must_use]
    pub fn endpoint_parts(&self) -> EndpointParts {
        // Determine scheme/host/port from endpoint or AWS default.
        let (scheme, raw_host) = if let Some(ep) = &self.endpoint {
            let u = Url::parse(ep).expect("endpoint validated at parse time");
            let scheme = u.scheme().to_string();
            let host = match u.port() {
                Some(p) => format!("{}:{}", u.host_str().unwrap_or(""), p),
                None => u.host_str().unwrap_or("").to_string(),
            };
            (scheme, host)
        } else {
            // AWS default endpoint.
            (
                "https".to_string(),
                format!("s3.{}.amazonaws.com", self.region),
            )
        };

        match self.style {
            AddressingStyle::Virtual => EndpointParts {
                scheme,
                host: format!("{}.{}", self.bucket, raw_host),
                bucket_in_path: false,
            },
            AddressingStyle::Path => EndpointParts {
                scheme,
                host: raw_host,
                bucket_in_path: true,
            },
        }
    }

    /// The full stored object key for a logical storage key: `prefix/key`.
    #[must_use]
    pub fn object_key(&self, key: &str) -> String {
        match &self.prefix {
            Some(p) => format!("{p}/{key}"),
            None => key.to_string(),
        }
    }
}

/// Resolved endpoint host/scheme and whether the bucket lives in the path.
#[derive(Debug, Clone)]
pub struct EndpointParts {
    /// `http` or `https`.
    pub scheme: String,
    /// Host[:port] used both for the URL authority and the signed `host` header.
    pub host: String,
    /// If true, the object path is prefixed with `/{bucket}`.
    pub bucket_in_path: bool,
}

fn cfg_err(msg: impl Into<String>) -> S3Error {
    S3Error::InvalidResponse(format!("config: {}", msg.into()))
}

/// Parse and validate the URL path into a clean internal prefix.
///
/// Operates on the **raw input string** rather than `url.path()`, because the
/// `url` crate silently normalizes dot segments (`a/../b` -> `b`) — which would
/// hide the `.`/`..` the parser is required to reject.
fn parse_prefix(input: &str, bucket: &str) -> Result<Option<String>, S3Error> {
    // input is `s3://<bucket><path>?<query>`. Strip scheme + bucket, then the
    // query, leaving the raw path (which begins with `/` if present).
    let after_scheme = input
        .strip_prefix("s3://")
        .ok_or_else(|| cfg_err("expected s3:// scheme"))?;
    let after_bucket = after_scheme
        .strip_prefix(bucket)
        .ok_or_else(|| cfg_err("internal: bucket prefix mismatch"))?;
    // Drop the query component if any.
    let path = match after_bucket.find('?') {
        Some(i) => &after_bucket[..i],
        None => after_bucket,
    };
    // Strip exactly one leading slash (URL syntax). `s3://bucket//x` yields
    // path "//x" -> after stripping one leading slash, "/x" => a leading empty
    // segment, which is rejected as a repeated separator.
    if path.is_empty() {
        return Ok(None);
    }
    let raw = path.strip_prefix('/').unwrap_or(path);
    // Trim a single trailing slash on a non-empty prefix.
    let trimmed = raw.strip_suffix('/').unwrap_or(raw);
    if trimmed.is_empty() {
        // covers "", "/" (=> "" after both strips)
        return Ok(None);
    }

    let mut segments = Vec::new();
    for seg in trimmed.split('/') {
        if seg.is_empty() {
            return Err(cfg_err("repeated/empty prefix separator is not allowed"));
        }
        let decoded = percent_decode(seg)?;
        if decoded == "." || decoded == ".." {
            return Err(cfg_err("'.'/'..' prefix segment is not allowed"));
        }
        if decoded.contains('\\') {
            return Err(cfg_err("backslash in prefix is not allowed"));
        }
        if decoded.contains('/') {
            // %2F decoded into a slash — reject (would change segmentation).
            return Err(cfg_err("%2F inside a prefix segment is not allowed"));
        }
        segments.push(decoded);
    }
    Ok(Some(segments.join("/")))
}

/// Percent-decode a single path segment as UTF-8 (config ergonomics).
fn percent_decode(seg: &str) -> Result<String, S3Error> {
    let decoded = percent_encoding::percent_decode_str(seg)
        .decode_utf8()
        .map_err(|_| cfg_err("invalid UTF-8 in percent-encoded prefix"))?;
    Ok(decoded.into_owned())
}

/// Parse the query string into a map, rejecting duplicates and empty keys.
fn parse_query(url: &Url) -> Result<std::collections::BTreeMap<String, String>, S3Error> {
    let mut map = std::collections::BTreeMap::new();
    for (k, v) in url.query_pairs() {
        let k = k.into_owned();
        if k.is_empty() {
            return Err(cfg_err("empty query parameter name"));
        }
        if !KNOWN_PARAMS.contains(&k.as_str()) {
            return Err(cfg_err(format!("unknown query parameter: {k}")));
        }
        if map.insert(k.clone(), v.into_owned()).is_some() {
            return Err(cfg_err(format!("duplicate query parameter: {k}")));
        }
    }
    Ok(map)
}

const KNOWN_PARAMS: &[&str] = &[
    "provider",
    "endpoint",
    "region",
    "style",
    "dev_http",
    "checksum",
    "sse",
    "max_list_entries",
];

fn infer_provider(
    endpoint: Option<&str>,
    params: &std::collections::BTreeMap<String, String>,
) -> Provider {
    match endpoint {
        None => Provider::Aws,
        Some(ep) => {
            let host = Url::parse(ep)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_default();
            if host.ends_with(".r2.cloudflarestorage.com") {
                Provider::R2
            } else if host.ends_with(".amazonaws.com") {
                Provider::Aws
            } else if params.get("dev_http").map(String::as_str) == Some("true")
                && is_loopback_host(&host)
            {
                Provider::Minio
            } else {
                Provider::Generic
            }
        }
    }
}

fn validate_endpoint(ep: &str, dev_http: bool) -> Result<(), S3Error> {
    let u = Url::parse(ep).map_err(|e| cfg_err(format!("invalid endpoint url: {e}")))?;
    match u.scheme() {
        "https" => Ok(()),
        "http" => {
            if !dev_http {
                return Err(cfg_err("plain http endpoint requires dev_http=true"));
            }
            let host = u.host_str().unwrap_or("");
            if !is_loopback_host(host) {
                return Err(cfg_err(
                    "dev_http=true is only allowed for loopback/localhost endpoints",
                ));
            }
            Ok(())
        }
        other => Err(cfg_err(format!("unsupported endpoint scheme: {other}"))),
    }
}

/// Whether a host is loopback/localhost (`localhost`, `127.0.0.0/8`, `::1`).
fn is_loopback_host(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        return v4.octets()[0] == 127;
    }
    if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
        return v6.is_loopback();
    }
    // url stores bracketed IPv6 as the bare address; also accept "[::1]".
    let unbracketed = host.trim_start_matches('[').trim_end_matches(']');
    matches!(unbracketed.parse::<std::net::Ipv6Addr>(), Ok(v6) if v6.is_loopback())
}

fn parse_sse(raw: Option<&str>, provider: Provider) -> Result<SseMode, S3Error> {
    let mode = match raw {
        None | Some("none") => SseMode::None,
        Some("sse-s3") => SseMode::SseS3,
        Some(s) if s.starts_with("sse-kms:") => {
            let key = s.trim_start_matches("sse-kms:");
            if key.is_empty() {
                return Err(cfg_err("sse-kms requires a key id/alias"));
            }
            SseMode::SseKms(key.to_string())
        }
        Some(other) => return Err(cfg_err(format!("invalid sse: {other}"))),
    };
    if provider == Provider::R2 && !matches!(mode, SseMode::None) {
        return Err(cfg_err("provider=r2 rejects sse-s3/sse-kms"));
    }
    if matches!(mode, SseMode::SseKms(_)) && provider != Provider::Aws {
        return Err(cfg_err("sse-kms is AWS-only in v1"));
    }
    Ok(mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_minimal() {
        let c = S3Config::parse_url("s3://bucket/prefix?region=us-east-1").unwrap();
        assert_eq!(c.bucket, "bucket");
        assert_eq!(c.prefix.as_deref(), Some("prefix"));
        assert_eq!(c.provider, Provider::Aws);
        assert_eq!(c.region, "us-east-1");
        assert_eq!(c.style, AddressingStyle::Virtual);
        assert_eq!(c.checksum, ChecksumMode::Sha256);
        assert_eq!(c.sse, SseMode::None);
        let parts = c.endpoint_parts();
        assert_eq!(parts.scheme, "https");
        assert_eq!(parts.host, "bucket.s3.us-east-1.amazonaws.com");
        assert!(!parts.bucket_in_path);
    }

    #[test]
    fn empty_prefix_variants() {
        assert_eq!(
            S3Config::parse_url("s3://bucket?region=us-east-1")
                .unwrap()
                .prefix,
            None
        );
        assert_eq!(
            S3Config::parse_url("s3://bucket/?region=us-east-1")
                .unwrap()
                .prefix,
            None
        );
    }

    #[test]
    fn repeated_prefix_separator_rejected() {
        let e = S3Config::parse_url("s3://bucket//x?region=us-east-1").unwrap_err();
        assert!(format!("{e}").contains("repeated"));
    }

    #[test]
    fn nested_prefix_ok_and_trailing_slash_trimmed() {
        let c = S3Config::parse_url("s3://bucket/a/b/c/?region=us-east-1").unwrap();
        assert_eq!(c.prefix.as_deref(), Some("a/b/c"));
        assert_eq!(c.object_key("k"), "a/b/c/k");
    }

    #[test]
    fn pct2f_in_prefix_rejected() {
        let e = S3Config::parse_url("s3://bucket/a%2Fb?region=us-east-1").unwrap_err();
        assert!(format!("{e}").contains("%2F"));
    }

    #[test]
    fn dot_segment_in_prefix_rejected() {
        assert!(S3Config::parse_url("s3://bucket/a/../b?region=us-east-1").is_err());
        assert!(S3Config::parse_url("s3://bucket/./b?region=us-east-1").is_err());
    }

    #[test]
    fn unknown_and_duplicate_params_rejected() {
        assert!(S3Config::parse_url("s3://bucket/p?region=us-east-1&foo=1").is_err());
        assert!(S3Config::parse_url("s3://bucket/p?region=us-east-1&region=us-west-2").is_err());
    }

    #[test]
    fn r2_profile() {
        let c = S3Config::parse_url(
            "s3://bucket/p?provider=r2&endpoint=https://acct.r2.cloudflarestorage.com&region=auto&style=path",
        )
        .unwrap();
        assert_eq!(c.provider, Provider::R2);
        assert_eq!(c.region, "auto");
        assert_eq!(c.style, AddressingStyle::Path);
        assert_eq!(c.checksum, ChecksumMode::None);
        let parts = c.endpoint_parts();
        assert_eq!(parts.host, "acct.r2.cloudflarestorage.com");
        assert!(parts.bucket_in_path);
    }

    #[test]
    fn r2_inferred_from_endpoint() {
        let c = S3Config::parse_url(
            "s3://bucket/p?endpoint=https://acct.r2.cloudflarestorage.com&region=auto&style=path",
        )
        .unwrap();
        assert_eq!(c.provider, Provider::R2);
    }

    #[test]
    fn region_auto_rejected_outside_r2() {
        assert!(S3Config::parse_url("s3://bucket/p?region=auto").is_err());
    }

    #[test]
    fn region_required() {
        assert!(S3Config::parse_url("s3://bucket/p").is_err());
    }

    #[test]
    fn minio_profile() {
        let c = S3Config::parse_url(
            "s3://bucket/p?provider=minio&endpoint=http://127.0.0.1:9000&region=us-east-1&style=path&dev_http=true",
        )
        .unwrap();
        assert_eq!(c.provider, Provider::Minio);
        assert!(c.dev_http);
        let parts = c.endpoint_parts();
        assert_eq!(parts.scheme, "http");
        assert_eq!(parts.host, "127.0.0.1:9000");
        assert!(parts.bucket_in_path);
    }

    #[test]
    fn dev_http_requires_loopback() {
        let e = S3Config::parse_url(
            "s3://bucket/p?provider=minio&endpoint=http://example.com:9000&region=us-east-1&style=path&dev_http=true",
        )
        .unwrap_err();
        assert!(format!("{e}").contains("loopback"));
    }

    #[test]
    fn plain_http_without_dev_http_rejected() {
        assert!(S3Config::parse_url(
            "s3://bucket/p?provider=minio&endpoint=http://127.0.0.1:9000&region=us-east-1&style=path"
        )
        .is_err());
    }

    #[test]
    fn dotted_bucket_requires_path_style() {
        assert!(S3Config::parse_url("s3://my.bucket/p?region=us-east-1").is_err());
        assert!(
            S3Config::parse_url("s3://my.bucket/p?region=us-east-1&style=path").is_ok()
        );
    }

    #[test]
    fn sse_validation() {
        assert_eq!(
            S3Config::parse_url("s3://b/p?region=us-east-1&sse=sse-s3")
                .unwrap()
                .sse,
            SseMode::SseS3
        );
        assert_eq!(
            S3Config::parse_url("s3://b/p?region=us-east-1&sse=sse-kms:alias/k")
                .unwrap()
                .sse,
            SseMode::SseKms("alias/k".into())
        );
        // R2 rejects SSE-S3/KMS.
        assert!(S3Config::parse_url(
            "s3://b/p?provider=r2&endpoint=https://x.r2.cloudflarestorage.com&region=auto&style=path&sse=sse-s3"
        )
        .is_err());
        // sse-kms requires AWS.
        assert!(S3Config::parse_url(
            "s3://b/p?provider=minio&endpoint=http://127.0.0.1:9000&region=us-east-1&style=path&dev_http=true&sse=sse-kms:k"
        )
        .is_err());
    }

    #[test]
    fn checksum_override() {
        assert_eq!(
            S3Config::parse_url("s3://b/p?region=us-east-1&checksum=none")
                .unwrap()
                .checksum,
            ChecksumMode::None
        );
    }

    #[test]
    fn object_key_without_prefix() {
        let c = S3Config::parse_url("s3://bucket?region=us-east-1").unwrap();
        assert_eq!(c.object_key("a/b"), "a/b");
    }
}
