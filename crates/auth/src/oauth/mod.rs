//! Pluggable OAuth provider system — Google, GitHub, and extensible to others.
//!
//! Each provider implements [`OAuthProvider`], which knows how to build an
//! authorization URL and exchange an authorization code for a user profile.

pub mod apple;
pub mod github;
pub mod google;
pub mod meta;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

/// Shared cyper HTTP client — reuses connections across OAuth calls.
fn http_client() -> &'static cyper::Client {
    static CLIENT: OnceLock<cyper::Client> = OnceLock::new();
    CLIENT.get_or_init(cyper::Client::new)
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The future type returned by [`OAuthProvider::exchange`].
pub type ExchangeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<OAuthProfile, String>> + Send + 'a>>;

/// User profile returned by an OAuth provider after authentication.
#[derive(Debug, Clone)]
pub struct OAuthProfile {
    pub email: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

/// Configuration for an OAuth provider (client credentials + redirect URI).
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

// ---------------------------------------------------------------------------
// Provider trait (dyn-compatible)
// ---------------------------------------------------------------------------

/// Trait for OAuth providers. Each provider (Google, GitHub) implements this.
///
/// The `exchange` method returns a boxed future so that the trait is
/// dyn-compatible and providers can be stored in a `HashMap<String, Box<dyn OAuthProvider>>`.
pub trait OAuthProvider: Send + Sync {
    /// Provider name (e.g., `"google"`, `"github"`).
    fn name(&self) -> &str;

    /// Build the authorization URL that the user's browser is redirected to.
    fn authorize_url(&self, state: &str) -> String;

    /// Exchange an authorization code for a user profile.
    ///
    /// Makes server-to-server HTTPS calls to the provider's token endpoint
    /// and userinfo endpoint.
    fn exchange(&self, code: &str) -> ExchangeFuture<'_>;
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// A registry of OAuth providers, keyed by provider name.
#[derive(Default)]
pub struct OAuthRegistry {
    providers: HashMap<String, Box<dyn OAuthProvider>>,
}

impl OAuthRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider. Overwrites any existing provider with the same name.
    pub fn register(&mut self, provider: impl OAuthProvider + 'static) {
        self.providers
            .insert(provider.name().to_string(), Box::new(provider));
    }

    /// Look up a provider by name.
    pub fn get(&self, name: &str) -> Option<&dyn OAuthProvider> {
        self.providers.get(name).map(|b| b.as_ref())
    }
}

impl std::fmt::Debug for OAuthRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthRegistry")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .finish()
    }
}
