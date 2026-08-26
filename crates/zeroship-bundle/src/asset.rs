use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::rule::CacheCtl;

/// One asset blob, content-addressed. Stored in the object store
/// keyed by `hash`; surfaced to the gateway via the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetEntry {
    /// SHA-256 (hex) of the raw (identity) bytes. Used as the
    /// object-store key, the canonical ETag, and the default body when
    /// no `Accept-Encoding` variant is selected.
    pub hash: String,
    pub content_type: String,
    /// Identity (uncompressed) byte count. Used for `Content-Length`
    /// and quotas. Compressed variants have their own `size` in
    /// `variants[<encoding>].size`.
    pub size: u64,
    /// Per-asset cache override. If absent, the rule's `cache` field
    /// applies; if that's also absent, gateway picks a sensible default
    /// (1y immutable for `/_assets/<hash>` paths, short for everything
    /// else).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheCtl>,
    /// Unix-seconds. Set on `assets.put`. The gateway computes
    /// staleness from `now - updated_at` against the `cache` window.
    #[serde(default)]
    pub updated_at: i64,
    /// Pre-compressed encoding variants. Keys are HTTP
    /// `Content-Encoding` token names (`"br"`, `"gzip"`); values are
    /// the COMPRESSED hash + compressed size. Identity is always
    /// available via the parent `hash`/`size`; entries here are the
    /// alternative encodings the build pipeline emitted.
    ///
    /// The gateway negotiates via `Accept-Encoding` and serves the
    /// chosen variant's blob (with `Content-Encoding: <key>` and
    /// `Vary: Accept-Encoding` set on the response). The ETag and
    /// Content-Length reflect the variant, not the identity.
    ///
    /// Validation: variant keys MUST be one of the allowed encodings
    /// (currently `"br"` and `"gzip"`); variant hashes MUST be
    /// 64-char lowercase hex.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub variants: HashMap<String, AssetVariant>,
}

/// A pre-compressed encoding variant of an [`AssetEntry`].
///
/// Stored in `AssetEntry::variants` keyed by `Content-Encoding` token.
/// Each variant is its own content-addressed blob — the build
/// pipeline compresses the asset, hashes the compressed bytes, and
/// uploads the variant blob alongside the identity blob.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetVariant {
    /// SHA-256 (hex) of the compressed bytes.
    pub hash: String,
    /// Size of the compressed bytes (NOT the original).
    pub size: u64,
}
