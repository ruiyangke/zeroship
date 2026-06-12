//! `ListObjectsV2` XML response parsing via `quick-xml`.
//!
//! S3 list responses are XML; a page holds at most 1,000 keys. Requests set
//! `encoding-type=url`, so `<Key>` values are URL-encoded and must be
//! percent-decoded after XML parsing. The parser is intentionally strict about
//! required fields (`Key`, `Size`, `LastModified`) and tolerant of unknown
//! elements (forward-compatible with provider extensions).
//!
//! Also parses the `CompleteMultipartUpload` response to detect in-200-body
//! errors (S3 can return a `200 OK` whose body is an `<Error>` document).

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::error::S3Error;

/// One raw entry parsed from a list page (key still has the internal prefix and
/// is already percent-decoded to a logical UTF-8 key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawListEntry {
    /// Full object key (decoded, still internal-prefixed).
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
    /// `LastModified` (RFC3339), as the raw string for the caller to parse.
    pub last_modified: String,
}

/// A parsed `ListObjectsV2` page.
#[derive(Debug, Clone)]
pub struct RawListPage {
    /// Entries in document order.
    pub entries: Vec<RawListEntry>,
    /// Whether the result was truncated.
    pub is_truncated: bool,
    /// Continuation token for the next page, if truncated.
    pub next_continuation_token: Option<String>,
}

fn err(msg: impl Into<String>) -> S3Error {
    S3Error::InvalidResponse(format!("list xml: {}", msg.into()))
}

/// Parse a `ListObjectsV2` XML body. `encoding-type=url` is assumed: `<Key>`
/// text is percent-decoded as UTF-8.
pub fn parse_list_objects_v2(xml: &[u8]) -> Result<RawListPage, S3Error> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut entries = Vec::new();
    let mut is_truncated = false;
    let mut next_token: Option<String> = None;

    // Per-Contents accumulators.
    let mut in_contents = false;
    let mut cur_key: Option<String> = None;
    let mut cur_size: Option<u64> = None;
    let mut cur_modified: Option<String> = None;

    // The element whose text we are currently capturing.
    let mut path: Vec<String> = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "Contents" {
                    in_contents = true;
                    cur_key = None;
                    cur_size = None;
                    cur_modified = None;
                }
                path.push(name);
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                path.pop();
                if name == "Contents" {
                    in_contents = false;
                    let key = cur_key.take().ok_or_else(|| err("Contents missing Key"))?;
                    let size = cur_size.take().ok_or_else(|| err("Contents missing Size"))?;
                    let last_modified = cur_modified
                        .take()
                        .ok_or_else(|| err("Contents missing LastModified"))?;
                    entries.push(RawListEntry {
                        key,
                        size,
                        last_modified,
                    });
                }
            }
            Ok(Event::Text(t)) => {
                let text = t
                    .decode()
                    .map_err(|_| err("invalid UTF-8 in text node"))?
                    .into_owned();
                let cur = path.last().map_or("", String::as_str);
                match cur {
                    "IsTruncated" if path_is_top_level(&path) => {
                        is_truncated = parse_bool(&text)?;
                    }
                    "NextContinuationToken" if path_is_top_level(&path) => {
                        if !text.is_empty() {
                            next_token = Some(text);
                        }
                    }
                    "Key" if in_contents => {
                        cur_key = Some(percent_decode_key(&text)?);
                    }
                    "Size" if in_contents => {
                        cur_size = Some(
                            text.parse::<u64>()
                                .map_err(|_| err(format!("invalid Size: {text}")))?,
                        );
                    }
                    "LastModified" if in_contents => {
                        cur_modified = Some(text);
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(err(format!("xml error: {e}"))),
            _ => {}
        }
        buf.clear();
    }

    Ok(RawListPage {
        entries,
        is_truncated,
        next_continuation_token: next_token,
    })
}

/// `IsTruncated`/`NextContinuationToken` are direct children of the root
/// `ListBucketResult` element. A path of length 2 (`[ListBucketResult, X]`)
/// is top-level; anything deeper is nested and ignored.
const fn path_is_top_level(path: &[String]) -> bool {
    path.len() == 2
}

