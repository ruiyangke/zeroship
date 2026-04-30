//! Per-app routing — produces a single [`Outcome`] describing what the
//! gateway should do with the request. Pure: no IO, no async, no state.
//! The outer router executes the outcome.
//!
//! Dispatch itself lives in [`crate::compiled::CompiledManifest`]; this
//! module owns the [`Outcome`] / [`StaticHit`] surface that the router
//! and the compiled dispatcher share.

use std::collections::HashMap;

use zeroship_core::types::{AssetVariant, CacheCtl, WorkerMode};

#[cfg(test)]
use zeroship_core::types::Manifest;

#[cfg(test)]
use crate::compiled::CompiledManifest;

/// What the gateway should do with this request after walking the
/// rules.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Serve a static asset from the object store.
    Static(StaticHit),
    /// Forward to the worker.
    Worker {
        mode: WorkerMode,
        cache: Option<CacheCtl>,
        /// Per-rule rate limit declared in the manifest. Enforced on top
        /// of the gateway's global per-app limit.
        rate_limit: Option<zeroship_core::types::RateLimit>,
        /// Index of the rule that matched, in declaration order. Used
        /// by the gateway's per-rule rate limiter so two worker rules
        /// with the same `rate_limit` config get independent buckets,
        /// and the bucket survives manifest re-orderings only as far
        /// as the rule's position is stable.
        rule_idx: u32,
    },
    /// Redirect (HTTP 30x).
    Redirect { to: String, status: u16 },
    /// No rule matched.
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
    /// Cache directives, resolved from (asset entry override) ∪ (rule
    /// cache config) ∪ (gateway defaults).
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

