//! Pre-compiled manifest dispatch — Tier 2 items 7 + 10.
//!
//! Source `Manifest` is unchanged on the wire. At route-update time the
//! gateway compiles each app's manifest into a hot-path-friendly form:
//! method bitsets, pre-segmented globs, pre-parsed templates. Per-request
//! work drops to bit-AND for method gating, segment compares for path
//! matching, and a chunk-walk for template render — no per-request
//! HashMap, no per-call `String::replace`.

use std::collections::HashMap;

use zeroship_core::types::{
    Action, AssetEntry, CacheCtl, HttpMethod, Manifest, Match, RateLimit, Rule, WorkerMode,
};

use crate::dispatch::{Outcome, StaticHit};

// ---------------------------------------------------------------------------
// Method bitset constants — local copy of core's helpers so we don't
// depend on `pub`-ifying them. Must stay in sync with crates/core/src/types.rs.
// ---------------------------------------------------------------------------

const M_GET: u8 = 1 << 0;
const M_POST: u8 = 1 << 1;
const M_PUT: u8 = 1 << 2;
const M_PATCH: u8 = 1 << 3;
const M_DELETE: u8 = 1 << 4;
const M_HEAD: u8 = 1 << 5;
const M_OPTIONS: u8 = 1 << 6;
const M_ALL: u8 = M_GET | M_POST | M_PUT | M_PATCH | M_DELETE | M_HEAD | M_OPTIONS;
const M_GET_HEAD: u8 = M_GET | M_HEAD;

fn method_bit(m: HttpMethod) -> u8 {
    match m {
        HttpMethod::Get => M_GET,
        HttpMethod::Post => M_POST,
        HttpMethod::Put => M_PUT,
        HttpMethod::Patch => M_PATCH,
        HttpMethod::Delete => M_DELETE,
        HttpMethod::Head => M_HEAD,
        HttpMethod::Options => M_OPTIONS,
        HttpMethod::Any => M_ALL,
    }
}

