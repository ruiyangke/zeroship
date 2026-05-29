//! Live JWKS round-trip against a running hydra (local stack).
//!
//! Skips when `AUTH_HYDRA_ADMIN` is unset so CI without docker still
//! passes. When the env var IS set (i.e. the local auth stack is up),
//! we fetch hydra's JWKS from the public endpoint (:4444) and assert
//! the cache populates at least one signing key.
//!
//! Verifying an actual ID token is covered transitively by P3-U11's
//! full-stack e2e, where hydra mints a token end-to-end.

use zeroship_core::oidc_verify::JwksCache;

#[compio::test]
async fn jwks_cache_fetches_from_live_hydra() {
    if std::env::var("AUTH_HYDRA_ADMIN").is_err() {
        eprintln!("skip: AUTH_HYDRA_ADMIN unset");
        return;
    }
    // Hydra public is reachable on :4444 in the local stack.
    let cache = JwksCache::new("http://localhost:4444/.well-known/jwks.json");
    cache.refresh().await.expect("refresh");
    let keys = cache.keys().await.expect("keys");
    assert!(!keys.is_empty(), "expected at least one signing key");
}
