//! Pluggable OAuth provider system built on `openidconnect` + `oauth2`.
//!
//! Two kinds of providers are supported:
//!
//! - **OIDC** (Google, Apple) — uses OpenID Connect Discovery and extracts user
//!   profile from the `id_token` claims.
//! - **Plain OAuth2** (GitHub, Meta) — uses manually configured endpoints and
//!   fetches user profile from a provider-specific userinfo API.
//!
//! All providers share the same [`Provider`] struct and are stored in a
//! [`ProviderRegistry`] keyed by name. PKCE is used for every provider.

pub mod apple;
pub mod github;
pub mod google;
pub mod meta;

use std::collections::HashMap;
use std::sync::OnceLock;

use openidconnect::core::{
    CoreAuthenticationFlow, CoreClient, CoreIdTokenClaims, CoreJsonWebKey,
    CoreJwsSigningAlgorithm, CoreProviderMetadata, CoreResponseType,
    CoreSubjectIdentifierType, CoreTokenResponse,
};
use openidconnect::{
    AuthUrl, AuthorizationCode, CsrfToken, EmptyAdditionalProviderMetadata,
    EndpointMaybeSet, EndpointNotSet, EndpointSet, IssuerUrl, JsonWebKeySet,
    JsonWebKeySetUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier,
    ResponseTypes, Scope, TokenResponse, TokenUrl, http,
};

/// The concrete `CoreClient` type returned by both `discover_async` ->
/// `from_provider_metadata()` and manual `CoreProviderMetadata::new()` ->
/// `from_provider_metadata()`. Both code paths produce the same typestate:
///
/// - `HasAuthUrl = EndpointSet`        (always set)
/// - `HasDeviceAuthUrl = EndpointNotSet`
/// - `HasIntrospectionUrl = EndpointNotSet`
/// - `HasRevocationUrl = EndpointNotSet`
/// - `HasTokenUrl = EndpointMaybeSet`  (Some for all our providers)
/// - `HasUserInfoUrl = EndpointMaybeSet`
pub(crate) type OAuthCoreClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

// ---------------------------------------------------------------------------
// Cyper HTTP adapter for openidconnect
// ---------------------------------------------------------------------------

/// Shared cyper HTTP client — reuses connections across OAuth calls.
pub(crate) fn cyper_client() -> &'static cyper::Client {
    static CLIENT: OnceLock<cyper::Client> = OnceLock::new();
    CLIENT.get_or_init(cyper::Client::new)
}

/// Adapter that bridges `cyper` (compio-native) to the `oauth2::AsyncHttpClient`
/// trait expected by `openidconnect`.
///
/// `oauth2` implements `AsyncHttpClient<'c>` for any
/// `Fn(http::Request<Vec<u8>>) -> Future<Output = Result<http::Response<Vec<u8>>, E>>`,
/// so we provide a closure-compatible async function.
pub(crate) async fn http_client_fn(
    request: http::Request<Vec<u8>>,
) -> Result<http::Response<Vec<u8>>, CyperAdapterError> {
    let client = cyper_client();

    let method: http::Method = request.method().clone();
    let url: String = request.uri().to_string();

    let mut builder = client
        .request(method, &url)
        .map_err(|e| CyperAdapterError(format!("build request: {e}")))?;

    for (name, value) in request.headers() {
        let val_str: &str = value
            .to_str()
            .map_err(|e| CyperAdapterError(format!("header value: {e}")))?;
        builder = builder
            .header(name.as_str(), val_str)
            .map_err(|e| CyperAdapterError(format!("set header: {e}")))?;
    }

    let body: Vec<u8> = request.into_body();
    if !body.is_empty() {
        builder = builder.body(body);
    }

    let resp = builder
        .send()
        .await
        .map_err(|e| CyperAdapterError(format!("send: {e}")))?;

    let status: http::StatusCode = resp.status();
    let headers: http::HeaderMap = resp.headers().clone();
    let body_bytes: Vec<u8> = resp
        .bytes()
        .await
        .map_err(|e| CyperAdapterError(format!("read body: {e}")))?
        .to_vec();

    let mut response = http::Response::builder().status(status);
    for (k, v) in &headers {
        response = response.header(k, v);
    }

    response
        .body(body_bytes)
        .map_err(|e| CyperAdapterError(format!("build response: {e}")))
}