/// Map a request's method string to a single-bit mask. Returns 0 for
/// unrecognized methods so they short-circuit to "no match".
fn request_method_bit(method: &str) -> u8 {
    match method {
        "GET" => M_GET,
        "POST" => M_POST,
        "PUT" => M_PUT,
        "PATCH" => M_PATCH,
        "DELETE" => M_DELETE,
        "HEAD" => M_HEAD,
        "OPTIONS" => M_OPTIONS,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Compiled manifest types
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
pub struct CompiledManifest {
    rules: Vec<CompiledRule>,
    assets: HashMap<String, AssetEntry>,
    runtime_assets: HashMap<String, AssetEntry>,
    #[allow(dead_code)]
    server_bundle: Option<String>,
    #[allow(dead_code)]
    asset_version: i64,
}

struct CompiledRule {
    methods: u8,
    matcher: CompiledMatch,
    action: CompiledAction,
}

enum CompiledMatch {
    Exact(String),
    Prefix(String),
    Glob(CompiledGlob),
    Any,
}

struct CompiledGlob {
    segments: Vec<GlobSegment>,
}

enum GlobSegment {
    Literal(String),
    SingleCapture(String),
    CatchAll(String),
    Star,
}

enum CompiledAction {
    Static {
        try_chain: Vec<CompiledTemplate>,
        cache: Option<CacheCtl>,
        status: Option<u16>,
    },
    Worker {
        mode: WorkerMode,
        cache: Option<CacheCtl>,
        rate_limit: Option<RateLimit>,
    },
    Redirect {
        to: CompiledTemplate,
        status: u16,
    },
    Rewrite {
        to: CompiledTemplate,
    },
}

#[allow(missing_debug_implementations)]
pub struct CompiledTemplate {
    chunks: Vec<TemplateChunk>,
}

enum TemplateChunk {
    Literal(String),
    Path,
    Capture(String),
}

// ---------------------------------------------------------------------------
// Captures — keep allocation off the non-glob hot path.
// ---------------------------------------------------------------------------

enum Captures<'a> {
    Empty,
    Glob(Vec<(&'a str, String)>),
}

impl<'a> Captures<'a> {
    fn lookup(&self, name: &str) -> Option<&str> {
        match self {
            Self::Empty => None,
            Self::Glob(v) => v.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str()),
        }
    }
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

impl CompiledManifest {
    pub fn compile(m: &Manifest) -> Self {
        let rules = m.rules.iter().map(compile_rule).collect();
        Self {
            rules,
            assets: m.assets.clone(),
            runtime_assets: m.runtime_assets.clone(),
            server_bundle: m.server_bundle.clone(),
            asset_version: m.asset_version,
        }
    }
}

fn compile_rule(rule: &Rule) -> CompiledRule {
    let methods = effective_methods(rule);
    let matcher = compile_match(&rule.r#match);
    let action = compile_action(&rule.action);
    CompiledRule { methods, matcher, action }
}

fn effective_methods(rule: &Rule) -> u8 {
    let base = match rule.r#match {
        Match::Exact { method, .. }
        | Match::Prefix { method, .. }
        | Match::Glob { method, .. } => match method {
            None | Some(HttpMethod::Any) => M_ALL,
            Some(m) => method_bit(m),
        },
        Match::Any => M_ALL,
    };
    if matches!(rule.action, Action::Static { .. }) {
        base & M_GET_HEAD
    } else {
        base
    }
}

fn compile_match(m: &Match) -> CompiledMatch {
    match m {
        Match::Exact { path, .. } => CompiledMatch::Exact(path.clone()),
        Match::Prefix { path, .. } => {
            // Normalize: store without trailing slash so the boundary check
            // is uniform — `path == bare || path.starts_with("{bare}/")`.
            let bare = path.strip_suffix('/').unwrap_or(path).to_string();
            CompiledMatch::Prefix(bare)
        }
        Match::Glob { path, .. } => CompiledMatch::Glob(compile_glob(path)),
        Match::Any => CompiledMatch::Any,
    }
}

fn compile_glob(pattern: &str) -> CompiledGlob {
    let raw_segs: Vec<&str> = pattern.trim_start_matches('/').split('/').collect();
    let total = raw_segs.len();
    let mut segments = Vec::with_capacity(total);
    for (i, seg) in raw_segs.iter().enumerate() {
        if seg.starts_with("[...") && seg.ends_with(']') {
            assert!(
                i + 1 == total,
                "glob catch-all [...name] must be the last segment in `{pattern}`",
            );
            let name = &seg[4..seg.len() - 1];
            segments.push(GlobSegment::CatchAll(name.to_string()));
        } else if seg.starts_with('[') && seg.ends_with(']') {
            let name = &seg[1..seg.len() - 1];
            segments.push(GlobSegment::SingleCapture(name.to_string()));
        } else if *seg == "*" {
            segments.push(GlobSegment::Star);
        } else {
            segments.push(GlobSegment::Literal((*seg).to_string()));
        }
    }
    CompiledGlob { segments }
}

fn compile_action(a: &Action) -> CompiledAction {
    match a {
        Action::Static { r#try, cache, status } => CompiledAction::Static {
            try_chain: r#try.iter().map(|t| CompiledTemplate::parse(t)).collect(),
            cache: cache.clone(),
            status: *status,
        },
        Action::Worker { mode, cache, rate_limit } => CompiledAction::Worker {
            mode: *mode,
            cache: cache.clone(),
            rate_limit: rate_limit.clone(),
        },
        Action::Redirect { to, status } => CompiledAction::Redirect {
            to: CompiledTemplate::parse(to),
            status: *status,
        },
        Action::Rewrite { to } => CompiledAction::Rewrite {
            to: CompiledTemplate::parse(to),
        },
    }
}

// ---------------------------------------------------------------------------
// Template parsing + rendering
// ---------------------------------------------------------------------------

impl CompiledTemplate {
    pub fn parse(s: &str) -> Self {
        let bytes = s.as_bytes();
        let mut chunks: Vec<TemplateChunk> = Vec::new();
        let mut lit = String::new();
        let mut i = 0;
        while i < bytes.len() {
            // $path token
            if bytes[i] == b'$' && bytes[i..].starts_with(b"$path") {
                if !lit.is_empty() {
                    chunks.push(TemplateChunk::Literal(std::mem::take(&mut lit)));
                }
                chunks.push(TemplateChunk::Path);
                i += 5;
                continue;
            }
            // [name] or [...name] capture token
            if bytes[i] == b'[' {
                if let Some(end_rel) = bytes[i..].iter().position(|&b| b == b']') {
                    let end = i + end_rel;
                    let inner = &s[i + 1..end];
                    let name = inner.strip_prefix("...").unwrap_or(inner);
                    if !lit.is_empty() {
                        chunks.push(TemplateChunk::Literal(std::mem::take(&mut lit)));
                    }
                    chunks.push(TemplateChunk::Capture(name.to_string()));
                    i = end + 1;
                    continue;
                }
            }
            lit.push(bytes[i] as char);
            i += 1;
        }
        if !lit.is_empty() {
            chunks.push(TemplateChunk::Literal(lit));
        }
        if chunks.is_empty() {
            chunks.push(TemplateChunk::Literal(String::new()));
        }
        Self { chunks }
    }

    fn render(&self, path: &str, captures: &Captures<'_>) -> String {
        // Pre-size the buffer for the common case (literal-only template).
        let mut hint = 0;
        for c in &self.chunks {
            match c {
                TemplateChunk::Literal(s) => hint += s.len(),
                TemplateChunk::Path => hint += path.len(),
                TemplateChunk::Capture(_) => hint += 8,
            }
        }
        let mut out = String::with_capacity(hint);
        for c in &self.chunks {
            match c {
                TemplateChunk::Literal(s) => out.push_str(s),
                TemplateChunk::Path => out.push_str(path),
                TemplateChunk::Capture(name) => {
                    if let Some(v) = captures.lookup(name) {
                        out.push_str(v);
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Path matching
// ---------------------------------------------------------------------------

fn match_prefix_bare(bare: &str, path: &str) -> bool {
    // `bare` has no trailing slash. Match the whole path or require a `/`
    // boundary so `/admin` doesn't match `/administrator`.
    if path == bare {
        return true;
    }
    if let Some(rest) = path.strip_prefix(bare) {
        return rest.starts_with('/');
    }
    false
}

fn match_glob<'a>(glob: &'a CompiledGlob, path: &'a str) -> Option<Vec<(&'a str, String)>> {
    let path_segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let mut captures: Vec<(&str, String)> = Vec::new();
    let mut pi = 0;
    let mut xi = 0;
    while pi < glob.segments.len() {
        let seg = &glob.segments[pi];
        if let GlobSegment::CatchAll(name) = seg {
            // CatchAll consumes the rest of the path; compile_glob already
            // asserted it's the last segment.
            let rest = path_segs[xi..].join("/");
            captures.push((name.as_str(), rest));
            return Some(captures);
        }
        if xi >= path_segs.len() {
            return None;
        }
        let x = path_segs[xi];
        match seg {
            GlobSegment::Literal(lit) => {
                if lit != x {
                    return None;
                }
            }
            GlobSegment::SingleCapture(name) => {
                if x.is_empty() {
                    return None;
                }
                captures.push((name.as_str(), x.to_string()));
            }
            GlobSegment::Star => {
                if x.is_empty() {
                    return None;
                }
            }
            GlobSegment::CatchAll(_) => unreachable!(),
        }
        pi += 1;
        xi += 1;
    }
    if xi != path_segs.len() {
        return None;
    }
    Some(captures)
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

impl CompiledManifest {
    pub fn dispatch(&self, method: &str, path: &str) -> Outcome {
        const MAX_HOPS: u32 = 8;
        let method_mask = request_method_bit(method);
        let mut current_path = path.to_string();
        for _hop in 0..MAX_HOPS {
            let mut rewrote = false;
            for rule in &self.rules {
                if (method_mask & rule.methods) == 0 {
                    continue;
                }
                let captures = match &rule.matcher {
                    CompiledMatch::Exact(p) => {
                        if current_path == *p {
                            Captures::Empty
                        } else {
                            continue;
                        }
                    }
                    CompiledMatch::Prefix(bare) => {
                        if match_prefix_bare(bare, &current_path) {
                            Captures::Empty
                        } else {
                            continue;
                        }
                    }
                    CompiledMatch::Glob(g) => match match_glob(g, &current_path) {
                        Some(c) => Captures::Glob(c),
                        None => continue,
                    },
                    CompiledMatch::Any => Captures::Empty,
                };
                match self.resolve(&rule.action, &current_path, &captures) {
                    ResolveResult::Outcome(o) => return o,
                    ResolveResult::Rewrite(new_path) => {
                        current_path = new_path;
                        rewrote = true;
                        break;
                    }
                    ResolveResult::Continue => continue,
                }
            }
            if !rewrote {
                return Outcome::NotFound;
            }
        }
        Outcome::NotFound
    }

    fn resolve(
        &self,
        action: &CompiledAction,
        path: &str,
        captures: &Captures<'_>,
    ) -> ResolveResult {
        match action {
            CompiledAction::Static { try_chain, cache, status } => {
                for tpl in try_chain {
                    let resolved = tpl.render(path, captures);
                    if let Some((entry, mutable)) = self.lookup_asset(&resolved) {
                        return ResolveResult::Outcome(Outcome::Static(StaticHit {
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
                // Static rule matched but no asset resolved — the original
                // walker bubbled this as Outcome::NotFound (terminal).
                ResolveResult::Outcome(Outcome::NotFound)
            }
            CompiledAction::Worker { mode, cache, rate_limit } => {
                ResolveResult::Outcome(Outcome::Worker {
                    mode: *mode,
                    cache: cache.clone(),
                    rate_limit: rate_limit.clone(),
                })
            }
            CompiledAction::Redirect { to, status } => {
                ResolveResult::Outcome(Outcome::Redirect {
                    to: to.render(path, captures),
                    status: *status,
                })
            }
            CompiledAction::Rewrite { to } => {
                ResolveResult::Rewrite(to.render(path, captures))
            }
        }
    }

    fn lookup_asset(&self, path: &str) -> Option<(&AssetEntry, bool)> {
        if let Some(e) = self.runtime_assets.get(path) {
            return Some((e, true));
        }
        if let Some(e) = self.assets.get(path) {
            return Some((e, false));
        }
        None
    }
}

enum ResolveResult {
    Outcome(Outcome),
    Rewrite(String),
    #[allow(dead_code)]
    Continue,
}

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
    use zeroship_core::types::{
        Action, AssetEntry, Manifest, Match, Rule, WorkerMode,
    };

    fn asset(hash: &str, ct: &str) -> AssetEntry {
        AssetEntry {
            hash: hash.into(),
            content_type: ct.into(),
            size: 0,
            cache: None,
            updated_at: 0,
        }
    }

    #[test]
    fn compile_preserves_dispatch_semantics_for_passthrough() {
        let m = Manifest::passthrough();
        let c = CompiledManifest::compile(&m);
        match c.dispatch("POST", "/_rpc/listTodos") {
            Outcome::Worker { mode, .. } => assert_eq!(mode, WorkerMode::Rpc),
            other => panic!("expected Worker(Rpc), got {other:?}"),
        }
        match c.dispatch("GET", "/") {
            Outcome::Worker { mode, .. } => assert_eq!(mode, WorkerMode::Ssr),
            other => panic!("expected Worker(Ssr), got {other:?}"),
        }
    }

    #[test]
    fn compile_handles_static_method_narrowing() {
        let m = Manifest {
            rules: vec![Rule {
                r#match: Match::Any,
                action: Action::Static {
                    r#try: vec!["/index.html".into()],
                    cache: None,
                    status: None,
                },
            }],
            assets: HashMap::from([(
                "/index.html".into(),
                asset("h1", "text/html"),
            )]),
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        match c.dispatch("GET", "/") {
            Outcome::Static(hit) => assert_eq!(hit.hash, "h1"),
            other => panic!("expected Static, got {other:?}"),
        }
        match c.dispatch("POST", "/") {
            Outcome::NotFound => {}
            other => panic!("expected NotFound for POST, got {other:?}"),
        }
    }

    #[test]
    fn compile_glob_captures_propagate_to_redirect_template() {
        let m = Manifest {
            rules: vec![Rule {
                r#match: Match::Glob {
                    method: None,
                    path: "/old/[slug]".into(),
                },
                action: Action::Redirect {
                    to: "/new/[slug]".into(),
                    status: 301,
                },
            }],
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        match c.dispatch("GET", "/old/foo") {
            Outcome::Redirect { to, status } => {
                assert_eq!(to, "/new/foo");
                assert_eq!(status, 301);
            }
            other => panic!("expected Redirect, got {other:?}"),
        }
    }

    #[test]
    fn compile_rewrite_chain_resolves_to_terminal() {
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
            assets: HashMap::from([(
                "/new.html".into(),
                asset("h-new", "text/html"),
            )]),
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        match c.dispatch("GET", "/old") {
            Outcome::Static(hit) => assert_eq!(hit.hash, "h-new"),
            other => panic!("expected Static after rewrite, got {other:?}"),
        }
    }

    #[test]
    fn compile_circular_rewrite_trips_max_hops() {
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
            ..Manifest::default()
        };
        let c = CompiledManifest::compile(&m);
        match c.dispatch("GET", "/a") {
            Outcome::NotFound => {}
            other => panic!("expected NotFound after circular rewrite, got {other:?}"),
        }
    }

    #[test]
    fn compile_template_path_substitution() {
        // Bare $path
        let t = CompiledTemplate::parse("$path");
        assert_eq!(t.chunks.len(), 1);
        assert!(matches!(t.chunks[0], TemplateChunk::Path));
        assert_eq!(t.render("/foo", &Captures::Empty), "/foo");

        // $path/x — Path then literal
        let t = CompiledTemplate::parse("$path/x");
        assert_eq!(t.chunks.len(), 2);
        assert!(matches!(t.chunks[0], TemplateChunk::Path));
        assert!(matches!(&t.chunks[1], TemplateChunk::Literal(s) if s == "/x"));
        assert_eq!(t.render("/foo", &Captures::Empty), "/foo/x");

        // /[slug]/x — literal "/", capture, literal "/x"
        let t = CompiledTemplate::parse("/[slug]/x");
        assert_eq!(t.chunks.len(), 3);
        assert!(matches!(&t.chunks[0], TemplateChunk::Literal(s) if s == "/"));
        assert!(matches!(&t.chunks[1], TemplateChunk::Capture(n) if n == "slug"));
        assert!(matches!(&t.chunks[2], TemplateChunk::Literal(s) if s == "/x"));
        let caps = Captures::Glob(vec![("slug", "hello".to_string())]);
        assert_eq!(t.render("/ignored", &caps), "/hello/x");

        // /[...rest] — literal "/", capture (catch-all uses the same name)
        let t = CompiledTemplate::parse("/[...rest]");
        assert_eq!(t.chunks.len(), 2);
        assert!(matches!(&t.chunks[0], TemplateChunk::Literal(s) if s == "/"));
        assert!(matches!(&t.chunks[1], TemplateChunk::Capture(n) if n == "rest"));
        let caps = Captures::Glob(vec![("rest", "a/b/c".to_string())]);
        assert_eq!(t.render("/ignored", &caps), "/a/b/c");

        // All-literal collapses to one chunk
        let t = CompiledTemplate::parse("/static/path");
        assert_eq!(t.chunks.len(), 1);
        assert!(matches!(&t.chunks[0], TemplateChunk::Literal(s) if s == "/static/path"));
    }

    #[test]
    fn compile_prefix_normalizes_trailing_slash() {
        // Both "/admin/" and "/admin" should produce a matcher that hits
        // /admin and /admin/users identically.
        let with_slash = compile_match(&Match::Prefix {
            method: None,
            path: "/admin/".into(),
        });
        let without_slash = compile_match(&Match::Prefix {
            method: None,
            path: "/admin".into(),
        });
        let CompiledMatch::Prefix(a) = with_slash else {
            panic!("expected Prefix");
        };
        let CompiledMatch::Prefix(b) = without_slash else {
            panic!("expected Prefix");
        };
        assert_eq!(a, b, "trailing slash should be normalized away");
        assert_eq!(a, "/admin");

        for p in [&a, &b] {
            assert!(match_prefix_bare(p, "/admin"));
            assert!(match_prefix_bare(p, "/admin/users"));
            assert!(!match_prefix_bare(p, "/administrator"));
            assert!(!match_prefix_bare(p, "/other"));
        }
    }
}