/// Test-only entry: compile + dispatch a [`Manifest`] in one shot.
/// Production code goes through `RouteCache` so manifests are compiled
/// once per route update; this helper keeps the existing test surface.
/// Drops the per-rule `Cors` returned alongside the outcome — tests in
/// this module don't exercise the CORS plumbing (the gateway router
/// tests do).
#[cfg(test)]
pub fn dispatch_manifest(method: &str, path: &str, m: &Manifest) -> Outcome {
    CompiledManifest::compile(m).dispatch(method, path).0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zeroship_core::types::{
        Action, AssetEntry, HttpMethod, Manifest, Match, Rule, WorkerMode,
    };

    fn asset(hash: &str, ct: &str) -> AssetEntry {
        AssetEntry {
            hash: hash.into(),
            content_type: ct.into(),
            size: 0,
            cache: None,
            updated_at: 0,
            variants: HashMap::new(),
        }
    }

    fn manifest_full() -> Manifest {
        Manifest {
            rules: vec![
                Rule {
                    r#match: Match::Prefix {
                        method: Some(HttpMethod::Post),
                        path: "/_rpc/".into(),
                    },
                    action: Action::Worker {
                        mode: WorkerMode::Rpc,
                        cache: None,
                        rate_limit: None,
                    },
                    cors: None,
                },
                Rule {
                    r#match: Match::Glob {
                        method: None,
                        path: "/blog/[slug]".into(),
                    },
                    action: Action::Worker {
                        mode: WorkerMode::Ssr,
                        cache: None,
                        rate_limit: None,
                    },
                    cors: None,
                },
                Rule {
                    r#match: Match::Any,
                    action: Action::Static {
                        r#try: vec!["$path".into(), "/index.html".into()],
                        cache: None,
                        status: None,
                    },
                    cors: None,
                },
            ],
            assets: HashMap::from([
                ("/index.html".into(), asset("h1", "text/html")),
                ("/_assets/main.js".into(), asset("h2", "application/javascript")),
            ]),
            ..Manifest::default()
        }
    }

    #[test]
    fn rpc_dispatches_to_worker_rpc() {
        let m = manifest_full();
        match dispatch_manifest("POST", "/_rpc/listTodos", &m) {
            Outcome::Worker { mode, .. } => assert_eq!(mode, WorkerMode::Rpc),
            other => panic!("expected worker rpc, got {other:?}"),
        }
    }

    #[test]
    fn ssr_dispatches_to_worker_ssr() {
        let m = manifest_full();
        match dispatch_manifest("GET", "/blog/hello", &m) {
            Outcome::Worker { mode, .. } => assert_eq!(mode, WorkerMode::Ssr),
            other => panic!("expected worker ssr, got {other:?}"),
        }
    }

    #[test]
    fn static_hit_serves_build_asset() {
        let m = manifest_full();
        match dispatch_manifest("GET", "/index.html", &m) {
            Outcome::Static(hit) => {
                assert_eq!(hit.hash, "h1");
                assert!(!hit.mutable);
            }
            other => panic!("expected static, got {other:?}"),
        }
    }

    #[test]
    fn static_falls_back_to_index_html_for_unknown() {
        let m = manifest_full();
        match dispatch_manifest("GET", "/some-spa-route", &m) {
            Outcome::Static(hit) => assert_eq!(hit.hash, "h1"),
            other => panic!("expected static spa fallback, got {other:?}"),
        }
    }

    #[test]
    fn hashed_asset_gets_immutable_cache() {
        let m = manifest_full();
        match dispatch_manifest("GET", "/_assets/main.js", &m) {
            Outcome::Static(hit) => {
                assert!(hit.cache.immutable);
                assert!(hit.cache.max_age > 1_000_000);
            }
            other => panic!("expected static, got {other:?}"),
        }
    }

    #[test]
    fn passthrough_rpc_to_worker_rpc() {
        // The synthesized passthrough manifest dispatches POST /_rpc/* to
        // WorkerMode::Rpc and everything else to WorkerMode::Ssr.
        let m = Manifest::passthrough();
        match dispatch_manifest("POST", "/_rpc/listTodos", &m) {
            Outcome::Worker { mode, .. } => assert_eq!(mode, WorkerMode::Rpc),
            other => panic!("expected Worker(Rpc), got {other:?}"),
        }
        match dispatch_manifest("GET", "/anything", &m) {
            Outcome::Worker { mode, .. } => assert_eq!(mode, WorkerMode::Ssr),
            other => panic!("expected Worker(Ssr), got {other:?}"),
        }
    }

    // -- Tier 1 bug regressions ------------------------------------------------

    #[test]
    fn rewrite_chain_resolves_to_terminal_outcome() {
        // /old rewrites to /new; /new serves /new.html.
        let m = Manifest {
            rules: vec![
                Rule {
                    r#match: Match::Exact { method: None, path: "/old".into() },
                    action: Action::Rewrite { to: "/new".into() },
                    cors: None,
                },
                Rule {
                    r#match: Match::Exact { method: None, path: "/new".into() },
                    action: Action::Static {
                        r#try: vec!["/new.html".into()],
                        cache: None,
                        status: None,
                    },
                    cors: None,
                },
            ],
            assets: HashMap::from([("/new.html".into(), asset("h-new", "text/html"))]),
            ..Manifest::default()
        };
        match dispatch_manifest("GET", "/old", &m) {
            Outcome::Static(hit) => assert_eq!(hit.hash, "h-new"),
            other => panic!("expected Static after rewrite, got {other:?}"),
        }
    }

    #[test]
    fn rewrite_circular_returns_not_found_after_hop_limit() {
        let m = Manifest {
            rules: vec![
                Rule {
                    r#match: Match::Exact { method: None, path: "/a".into() },
                    action: Action::Rewrite { to: "/b".into() },
                    cors: None,
                },
                Rule {
                    r#match: Match::Exact { method: None, path: "/b".into() },
                    action: Action::Rewrite { to: "/a".into() },
                    cors: None,
                },
            ],
            ..Manifest::default()
        };
        match dispatch_manifest("GET", "/a", &m) {
            Outcome::NotFound => {}
            other => panic!("expected NotFound after circular rewrite, got {other:?}"),
        }
    }

    #[test]
    fn static_action_skipped_for_non_get_head() {
        // manifest_full()'s last rule is `Any → Static{try:["$path","/index.html"]}`.
        // A POST to an unknown path must NOT serve the SPA shell.
        let m = manifest_full();
        match dispatch_manifest("POST", "/some-spa-route", &m) {
            Outcome::Static(hit) => panic!("POST must not match static SPA fallback (got hash={})", hit.hash),
            _ => {}
        }
    }

    #[test]
    fn worker_rule_propagates_rate_limit() {
        use zeroship_core::types::{RateLimit, RateLimitPer};
        let m = Manifest {
            rules: vec![Rule {
                r#match: Match::Exact { method: None, path: "/api".into() },
                action: Action::Worker {
                    mode: WorkerMode::Ssr,
                    cache: None,
                    rate_limit: Some(RateLimit {
                        rpm: Some(60),
                        rps: None,
                        per: RateLimitPer::Ip,
                    }),
                },
                cors: None,
            }],
            ..Manifest::default()
        };
        match dispatch_manifest("GET", "/api", &m) {
            Outcome::Worker { rate_limit: Some(rl), .. } => {
                assert_eq!(rl.rpm, Some(60));
            }
            other => panic!("expected Worker with rate_limit, got {other:?}"),
        }
    }
}
