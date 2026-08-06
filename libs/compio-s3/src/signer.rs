//! Hand-rolled AWS Signature Version 4 (`SigV4`) for S3.
//!
//! `aws-sigv4` is banned in this workspace (it pulls `tokio` transitively via
//! `aws-smithy-async`). This module implements `SigV4` from the published
//! algorithm so the S3 client has zero async-runtime dependency beyond compio.
//!
//! Pipeline (per the AWS spec):
//!
//! 1. **Canonical request** —
//!    `METHOD\nCanonicalURI\nCanonicalQuery\nCanonicalHeaders\nSignedHeaders\nHashedPayload`.
//! 2. **String to sign** —
//!    `AWS4-HMAC-SHA256\nAmzDate\nScope\nSHA256(CanonicalRequest)`.
//! 3. **Signing key** — chained HMAC-SHA256:
//!    `kDate = HMAC("AWS4"+secret, date)`, `kRegion`, `kService`,
//!    `kSigning = HMAC(kService, "aws4_request")`.
//! 4. **Signature** — `HMAC(kSigning, StringToSign)` (hex).
//! 5. **Authorization header**.
//!
//! S3-specific rules honored here:
//!
//! - Object key paths are **not** normalized: repeated slashes and dot
//!   segments are preserved (S3 treats them as literal key bytes).
//! - URI-encoding uses S3's `SigV4` rules: unreserved bytes literal, everything
//!   else `%XX` uppercase, `/` preserved in the path (but encoded in query
//!   values).
//! - The actual payload SHA-256 is signed and echoed in
//!   `x-amz-content-sha256`. `UNSIGNED-PAYLOAD` / streaming chunk signing is
//!   not used.
//!
//! The signer is a **pure function over the exact header set it is handed**.
//! It injects nothing — the caller ([`crate::client`]) assembles the full
//! header list (host, `x-amz-date`, `x-amz-content-sha256`,
//! `x-amz-security-token`, plus any operation headers) before signing. This
//! lets the unit tests reproduce the published AWS `SigV4` `.creq`/`.authz`
//! vectors byte-for-byte.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::credentials::S3Credentials;

type HmacSha256 = Hmac<Sha256>;

/// SHA-256 hex of an empty body — used for GET/HEAD/DELETE payload hashes.
pub const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// The `SigV4` algorithm identifier.
const ALGORITHM: &str = "AWS4-HMAC-SHA256";
/// The terminating scope string.
const REQUEST_TYPE: &str = "aws4_request";

/// A single header to be signed. Names are matched case-insensitively and
/// lowercased during canonicalization.
#[derive(Debug, Clone)]
pub struct SignHeader {
    /// Header name (any case; canonicalized to lowercase).
    pub name: String,
    /// Header value.
    pub value: String,
}

impl SignHeader {
    /// Convenience constructor.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// All inputs required to produce a `SigV4` `Authorization` header.
///
/// `headers` must contain the **complete** set of headers to sign, including
/// `host` and `x-amz-date`. The signer does not add any header.
#[derive(Debug)]
pub struct SignRequest<'a> {
    /// HTTP method, e.g. `GET`, `PUT`.
    pub method: &'a str,
    /// Canonical URI **path only** (no query), already percent-encoded per S3
    /// rules by [`encode_path`].
    pub canonical_uri: &'a str,
    /// Decoded query parameters as `(key, value)` pairs. Encoded and sorted
    /// internally.
    pub query: &'a [(String, String)],
    /// The complete set of headers to sign (must include `host` and
    /// `x-amz-date`).
    pub headers: &'a [SignHeader],
    /// Lowercase hex SHA-256 of the payload (or [`EMPTY_PAYLOAD_SHA256`]).
    pub payload_sha256: &'a str,
    /// `x-amz-date` value (`YYYYMMDD'T'HHMMSS'Z'`), used in the string-to-sign.
    pub amz_date: &'a str,
    /// Credential-scope date (`YYYYMMDD`).
    pub scope_date: &'a str,
    /// AWS region (e.g. `us-east-1`, or `auto` for R2).
    pub region: &'a str,
    /// Service name (`s3`, or `service` for the AWS test vectors).
    pub service: &'a str,
}

/// The signer output: the `Authorization` header value plus the signed-header
/// set and signature.
#[derive(Debug, Clone)]
pub struct SignedAuth {
    /// Full `Authorization` header value.
    pub authorization: String,
    /// `;`-joined lowercased signed header names.
    pub signed_headers: String,
    /// The computed signature hex (exposed for tests / debugging).
    pub signature: String,
}

/// S3 `SigV4` path encoding: percent-encode each byte except RFC 3986 unreserved
/// (`A-Za-z0-9-._~`) and the path separator `/`, which stays literal.
#[must_use]
pub fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        if is_unreserved(b) || b == b'/' {
            out.push(b as char);
        } else {
            push_pct(&mut out, b);
        }
    }
    out
}

/// S3 `SigV4` query-component encoding: percent-encode every byte except RFC 3986
/// unreserved. Unlike the path, `/` is **not** preserved here.
#[must_use]
pub fn encode_query_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            push_pct(&mut out, b);
        }
    }
    out
}

const fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn push_pct(out: &mut String, b: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('%');
    out.push(HEX[(b >> 4) as usize] as char);
    out.push(HEX[(b & 0xf) as usize] as char);
}

/// Build the canonical query string: encode each key and value, then sort by
/// encoded key (and by encoded value to break ties).
#[must_use]
pub fn canonical_query(query: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (encode_query_component(k), encode_query_component(v)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Collapse internal whitespace runs in a header value and trim ends, per the
/// `SigV4` trimall rule (applies to unquoted values).
fn trim_header_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut prev_space = false;
    for ch in v.trim().chars() {
        if ch == ' ' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

/// SHA-256 hex of a byte slice.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Derive the `SigV4` signing key by chaining HMAC-SHA256.
fn signing_key(secret: &str, scope_date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), scope_date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    hmac(&k_service, REQUEST_TYPE.as_bytes())
}

impl SignRequest<'_> {
    /// Compute the `SigV4` `Authorization` header and signed-header set over the
    /// exact header list supplied (no headers are injected).
    #[must_use]
    pub fn sign(&self, creds: &S3Credentials) -> SignedAuth {
        // 1. Canonical headers — lowercase names, trimmed values, sorted by
        //    name. The caller has already assembled the full header list.
        let mut canon: Vec<(String, String)> = self
            .headers
            .iter()
            .map(|h| (h.name.to_ascii_lowercase(), trim_header_value(&h.value)))
            .collect();
        canon.sort_by(|a, b| a.0.cmp(&b.0));

        let signed_headers = canon
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(";");

        let mut canonical_headers = String::new();
        for (n, v) in &canon {
            canonical_headers.push_str(n);
            canonical_headers.push(':');
            canonical_headers.push_str(v);
            canonical_headers.push('\n');
        }

        let canonical_query = canonical_query(self.query);

        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            self.method,
            self.canonical_uri,
            canonical_query,
            canonical_headers,
            signed_headers,
            self.payload_sha256,
        );

        // 2. String to sign.
        let scope = format!(
            "{}/{}/{}/{}",
            self.scope_date, self.region, self.service, REQUEST_TYPE
        );
        let hashed_canon = sha256_hex(canonical_request.as_bytes());
        let string_to_sign = format!("{ALGORITHM}\n{}\n{scope}\n{hashed_canon}", self.amz_date);

        // 3 + 4. Signing key and signature.
        let key = signing_key(
            &creds.secret_access_key,
            self.scope_date,
            self.region,
            self.service,
        );
        let signature = hex::encode(hmac(&key, string_to_sign.as_bytes()));

        // 5. Authorization header.
        let authorization = format!(
            "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            creds.access_key_id,
        );

        SignedAuth {
            authorization,
            signed_headers,
            signature,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::S3Credentials;

    // AWS SigV4 published test-suite parameters.
    const ACCESS_KEY: &str = "AKIDEXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const REGION: &str = "us-east-1";
    const SERVICE: &str = "service";
    const AMZ_DATE: &str = "20150830T123600Z";
    const SCOPE_DATE: &str = "20150830";
    const HOST: &str = "example.amazonaws.com";

    fn creds() -> S3Credentials {
        S3Credentials::new(ACCESS_KEY, SECRET, None)
    }

    /// Sign exactly host + x-amz-date with the empty payload, reproducing the
    /// AWS published vectors' header set.
    fn host_and_date() -> Vec<SignHeader> {
        vec![
            SignHeader::new("host", HOST),
            SignHeader::new("x-amz-date", AMZ_DATE),
        ]
    }

    #[test]
    fn signing_key_matches_aws_doc_example() {
        // AWS SigV4 documentation worked example (us-east-1 / iam / 20150830).
        let key = signing_key(SECRET, "20150830", "us-east-1", "iam");
        let expected = [
            196u8, 175, 177, 204, 87, 113, 216, 113, 118, 58, 57, 62, 68, 183, 3, 87, 27, 85, 204,
            40, 66, 77, 26, 94, 134, 218, 110, 211, 193, 84, 164, 185,
        ];
        assert_eq!(key, expected);
    }

    #[test]
    fn empty_payload_hash_is_correct() {
        assert_eq!(sha256_hex(b""), EMPTY_PAYLOAD_SHA256);
    }

    // ----- AWS SigV4 test-suite vectors (exact `.authz` signatures) -----

    #[test]
    fn get_vanilla() {
        let headers = host_and_date();
        let req = SignRequest {
            method: "GET",
            canonical_uri: "/",
            query: &[],
            headers: &headers,
            payload_sha256: EMPTY_PAYLOAD_SHA256,
            amz_date: AMZ_DATE,
            scope_date: SCOPE_DATE,
            region: REGION,
            service: SERVICE,
        };
        let signed = req.sign(&creds());
        assert_eq!(signed.signed_headers, "host;x-amz-date");
        assert_eq!(
            signed.signature,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        assert_eq!(
            signed.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn get_vanilla_query_order_key_case() {
        // Params must sort by encoded key: Param1 before Param2.
        let headers = host_and_date();
        let req = SignRequest {
            method: "GET",
            canonical_uri: "/",
            query: &[
                ("Param2".into(), "value2".into()),
                ("Param1".into(), "value1".into()),
            ],
            headers: &headers,
            payload_sha256: EMPTY_PAYLOAD_SHA256,
            amz_date: AMZ_DATE,
            scope_date: SCOPE_DATE,
            region: REGION,
            service: SERVICE,
        };
        assert_eq!(canonical_query(req.query), "Param1=value1&Param2=value2");
        let signed = req.sign(&creds());
        assert_eq!(
            signed.signature,
            "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"
        );
    }

    #[test]
    fn post_x_www_form_urlencoded() {
        // POST "/" with a form body; content-type signed. Per the AWS vector,
        // the canonical headers are content-type + host + x-amz-date and the
        // payload is the SHA-256 of "Param1=value1".
        let body = b"Param1=value1";
        let payload = sha256_hex(body);
        let headers = vec![
            SignHeader::new("Content-Type", "application/x-www-form-urlencoded"),
            SignHeader::new("host", HOST),
            SignHeader::new("x-amz-date", AMZ_DATE),
        ];
        let req = SignRequest {
            method: "POST",
            canonical_uri: "/",
            query: &[],
            headers: &headers,
            payload_sha256: &payload,
            amz_date: AMZ_DATE,
            scope_date: SCOPE_DATE,
            region: REGION,
            service: SERVICE,
        };
        let signed = req.sign(&creds());
        assert_eq!(
            signed.signed_headers,
            "content-type;host;x-amz-date"
        );
        assert_eq!(
            signed.signature,
            "ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a"
        );
    }

    #[test]
    fn get_header_value_trim() {
        // SigV4 trimall: leading/trailing trimmed, internal runs collapsed.
        let headers = vec![
            SignHeader::new("host", HOST),
            SignHeader::new("My-Header1", "  value1   value2  value3  "),
            SignHeader::new("x-amz-date", AMZ_DATE),
        ];
        let req = SignRequest {
            method: "GET",
            canonical_uri: "/",
            query: &[],
            headers: &headers,
            payload_sha256: EMPTY_PAYLOAD_SHA256,
            amz_date: AMZ_DATE,
            scope_date: SCOPE_DATE,
            region: REGION,
            service: SERVICE,
        };
        assert_eq!(
            trim_header_value("  value1   value2  value3  "),
            "value1 value2 value3"
        );
        let signed = req.sign(&creds());
        assert_eq!(signed.signed_headers, "host;my-header1;x-amz-date");
        // Independently verified against the `aws-sigv4` crate (throwaway
        // harness outside the workspace) for `My-Header1: value1 value2 value3`
        // — the SigV4 trimall collapse makes the two spacings identical.
        assert_eq!(
            signed.signature,
            "cfd34249e4b1c8d6b91ef74165d41a32e5fab3306300901bb65a51a73575eefd"
        );
    }

    // ----- Local encoding / canonicalization tests -----

    #[test]
    fn path_encoding_preserves_slashes_and_repeated_slashes() {
        assert_eq!(encode_path("/a/b//c"), "/a/b//c");
        assert_eq!(encode_path("/a b/c"), "/a%20b/c");
        assert_eq!(encode_path("/a+b"), "/a%2Bb");
        // %2F in a logical key must survive as literal %25 2 F, not a slash.
        assert_eq!(encode_path("/key%2Fname"), "/key%252Fname");
        assert_eq!(encode_path("/q?x#y"), "/q%3Fx%23y");
        // Unicode → UTF-8 bytes percent-encoded.
        assert_eq!(encode_path("/é"), "/%C3%A9");
    }

    #[test]
    fn query_encoding_does_not_preserve_slash() {
        assert_eq!(encode_query_component("a/b"), "a%2Fb");
        assert_eq!(encode_query_component("a b"), "a%20b");
        assert_eq!(encode_query_component(""), "");
    }

    #[test]
    fn empty_query_values_canonicalize() {
        let q = vec![("prefix".into(), String::new())];
        assert_eq!(canonical_query(&q), "prefix=");
    }

    #[test]
    fn session_token_signed_when_included_in_headers() {
        let creds = S3Credentials::new(ACCESS_KEY, SECRET, Some("TOKEN".into()));
        let headers = vec![
            SignHeader::new("host", HOST),
            SignHeader::new("x-amz-date", AMZ_DATE),
            SignHeader::new("x-amz-security-token", creds.session_token.clone().unwrap()),
        ];
        let req = SignRequest {
            method: "GET",
            canonical_uri: "/",
            query: &[],
            headers: &headers,
            payload_sha256: EMPTY_PAYLOAD_SHA256,
            amz_date: AMZ_DATE,
            scope_date: SCOPE_DATE,
            region: REGION,
            service: SERVICE,
        };
        let signed = req.sign(&creds);
        assert_eq!(
            signed.signed_headers,
            "host;x-amz-date;x-amz-security-token"
        );
    }
}
