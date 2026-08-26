//! Small shared helpers used across the router submodules.
//!
//! These are deliberately framework-thin — they don't own any state and
//! they don't reach into `GateState`. Keeping them in their own file
//! avoids polluting the larger semantic groups (dispatch, static_serve,
//! variants) with utility noise.

use ntex::web::HttpRequest;

/// Pull a header off a request as a borrowed `&str`. `None` when absent
/// or non-UTF-8. Used by the static-serve path for `If-None-Match` /
/// `Accept-Encoding` lookups where we want to avoid the allocation of
/// `to_string()`.
pub(super) fn header_str_borrowed<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
    req.headers().get(name).and_then(|v| v.to_str().ok())
}

/// Hash a resource key into a stable u32 for use as the
/// `PerRuleRateLimitRegistry` rule_idx. xxh3 keeps the
/// hash deterministic across processes; we truncate to u32 because the
/// registry's bucket key only differentiates by `(app_id, rule_idx,
/// bucket)` and a 32-bit space is plenty for the per-app resource set.
pub(super) fn resource_key_hash(key: &str) -> u32 {
    xxhash_rust::xxh3::xxh3_64(key.as_bytes()) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_key_hash_is_stable() {
        let a = resource_key_hash("rpc:todos.list");
        let b = resource_key_hash("rpc:todos.list");
        assert_eq!(a, b, "deterministic across calls");
        let c = resource_key_hash("rpc:todos.add");
        assert_ne!(a, c, "different keys hash differently");
    }
}
