//! Per-app routing — walks the manifest's rules in order and produces
//! a single [`Outcome`] describing what the gateway should do with the
//! request. Pure: no IO, no async, no state. The outer router executes
//! the outcome.
//!
//! Every app has a manifest (apps that haven't shipped their own get
//! [`Manifest::passthrough`] synthesized at load time), so dispatch is
//! always defined — there is no legacy fallback path.

use std::collections::HashMap;

use zeroship_core::types::{
    Action, AssetEntry, CacheCtl, Manifest, RouteEntry, Rule, WorkerMode,
};

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
    pub hash: String,
    pub content_type: String,
    pub size: u64,
    /// Cache directives, resolved from (asset entry override) ∪ (rule
    /// cache config) ∪ (gateway defaults).
    pub cache: CacheCtl,
    /// Override response status (e.g. 404 for a /404.html catch-all).
    pub status: Option<u16>,
    /// Whether this asset came from `runtime_assets` — the gateway
    /// honors stale-while-revalidate semantics for those.
    pub mutable: bool,
}

/// Top-level entry. Walks the manifest's rules first-match-wins.
pub fn dispatch(method: &str, path: &str, route: &RouteEntry) -> Outcome {
    walk_rules(method, path, &route.manifest)
}

/// Public for tests — same as `dispatch` but takes a Manifest directly.
pub fn dispatch_manifest(method: &str, path: &str, m: &Manifest) -> Outcome {
    walk_rules(method, path, m)
}

fn walk_rules(method: &str, path: &str, m: &Manifest) -> Outcome {
    // Re-walk on rewrite. Bounded to prevent infinite loops; matches
    // nginx's default rewrite_log limit semantics.
    const MAX_HOPS: u32 = 8;

    let mut current_path = path.to_string();
    for _hop in 0..MAX_HOPS {
        let mut rewrote = false;
        for rule in &m.rules {
            // Static actions only fire for GET/HEAD. POST /foo against a
            // catch-all `Any → Static` must not return the SPA shell — fall
            // through to the next rule (typically a Worker, else NotFound).
            if matches!(rule.action, Action::Static { .. })
                && !matches!(method, "GET" | "HEAD")
            {
                continue;
            }
            let captures = match rule.r#match.test(method, &current_path) {
                Some(c) => c,
                None => continue,
            };
            match resolve_action(method, &current_path, &captures, rule, m) {
                Ok(outcome) => return outcome,
                Err(new_path) => {
                    // Rewrite — restart from the top with the new path.
                    current_path = new_path;
                    rewrote = true;
                    break;
                }
            }
        }
        if !rewrote {
            return Outcome::NotFound;
        }
    }
    Outcome::NotFound // rewrite hop limit exhausted
}

/// Resolve a matched rule against the request. Returns Ok(Outcome) for
/// a terminal action, Err(new_path) when the action was a rewrite (so
/// the caller restarts the walk).
fn resolve_action(
    _method: &str,
    path: &str,
    captures: &HashMap<String, String>,
    rule: &Rule,
    m: &Manifest,
) -> Result<Outcome, String> {
    match &rule.action {
        Action::Static { r#try, cache, status } => {
            for candidate in r#try {
                let resolved = substitute(candidate, path, captures);
                if let Some((entry, mutable)) = lookup_asset(&resolved, m) {
                    return Ok(Outcome::Static(StaticHit {
                        path: resolved.clone(),
                        hash: entry.hash.clone(),
                        content_type: entry.content_type.clone(),
                        size: entry.size,
                        cache: pick_cache(entry, cache.as_ref(), &resolved, mutable),
                        status: *status,
                        mutable,
                    }));
                }
            }
            Ok(Outcome::NotFound)
        }
        Action::Worker { mode, cache, rate_limit } => Ok(Outcome::Worker {
            mode: *mode,
            cache: cache.clone(),
            rate_limit: rate_limit.clone(),
        }),
        Action::Redirect { to, status } => Ok(Outcome::Redirect {
            to: substitute(to, path, captures),
            status: *status,
        }),
        Action::Rewrite { to } => {
            let new_path = substitute(to, path, captures);
            Err(new_path)
        }
    }
}

