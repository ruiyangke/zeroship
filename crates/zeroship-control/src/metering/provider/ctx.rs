use std::sync::Arc;

use serde::de::DeserializeOwned;

use super::{LiteStore, ProviderError};
use crate::SecretString;

thread_local! {
    static PROVIDER_HTTP_CLIENT: cyper::Client = cyper::Client::new();
}

/// Cheap handle that returns the calling thread's `cyper` client.
#[derive(Debug, Clone, Copy, Default)]
pub struct HttpClientFactory;

impl HttpClientFactory {
    #[must_use]
    pub fn client(&self) -> cyper::Client {
        PROVIDER_HTTP_CLIENT.with(Clone::clone)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Clock;

impl Clock {
    #[must_use]
    pub fn now_unix(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }
}

/// A secret as an operator writes it inside `--provider-config`: the material
/// itself, or `urn:zeroship:file:<path>` naming the file that holds it.
///
/// THIS IS THE PLATFORM'S ONE SECRET GRAMMAR, not a second one for billing. It
/// is the same input `Secret<T>` takes for `control.stripe_secret_key` and every
/// other declared secret ([`zeroship_core::config::parse_secret_ref`]), so a
/// provider secret is written the way every other secret is written and the file
/// arm gets the same owner-only permission refusal.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(transparent)]
pub struct SecretInput(pub String);

pub trait SecretResolver: Send + Sync {
    fn resolve(&self, input: &SecretInput) -> Result<SecretString, ProviderError>;
}

/// Resolves a provider secret through the platform secret grammar.
///
/// # What this replaced, and why the replacement is not the deleted `env:` arm
///
/// Until 2026-08-20 this was a name lookup into a map the control plane built at
/// boot, and `main.rs` put exactly two names in it: `stripe_secret_key` and
/// `stripe_webhook_secret`. So `lago` and `openmeter`, whose keys are their own
/// and are not any Stripe secret, had NO name they could resolve: every value an
/// operator could write for `lago.api_key` was "unknown secret handle", the
/// factory returned `Config`, and `main.rs` exited 1. `--meter-provider lago`
/// could not boot at all, for anyone. The three Lago e2e harnesses and
/// `e2e_openmeter_export.sh` were where that surfaced.
///
/// 94c7ba7dd deleted an `env:<NAME>` arm here for a good reason that still
/// stands: a config string naming an environment variable is env-to-env
/// indirection, the read has no declared identity, and the provider could be
/// pointed at any variable in the process. Nothing below reads the environment.
/// A literal is the material; a `urn:zeroship:file:` reference is a path the
/// operator wrote, permission-checked before it is read.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlatformSecretResolver;

impl SecretResolver for PlatformSecretResolver {
    fn resolve(&self, input: &SecretInput) -> Result<SecretString, ProviderError> {
        let raw = input.0.trim();
        if raw.is_empty() {
            return Err(ProviderError::Config("empty provider secret".to_string()));
        }
        zeroship_core::config::resolve_secret(raw)
            .map(SecretString::new)
            .map_err(|e| ProviderError::Config(format!("provider secret: {e}")))
    }
}

pub struct ProviderCtx {
    pub http: HttpClientFactory,
    pub raw_config: serde_json::Value,
    pub secrets: Arc<dyn SecretResolver>,
    pub clock: Clock,
    pub store: Option<Arc<dyn LiteStore>>,
}

impl std::fmt::Debug for ProviderCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderCtx")
            .field("http", &self.http)
            .field("raw_config", &self.raw_config)
            .field("secrets", &"<secret resolver>")
            .field("clock", &self.clock)
            .field("has_store", &self.store.is_some())
            .finish()
    }
}

impl ProviderCtx {
    #[must_use]
    pub fn new(
        raw_config: serde_json::Value,
        secrets: Arc<dyn SecretResolver>,
        store: Option<Arc<dyn LiteStore>>,
    ) -> Self {
        Self {
            http: HttpClientFactory,
            raw_config,
            secrets,
            clock: Clock,
            store,
        }
    }

    pub fn parse_adapter_config<T: DeserializeOwned>(&self, id: &str) -> Result<T, ProviderError> {
        let value = self
            .raw_config
            .get(id)
            .cloned()
            .unwrap_or_else(|| self.raw_config.clone());
        serde_json::from_value(value)
            .map_err(|e| ProviderError::Config(format!("{id}: provider config is invalid: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Lago e2e harnesses write the key Lago's own `db:prepare` seeds. It is
    /// a literal by nature: it is not a Stripe secret the control plane loaded,
    /// and there is no third party to have pre-resolved it.
    const LAGO_SEEDED_KEY: &str = "lago_key-hooli-1234567890";

    fn resolve(raw: &str) -> Result<String, ProviderError> {
        PlatformSecretResolver
            .resolve(&SecretInput(raw.to_string()))
            .map(|s| s.expose_secret().to_string())
    }

    /// REGRESSION. From 94c7ba7dd (2026-08-13) to 2026-08-20 this was
    /// `Err(Config("unknown secret handle 'lago_key-hooli-1234567890'"))`, the
    /// lago factory propagated it, and `zeroship-control` exited 1 before
    /// binding a port. Every assertion in `tests/e2e_lago_billing.sh`,
    /// `tests/e2e_event_redelivery_dedup.sh` and
    /// `tests/e2e_multi_app_attribution.sh` was unreachable behind it.
    #[test]
    fn a_provider_key_written_as_a_literal_resolves_to_itself() {
        assert_eq!(resolve(LAGO_SEEDED_KEY).unwrap(), LAGO_SEEDED_KEY);
    }

    #[test]
    fn a_file_reference_resolves_to_the_file_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lago-api-key");
        std::fs::write(&path, format!("{LAGO_SEEDED_KEY}\n")).expect("write");
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("chmod");
        let raw = format!("urn:zeroship:file:{}", path.display());
        assert_eq!(resolve(&raw).unwrap(), LAGO_SEEDED_KEY);
    }

    /// A reserved-prefix value is never silently taken as literal material, so a
    /// typo in the file scheme refuses instead of becoming the provider's key.
    #[test]
    fn an_unrecognized_reserved_prefix_is_refused() {
        let err = resolve("urn:zeroship:vault:billing/lago").unwrap_err();
        assert!(
            matches!(&err, ProviderError::Config(m) if m.contains("malformed secret reference")),
            "want a malformed-reference refusal, got {err:?}"
        );
    }

    /// The env-to-env arm 94c7ba7dd deleted stays deleted: `env:LAGO_API_KEY` is
    /// not a reference, so it is the literal string, and no environment variable
    /// is read to produce it.
    #[test]
    fn an_env_prefixed_value_reads_no_environment_variable() {
        assert_eq!(resolve("env:LAGO_API_KEY").unwrap(), "env:LAGO_API_KEY");
    }

    #[test]
    fn an_empty_secret_is_refused() {
        let err = resolve("   ").unwrap_err();
        assert!(
            matches!(&err, ProviderError::Config(m) if m.contains("empty provider secret")),
            "want an empty-secret refusal, got {err:?}"
        );
    }
}