/// Error type for the cyper adapter, implementing `std::error::Error`.
#[derive(Debug)]
pub struct CyperAdapterError(pub String);

impl std::fmt::Display for CyperAdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CyperAdapterError: {}", self.0)
    }
}

impl std::error::Error for CyperAdapterError {}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

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
// Provider kind
// ---------------------------------------------------------------------------

/// Distinguishes OIDC providers (which have id_tokens) from plain OAuth2
/// providers (which need a manual userinfo API call).
#[derive(Debug, Clone)]
pub(crate) enum ProviderKind {
    /// Full OIDC — extract profile from id_token claims.
    Oidc,
    /// Plain OAuth2 — the access token is used by the `profile_fetcher` to
    /// call a provider-specific userinfo API.
    OAuth2,
}

/// Function pointer that fetches a user profile from a provider's API using
/// an access token. Returns a boxed future for dyn-compatibility.
pub(crate) type ProfileFetcher = fn(
    &str,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<OAuthProfile, String>> + Send + '_>,
>;

// ---------------------------------------------------------------------------
// Helper: build CoreProviderMetadata for non-OIDC providers
// ---------------------------------------------------------------------------

/// Construct a `CoreProviderMetadata` for a plain OAuth2 provider that does
/// not support OpenID Connect Discovery.
///
/// The resulting metadata has dummy values for fields that only matter for
/// OIDC (JWKS URI, signing algorithms, subject types). This lets us call
/// `CoreClient::from_provider_metadata()` and get the same typestate as
/// a discovery-based OIDC client.
pub(crate) fn manual_provider_metadata(
    issuer: &str,
    auth_url: &str,
    token_url: &str,
) -> Result<CoreProviderMetadata, String> {
    let issuer = IssuerUrl::new(issuer.to_string())
        .map_err(|e| format!("invalid issuer: {e}"))?;
    let auth_endpoint = AuthUrl::new(auth_url.to_string())
        .map_err(|e| format!("invalid auth URL: {e}"))?;
    let token_endpoint = TokenUrl::new(token_url.to_string())
        .map_err(|e| format!("invalid token URL: {e}"))?;

    // Dummy JWKS URI — non-OIDC providers don't use JWKS, but the
    // ProviderMetadata constructor requires one.
    let jwks_uri = JsonWebKeySetUrl::new("https://localhost/.well-known/jwks.json".to_string())
        .map_err(|e| format!("invalid jwks URI: {e}"))?;

    let metadata = CoreProviderMetadata::new(
        issuer,
        auth_endpoint,
        jwks_uri,
        // response_types_supported: authorization code flow
        vec![ResponseTypes::<CoreResponseType>::new(vec![CoreResponseType::Code])],
        // subject_types_supported
        vec![CoreSubjectIdentifierType::Public],
        // id_token_signing_alg_values_supported (dummy — won't verify id_tokens)
        vec![CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256],
        EmptyAdditionalProviderMetadata {},
    )
    .set_token_endpoint(Some(token_endpoint))
    // Override the JWKS with an empty set (no keys to verify).
    .set_jwks(JsonWebKeySet::<CoreJsonWebKey>::new(vec![]));

    Ok(metadata)
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// A unified OAuth provider backed by `openidconnect::CoreClient`.
///
/// OIDC providers (Google, Apple) are constructed via discovery.
/// Plain OAuth2 providers (GitHub, Meta) are constructed with manual endpoints.
/// Both share the same `OAuthCoreClient` typestate.
pub struct Provider {
    /// Human-readable name (e.g., `"google"`, `"github"`).
    pub(crate) name: String,
    /// The openidconnect client — handles auth URL generation, token exchange.
    pub(crate) client: OAuthCoreClient,
    /// What kind of provider this is (OIDC vs plain OAuth2).
    pub(crate) kind: ProviderKind,
    /// Scopes to request during authorization.
    pub(crate) scopes: Vec<String>,
    /// Async function to fetch an `OAuthProfile` from a provider-specific API
    /// using an access token. Only used for `ProviderKind::OAuth2`.
    pub(crate) profile_fetcher: Option<ProfileFetcher>,
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl Provider {
    /// Provider name (e.g., `"google"`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Build the authorization URL with PKCE.
    ///
    /// Returns `(url, csrf_token, nonce, pkce_verifier)`. The caller must
    /// store `csrf_token`, `nonce`, and `pkce_verifier` (e.g., in cookies)
    /// so that `exchange()` can use them during the callback.
    pub fn authorize_url(
        &self,
        custom_state: &str,
    ) -> (String, CsrfToken, Nonce, PkceCodeVerifier) {
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

        let state_value = custom_state.to_string();
        let mut auth_req = self.client.authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            move || CsrfToken::new(state_value),
            Nonce::new_random,
        );

        for scope in &self.scopes {
            auth_req = auth_req.add_scope(Scope::new(scope.clone()));
        }

        auth_req = auth_req.set_pkce_challenge(pkce_challenge);

        let (url, csrf_token, nonce) = auth_req.url();

        (url.to_string(), csrf_token, nonce, pkce_verifier)
    }

    /// Exchange an authorization code for a user profile.
    ///
    /// For OIDC providers, the profile is extracted from the id_token claims.
    /// For plain OAuth2 providers, the access token is used to call the
    /// provider-specific userinfo API.
    pub async fn exchange(
        &self,
        code: &str,
        pkce_verifier: PkceCodeVerifier,
        nonce: &Nonce,
    ) -> Result<OAuthProfile, String> {
        // Exchange the authorization code for tokens.
        let token_response: CoreTokenResponse = self
            .client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .map_err(|e| format!("{}: config error: {e}", self.name))?
            .set_pkce_verifier(pkce_verifier)
            .request_async(&http_client_fn)
            .await
            .map_err(|e| format!("{}: token exchange: {e}", self.name))?;

        match &self.kind {
            ProviderKind::Oidc => {
                self.extract_oidc_profile(&token_response, nonce)
            }
            ProviderKind::OAuth2 => {
                let access_token = token_response.access_token().secret().to_string();
                let fetcher = self.profile_fetcher.as_ref().ok_or_else(|| {
                    format!("{}: no profile_fetcher configured", self.name)
                })?;
                fetcher(&access_token).await
            }
        }
    }

    /// Extract user profile from the id_token claims (OIDC providers only).
    fn extract_oidc_profile(
        &self,
        token_response: &CoreTokenResponse,
        nonce: &Nonce,
    ) -> Result<OAuthProfile, String> {
        let id_token = token_response
            .id_token()
            .ok_or_else(|| format!("{}: no id_token in response", self.name))?;

        let claims: &CoreIdTokenClaims = id_token
            .claims(&self.client.id_token_verifier(), nonce)
            .map_err(|e| format!("{}: verify id_token: {e}", self.name))?;

        // Extract email — required.
        let email = match claims.email() {
            Some(e) => e.as_str().to_string(),
            None => return Err(format!("{}: no email in id_token", self.name)),
        };

        // Extract name — may not be present (especially Apple after first auth).
        // Falls back to the email prefix.
        let name = match claims.name() {
            Some(localized) => match localized.get(None) {
                Some(n) => n.as_str().to_string(),
                None => email.split('@').next().unwrap_or("user").to_string(),
            },
            None => email.split('@').next().unwrap_or("user").to_string(),
        };

        // Extract avatar URL — optional.
        let avatar_url = match claims.picture() {
            Some(localized) => localized.get(None).map(|u| u.as_str().to_string()),
            None => None,
        };

        Ok(OAuthProfile { email, name, avatar_url })
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// A registry of OAuth providers, keyed by provider name.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: HashMap<String, Provider>,
}

impl ProviderRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider. Overwrites any existing provider with the same name.
    pub fn register(&mut self, provider: Provider) {
        self.providers.insert(provider.name.clone(), provider);
    }

    /// Look up a provider by name.
    pub fn get(&self, name: &str) -> Option<&Provider> {
        self.providers.get(name)
    }
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .finish()
    }
}