/// Resolve an asset path against runtime first, build second.
/// Returns (entry, is_mutable).
fn lookup_asset<'a>(path: &str, m: &'a Manifest) -> Option<(&'a AssetEntry, bool)> {
    if let Some(e) = m.runtime_assets.get(path) {
        return Some((e, true));
    }
    if let Some(e) = m.build_assets.get(path) {
        return Some((e, false));
    }
    None
}

/// Substitute $path and [name] captures in a template string.
/// $path → the request path before any rewrite (or the current path)
/// [name] → the captured glob value
fn substitute(template: &str, path: &str, captures: &HashMap<String, String>) -> String {
    let mut out = template.replace("$path", path);
    for (k, v) in captures {
        out = out.replace(&format!("[{k}]"), v);
        out = out.replace(&format!("[...{k}]"), v);
    }
    out
}

/// Pick the effective cache config for a static hit.
///
/// Precedence:
///   1. explicit `entry.cache` (per-asset override)
///   2. explicit `rule.cache` (rule-level)
///   3. gateway default — long immutable for `/_assets/<hash>` URLs,
///      short revalidating for everything else
fn pick_cache(
    entry: &AssetEntry,
    rule_cache: Option<&CacheCtl>,
    resolved_path: &str,
    mutable: bool,
) -> CacheCtl {
    if let Some(c) = &entry.cache {
        return c.clone();
    }
    if let Some(c) = rule_cache {
        return c.clone();
    }
    if !mutable && resolved_path.starts_with("/_assets/") {
        // Hashed asset URLs — safe to cache forever.
        return CacheCtl {
            max_age: 31_536_000,
            swr_window: None,
            immutable: true,
            background_refresh: false,
            stale_on_error: false,
        };
    }
    CacheCtl {
        max_age: 60,
        swr_window: None,
        immutable: false,
        background_refresh: false,
        stale_on_error: false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zeroship_core::types::{AssetEntry, HttpMethod, Manifest, Match, Rule, WorkerMode};

    fn asset(hash: &str, ct: &str) -> AssetEntry {
        AssetEntry {
            hash: hash.into(),
            content_type: ct.into(),
            size: 0,
            cache: None,
            updated_at: 0,
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
                },
                Rule {
                    r#match: Match::Any,
                    action: Action::Static {
                        r#try: vec!["$path".into(), "/index.html".into()],
                        cache: None,
                        status: None,
                    },
                },
            ],
            build_assets: HashMap::from([
                ("/index.html".into(), asset("h1", "text/html")),
                ("/_assets/main.js".into(), asset("h2", "application/javascript")),
            ]),
            runtime_assets: HashMap::new(),
            server_bundle_hash: None,
            asset_version: 0,
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
                },
                Rule {
                    r#match: Match::Exact { method: None, path: "/new".into() },
                    action: Action::Static {
                        r#try: vec!["/new.html".into()],
                        cache: None,
                        status: None,
                    },
                },
            ],
            build_assets: HashMap::from([("/new.html".into(), asset("h-new", "text/html"))]),
            runtime_assets: HashMap::new(),
            server_bundle_hash: None,
            asset_version: 0,
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
                },
                Rule {
                    r#match: Match::Exact { method: None, path: "/b".into() },
                    action: Action::Rewrite { to: "/a".into() },
                },
            ],
            build_assets: HashMap::new(),
            runtime_assets: HashMap::new(),
            server_bundle_hash: None,
            asset_version: 0,
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
            }],
            build_assets: HashMap::new(),
            runtime_assets: HashMap::new(),
            server_bundle_hash: None,
            asset_version: 0,
        };
        match dispatch_manifest("GET", "/api", &m) {
            Outcome::Worker { rate_limit: Some(rl), .. } => {
                assert_eq!(rl.rpm, Some(60));
            }
            other => panic!("expected Worker with rate_limit, got {other:?}"),
        }
    }
}
