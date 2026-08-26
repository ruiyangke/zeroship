//! Dispatch outcomes — shared types describing what the gateway should
//! do with a request after resource-tree resolution.
//!
//! Pure: no IO, no async, no state. The outer router (`router.rs`)
//! constructs `Outcome` / `StaticHit` instances from the matched
//! `EffectivePolicy` and executes them.

use std::collections::HashMap;

use zeroship_bundle::{AssetVariant, CacheCtl, WorkerMode};

/// What the gateway should do with this request after resolving the
/// matched resource. Constructed by the resource-tree path in
/// `router.rs::execute_resource_tree`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum Outcome {
    /// Serve a static asset from the object store.
    Static(StaticHit),
    /// Forward to the worker.
    Worker {
        mode: WorkerMode,
        cache: Option<CacheCtl>,
        /// Per-resource rate limit declared in the manifest. Enforced on
        /// top of the gateway's global per-app limit.
        rate_limit: Option<zeroship_bundle::RateLimit>,
        /// Stable hash of the matched resource key. Used by the gateway's
        /// per-resource rate limiter so two resources with the same
        /// `rate_limit` config get independent buckets.
        rule_idx: u32,
    },
    /// Redirect (HTTP 30x).
    Redirect { to: String, status: u16 },
    /// No resource matched.
    NotFound,
}

/// Concrete information needed to serve a static asset.
#[derive(Debug, Clone)]
pub struct StaticHit {
    /// The asset path that resolved (e.g. `/index.html`). Gateway
    /// uses this to fetch the bytes from the BundleStore.
    pub path: String,
    /// Identity (uncompressed) hash. Gateway falls back to this when
    /// no `Accept-Encoding` variant matches.
    pub hash: String,
    pub content_type: String,
    /// Identity byte count. Compressed variants in `variants` carry
    /// their own size.
    pub size: u64,
    /// Cache directives, resolved from (asset entry override) ∪
    /// (resource cache config) ∪ (gateway defaults).
    pub cache: CacheCtl,
    /// Override response status (e.g. 404 for a /404.html catch-all).
    pub status: Option<u16>,
    /// Whether this asset came from `runtime_assets` — the gateway
    /// honors stale-while-revalidate semantics for those.
    pub mutable: bool,
    /// Pre-compressed encoding variants. Keys are HTTP
    /// `Content-Encoding` tokens (`"br"`, `"gzip"`); values point at
    /// the compressed blob and carry its compressed size. Empty when
    /// the build pipeline didn't emit any variants.
    pub variants: HashMap<String, AssetVariant>,
}