fn local_name(raw: &[u8]) -> String {
    // Strip an XML namespace prefix (`ns:Local` -> `Local`).
    let s = String::from_utf8_lossy(raw);
    match s.rsplit(':').next() {
        Some(local) => local.to_string(),
        None => s.into_owned(),
    }
}

fn parse_bool(s: &str) -> Result<bool, S3Error> {
    match s {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(err(format!("invalid boolean: {other}"))),
    }
}

fn percent_decode_key(s: &str) -> Result<String, S3Error> {
    percent_encoding::percent_decode_str(s)
        .decode_utf8()
        .map(std::borrow::Cow::into_owned)
        .map_err(|_| err("invalid UTF-8 after percent-decoding Key"))
}

/// Inspect a `CompleteMultipartUpload` response body. S3 may answer `200 OK`
/// whose body is an `<Error>` document; treat that as a typed error. A
/// `<CompleteMultipartUploadResult>` is success.
pub fn check_complete_multipart_response(xml: &[u8]) -> Result<(), S3Error> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_error = false;
    let mut code = String::new();
    let mut message = String::new();
    let mut path: Vec<String> = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "Error" {
                    in_error = true;
                }
                if name == "CompleteMultipartUploadResult" {
                    return Ok(());
                }
                path.push(name);
            }
            Ok(Event::End(_)) => {
                path.pop();
            }
            Ok(Event::Text(t)) => {
                if in_error {
                    let text = t.decode().unwrap_or_default().into_owned();
                    match path.last().map(String::as_str) {
                        Some("Code") => code = text,
                        Some("Message") => message = text,
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(err(format!("xml error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    if in_error {
        return Err(S3Error::InvalidResponse(format!(
            "complete-multipart in-body error: {code}: {message}"
        )));
    }
    // Neither a result nor an error element — treat as malformed.
    Err(err("complete-multipart response had no result or error element"))
}

/// Build the `CompleteMultipartUpload` request XML body from ordered parts.
#[must_use]
pub fn build_complete_multipart_body(parts: &[(u32, String)]) -> String {
    use std::fmt::Write as _;
    let mut s = String::from("<CompleteMultipartUpload>");
    for (n, etag) in parts {
        // ETags may contain quotes; XML-escape minimally.
        let etag = xml_escape(etag);
        let _ = write!(
            s,
            "<Part><PartNumber>{n}</PartNumber><ETag>{etag}</ETag></Part>"
        );
    }
    s.push_str("</CompleteMultipartUpload>");
    s
}

/// Extract `<UploadId>` from a `CreateMultipartUpload` response.
pub fn parse_upload_id(xml: &[u8]) -> Result<String, S3Error> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut upload_id: Option<String> = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => path.push(local_name(e.name().as_ref())),
            Ok(Event::End(_)) => {
                path.pop();
            }
            Ok(Event::Text(t)) => {
                if path.last().map(String::as_str) == Some("UploadId") {
                    upload_id = Some(t.decode().unwrap_or_default().into_owned());
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(err(format!("xml error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    upload_id.filter(|s| !s.is_empty()).ok_or_else(|| err("missing UploadId"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>bucket</Name>
  <Prefix>p/</Prefix>
  <KeyCount>2</KeyCount>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>p/a%2Fb.txt</Key>
    <LastModified>2015-08-30T12:36:00.000Z</LastModified>
    <ETag>"abc"</ETag>
    <Size>42</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <Contents>
    <Key>p/with%20space</Key>
    <LastModified>2015-08-30T12:37:00.000Z</LastModified>
    <Size>7</Size>
  </Contents>
</ListBucketResult>"#;

    #[test]
    fn parses_multiple_contents_and_url_decodes_keys() {
        let page = parse_list_objects_v2(SAMPLE.as_bytes()).unwrap();
        assert!(!page.is_truncated);
        assert_eq!(page.next_continuation_token, None);
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[0].key, "p/a/b.txt"); // %2F decoded
        assert_eq!(page.entries[0].size, 42);
        assert_eq!(page.entries[0].last_modified, "2015-08-30T12:36:00.000Z");
        assert_eq!(page.entries[1].key, "p/with space");
        assert_eq!(page.entries[1].size, 7);
    }

    #[test]
    fn truncated_with_token() {
        let xml = r"<ListBucketResult>
  <IsTruncated>true</IsTruncated>
  <NextContinuationToken>TOKEN==</NextContinuationToken>
  <Contents><Key>k</Key><Size>1</Size><LastModified>2020-01-01T00:00:00.000Z</LastModified></Contents>
</ListBucketResult>";
        let page = parse_list_objects_v2(xml.as_bytes()).unwrap();
        assert!(page.is_truncated);
        assert_eq!(page.next_continuation_token.as_deref(), Some("TOKEN=="));
        assert_eq!(page.entries.len(), 1);
    }

    #[test]
    fn empty_list() {
        let xml = r"<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>";
        let page = parse_list_objects_v2(xml.as_bytes()).unwrap();
        assert!(page.entries.is_empty());
        assert!(!page.is_truncated);
    }

    #[test]
    fn namespaced_elements_ok() {
        let xml = r#"<s3:ListBucketResult xmlns:s3="x">
  <s3:IsTruncated>false</s3:IsTruncated>
  <s3:Contents><s3:Key>k</s3:Key><s3:Size>3</s3:Size><s3:LastModified>2020-01-01T00:00:00Z</s3:LastModified></s3:Contents>
</s3:ListBucketResult>"#;
        let page = parse_list_objects_v2(xml.as_bytes()).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].key, "k");
    }

    #[test]
    fn missing_size_is_error() {
        let xml = r"<ListBucketResult><Contents><Key>k</Key><LastModified>2020-01-01T00:00:00Z</LastModified></Contents></ListBucketResult>";
        assert!(parse_list_objects_v2(xml.as_bytes()).is_err());
    }

    #[test]
    fn invalid_size_is_error() {
        let xml = r"<ListBucketResult><Contents><Key>k</Key><Size>NaN</Size><LastModified>2020-01-01T00:00:00Z</LastModified></Contents></ListBucketResult>";
        assert!(parse_list_objects_v2(xml.as_bytes()).is_err());
    }

    #[test]
    fn missing_key_is_error() {
        let xml = r"<ListBucketResult><Contents><Size>3</Size><LastModified>2020-01-01T00:00:00Z</LastModified></Contents></ListBucketResult>";
        assert!(parse_list_objects_v2(xml.as_bytes()).is_err());
    }

    #[test]
    fn complete_multipart_success() {
        let xml = r#"<CompleteMultipartUploadResult><Location>x</Location><ETag>"e"</ETag></CompleteMultipartUploadResult>"#;
        assert!(check_complete_multipart_response(xml.as_bytes()).is_ok());
    }

    #[test]
    fn complete_multipart_in_body_error() {
        let xml = r"<Error><Code>InternalError</Code><Message>boom</Message></Error>";
        let e = check_complete_multipart_response(xml.as_bytes()).unwrap_err();
        assert!(format!("{e}").contains("InternalError"));
    }

    #[test]
    fn parse_upload_id_works() {
        let xml = r"<InitiateMultipartUploadResult><Bucket>b</Bucket><Key>k</Key><UploadId>UP123</UploadId></InitiateMultipartUploadResult>";
        assert_eq!(parse_upload_id(xml.as_bytes()).unwrap(), "UP123");
    }

    #[test]
    fn build_complete_body_orders_parts() {
        let body = build_complete_multipart_body(&[(1, "\"e1\"".into()), (2, "\"e2\"".into())]);
        assert!(body.contains("<PartNumber>1</PartNumber>"));
        assert!(body.contains("<ETag>&quot;e1&quot;</ETag>"));
        assert!(body.find("<PartNumber>1").unwrap() < body.find("<PartNumber>2").unwrap());
    }
}
