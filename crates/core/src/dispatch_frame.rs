//! Gateway -> worker HTTP dispatch frame.
//!
//! Wire format:
//! `[u32 little-endian metadata_len][metadata JSON bytes][raw request body bytes]`.
//! Metadata carries method, URL, and headers only; the creator request body is
//! appended byte-for-byte after the metadata.

/// Fixed prefix containing the little-endian metadata length.
pub const DISPATCH_FRAME_PREFIX_BYTES: usize = 4;

/// Conservative guard for the JSON metadata prefix. The request body has its
/// own worker-side cap; this bound prevents pathological header blocks from
/// driving an unbounded metadata parse.
pub const MAX_DISPATCH_META_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DispatchMetadata {
    pub method: String,
    pub url: String,
    /// Headers as [[key, value], ...] array.
    pub headers: Vec<(String, String)>,
}

impl DispatchMetadata {
    pub fn new(method: &str, url: &str, headers: &[(String, String)]) -> Self {
        Self {
            method: method.to_string(),
            url: url.to_string(),
            headers: headers.to_vec(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchFrameError {
    #[error("dispatch frame too short")]
    TooShort,
    #[error("dispatch metadata length exceeds frame length")]
    TruncatedMetadata,
    #[error("dispatch metadata too large")]
    MetadataTooLarge,
    #[error("dispatch metadata length overflows u32")]
    MetadataLengthOverflow,
    #[error("invalid dispatch metadata: {0}")]
    InvalidMetadata(#[from] serde_json::Error),
}

#[derive(Debug)]
pub struct DispatchFrameParts<'a> {
    pub metadata: DispatchMetadata,
    pub body: &'a [u8],
    pub body_offset: usize,
}

pub fn encode_dispatch_frame(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<Vec<u8>, DispatchFrameError> {
    let metadata = DispatchMetadata::new(method, url, headers);
    let metadata_bytes = serde_json::to_vec(&metadata)?;
    if metadata_bytes.len() > MAX_DISPATCH_META_BYTES {
        return Err(DispatchFrameError::MetadataTooLarge);
    }
    let metadata_len: u32 = metadata_bytes
        .len()
        .try_into()
        .map_err(|_| DispatchFrameError::MetadataLengthOverflow)?;

    let mut frame =
        Vec::with_capacity(DISPATCH_FRAME_PREFIX_BYTES + metadata_bytes.len() + body.len());
    frame.extend_from_slice(&metadata_len.to_le_bytes());
    frame.extend_from_slice(&metadata_bytes);
    frame.extend_from_slice(body);
    Ok(frame)
}

pub fn decode_dispatch_frame(frame: &[u8]) -> Result<DispatchFrameParts<'_>, DispatchFrameError> {
    if frame.len() < DISPATCH_FRAME_PREFIX_BYTES {
        return Err(DispatchFrameError::TooShort);
    }

    let metadata_len = u32::from_le_bytes([
        frame[0], frame[1], frame[2], frame[3],
    ]) as usize;
    if metadata_len > MAX_DISPATCH_META_BYTES {
        return Err(DispatchFrameError::MetadataTooLarge);
    }

    let body_offset = DISPATCH_FRAME_PREFIX_BYTES
        .checked_add(metadata_len)
        .ok_or(DispatchFrameError::TruncatedMetadata)?;
    if body_offset > frame.len() {
        return Err(DispatchFrameError::TruncatedMetadata);
    }

    let metadata =
        serde_json::from_slice(&frame[DISPATCH_FRAME_PREFIX_BYTES..body_offset])?;
    Ok(DispatchFrameParts {
        metadata,
        body: &frame[body_offset..],
        body_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_frame_preserves_raw_body_bytes() {
        let body = [0xff, 0x00, 0xfe, 0x80];
        let headers = vec![("content-type".to_string(), "application/octet-stream".to_string())];
        let frame = encode_dispatch_frame("POST", "https://example.test/upload", &headers, &body)
            .expect("encode frame");
        let decoded = decode_dispatch_frame(&frame).expect("decode frame");

        assert_eq!(decoded.metadata.method, "POST");
        assert_eq!(decoded.metadata.url, "https://example.test/upload");
        assert_eq!(decoded.metadata.headers, headers);
        assert_eq!(decoded.body, body);
    }

    #[test]
    fn dispatch_frame_rejects_truncated_metadata() {
        let mut frame = 20u32.to_le_bytes().to_vec();
        frame.extend_from_slice(br#"{"method":"GET"}"#);

        let err = decode_dispatch_frame(&frame).expect_err("truncated metadata rejected");
        assert!(matches!(err, DispatchFrameError::TruncatedMetadata));
    }
}
