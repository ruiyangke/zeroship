//! Boot-time reconciliation of the first-party OAuth clients from config.
//!
//! This replaces the three `/admin/oauth-clients` routes. Registering a
//! relying party of the platform's own OP is a deployment decision - the
//! console and the CLI are the whole set today - so it belongs in the config
//! overlay (`[auth] oauth_clients`), read once at boot, and not behind an
//! operator credential at runtime.
//!
//! Every validation the deleted `create_oauth_client` performed happens here
//! instead, against the same inputs: the closed scope vocabulary, the redirect
//! URI rules (HTTPS, or loopback HTTP, absolute, no fragment), and the
//! auth-method vocabulary. What is NOT carried over is the generate-and-show-
//! once secret: a config file cannot be handed a value it never saw, so a
//! confidential client supplies its own secret and only the hash is persisted.
//!
//! `skip_consent` stays DERIVED from `[auth] trusted_oauth_clients`, exactly as
//! the route derived it. A registration that could set its own would be a
//! consent bypass written by whoever can edit one list instead of two.
//!
//! Failure is FATAL at boot, deliberately. A rejected registration under the
//! old route was a 400 to an operator who could see it; here the operator is
//! not present, so a silently skipped client would be a login surface that
//! quietly does not exist.

use std::collections::HashSet;

use compio_postgres::Client;
use zeroship_authz::Scope;
use zeroship_core::auth::hash_api_key;
use zeroship_core::config::OauthClientRegistration;
use zeroship_core::device_grant::PLATFORM_CLI_CLIENT_ID;
use zeroship_core::typed_id::app_id_from_oauth_client_id;

const MAX_REDIRECT_URIS: usize = 100;
const MAX_REDIRECT_URI_LEN: usize = 2048;

/// What one reconcile pass changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OauthClientReconcileReport {
    /// Registrations upserted from config.
    pub registered: usize,
    /// First-party rows de-registered because config no longer names them.
    pub pruned: usize,
}

/// Reconcile `zeroship.oauth_clients` against the configured first-party set.
///
/// `None` leaves the table untouched - a deployment that does not use the key
/// keeps whatever is registered. `Some(list)` is AUTHORITATIVE for first-party
/// clients: each entry is upserted, and any other first-party row is deleted.
///
/// Two client families are excluded from that pruning because this is not their
/// registrar: per-app end-user clients (`oac_…`), written by the deploy path,
/// and [`PLATFORM_CLI_CLIENT_ID`], which the auth service reconciles at its own
/// boot. Pruning either would make two services fight over the same rows.
///
/// # Errors
///
/// Returns a human-readable message when a registration is invalid or the
/// database rejects the write. Callers treat this as fatal.
pub async fn reconcile_oauth_clients(
    pg: &Client,
    configured: Option<&[OauthClientRegistration]>,
    trusted: &HashSet<String>,
) -> Result<OauthClientReconcileReport, String> {
    let Some(registrations) = configured else {
        return Ok(OauthClientReconcileReport::default());
    };

    let mut seen: HashSet<&str> = HashSet::new();
    for registration in registrations {
        if !seen.insert(registration.client_id.as_str()) {
            return Err(format!(
                "oauth client {:?} is registered twice",
                registration.client_id
            ));
        }
    }

    let mut report = OauthClientReconcileReport::default();
    for registration in registrations {
        upsert(pg, registration, trusted).await?;
        report.registered += 1;
    }
    report.pruned = prune(pg, &seen).await?;
    Ok(report)
}

async fn upsert(
    pg: &Client,
    registration: &OauthClientRegistration,
    trusted: &HashSet<String>,
) -> Result<(), String> {
    let scopes = validate(registration)?;
    let skip_consent = trusted.contains(&registration.client_id);
    let client_secret_hash = registration.client_secret.as_deref().map(hash_api_key);
    let redirect_uris: Vec<&str> = registration
        .redirect_uris
        .iter()
        .map(String::as_str)
        .collect();
    let scopes: Vec<&str> = scopes.iter().map(String::as_str).collect();

    pg.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
             skip_consent, created_by, client_secret_hash, \
             refresh_allowed, token_endpoint_auth_method) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, NULL, $8, $9, $10) \
         ON CONFLICT (client_id) DO UPDATE SET \
            client_name = EXCLUDED.client_name, \
            client_uri = EXCLUDED.client_uri, \
            logo_uri = EXCLUDED.logo_uri, \
            redirect_uris = EXCLUDED.redirect_uris, \
            scopes = EXCLUDED.scopes, \
            skip_consent = EXCLUDED.skip_consent, \
            client_secret_hash = EXCLUDED.client_secret_hash, \
            refresh_allowed = EXCLUDED.refresh_allowed, \
            token_endpoint_auth_method = EXCLUDED.token_endpoint_auth_method",
        &[
            &registration.client_id,
            &registration.client_name,
            &registration.client_uri,
            &registration.logo_uri,
            &redirect_uris,
            &scopes,
            &skip_consent,
            &client_secret_hash,
            &registration.refresh_allowed,
            &registration.token_endpoint_auth_method,
        ],
    )
    .await
    .map_err(|err| {
        format!(
            "registering oauth client {:?}: {err}",
            registration.client_id
        )
    })?;
    Ok(())
}

async fn prune(pg: &Client, keep: &HashSet<&str>) -> Result<usize, String> {
    let rows = pg
        .query("SELECT client_id FROM zeroship.oauth_clients", &[])
        .await
        .map_err(|err| format!("listing oauth clients: {err}"))?;

    let mut pruned = 0;
    for row in &rows {
        let client_id: String = row.get("client_id");
        if keep.contains(client_id.as_str())
            || client_id == PLATFORM_CLI_CLIENT_ID
            || app_id_from_oauth_client_id(&client_id).is_some()
        {
            continue;
        }
        pg.execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .map_err(|err| format!("de-registering oauth client {client_id:?}: {err}"))?;
        tracing::info!(
            client_id = %client_id,
            "control: de-registered an oauth client the config no longer names"
        );
        pruned += 1;
    }
    Ok(pruned)
}

/// Validate one registration and return its parsed scope list.
fn validate(registration: &OauthClientRegistration) -> Result<Vec<String>, String> {
    let id = &registration.client_id;
    if id.trim().is_empty() {
        return Err("oauth client client_id is required".to_string());
    }
    if registration.client_name.trim().is_empty() {
        return Err(format!("oauth client {id:?} requires a client_name"));
    }

    match registration.token_endpoint_auth_method.as_str() {
        "client_secret_basic" => {
            if registration
                .client_secret
                .as_deref()
                .map_or(true, |secret| secret.trim().is_empty())
            {
                return Err(format!(
                    "oauth client {id:?} uses client_secret_basic and must supply a client_secret"
                ));
            }
        }
        // A public client authenticates with PKCE alone. Accepting a secret
        // here would register a credential the token endpoint never checks -
        // an operator would believe the client was confidential.
        "none" => {
            if registration.client_secret.is_some() {
                return Err(format!(
                    "oauth client {id:?} is public (token_endpoint_auth_method = \"none\") \
                     and must not supply a client_secret"
                ));
            }
        }
        other => {
            return Err(format!(
                "oauth client {id:?} has token_endpoint_auth_method {other:?}; \
                 expected client_secret_basic or none"
            ));
        }
    }

    if registration.redirect_uris.is_empty() {
        return Err(format!("oauth client {id:?} requires at least one redirect_uri"));
    }
    if registration.redirect_uris.len() > MAX_REDIRECT_URIS {
        return Err(format!(
            "oauth client {id:?} declares more than {MAX_REDIRECT_URIS} redirect_uris"
        ));
    }
    for redirect_uri in &registration.redirect_uris {
        validate_redirect_uri(id, redirect_uri)?;
    }

    let mut scopes = Vec::with_capacity(registration.scopes.len());
    for scope in &registration.scopes {
        let parsed = Scope::parse(scope)
            .map_err(|_| format!("oauth client {id:?} declares unknown scope {scope:?}"))?;
        scopes.push(parsed.as_str().to_owned());
    }
    if scopes.is_empty() {
        return Err(format!("oauth client {id:?} requires at least one scope"));
    }
    Ok(scopes)
}

fn validate_redirect_uri(client_id: &str, redirect_uri: &str) -> Result<(), String> {
    let reject = |reason: &str| Err(format!("oauth client {client_id:?} redirect_uri {redirect_uri:?}: {reason}"));

    if redirect_uri.is_empty() || redirect_uri.trim() != redirect_uri {
        return reject("must be a non-empty URI without leading or trailing whitespace");
    }
    if redirect_uri.len() > MAX_REDIRECT_URI_LEN {
        return reject("exceeds the maximum length");
    }
    let Ok(parsed) = url::Url::parse(redirect_uri) else {
        return reject("must be an absolute URI");
    };
    if parsed.fragment().is_some() {
        return reject("must not contain a fragment");
    }
    match parsed.scheme() {
        "https" if parsed.host_str().is_some() => Ok(()),
        "http" if matches!(parsed.host_str(), Some("127.0.0.1" | "localhost")) => Ok(()),
        "http" => reject("http is allowed only for localhost or 127.0.0.1"),
        _ => reject("must use https unless it is loopback http"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public_client() -> OauthClientRegistration {
        OauthClientRegistration {
            client_id: "zeroship-console".to_string(),
            client_name: "zeroship console".to_string(),
            client_uri: None,
            logo_uri: None,
            redirect_uris: vec!["https://console.zeroship.ai/auth/callback".to_string()],
            scopes: vec!["apps:read".to_string()],
            token_endpoint_auth_method: "none".to_string(),
            client_secret: None,
            refresh_allowed: false,
        }
    }

    #[test]
    fn a_valid_public_client_validates_and_normalizes_its_scopes() {
        let scopes = validate(&public_client()).expect("valid registration");
        assert_eq!(scopes, vec!["apps:read".to_string()]);
    }

    /// The four rejections the deleted route enforced, each with the SAME
    /// otherwise-valid registration so the assertion is about the one field
    /// changed and not about the fixture being broken.
    #[test]
    fn each_registration_rule_rejects_on_its_own_field() {
        for (mutate, needle) in [
            (
                Box::new(|c: &mut OauthClientRegistration| c.scopes = vec!["nope:read".to_string()])
                    as Box<dyn Fn(&mut OauthClientRegistration)>,
                "unknown scope",
            ),
            (
                Box::new(|c: &mut OauthClientRegistration| {
                    c.redirect_uris = vec!["http://evil.example/cb".to_string()];
                }),
                "allowed only for localhost",
            ),
            (
                Box::new(|c: &mut OauthClientRegistration| {
                    c.token_endpoint_auth_method = "client_secret_post".to_string();
                }),
                "expected client_secret_basic or none",
            ),
            (
                Box::new(|c: &mut OauthClientRegistration| {
                    c.client_secret = Some("deadbeef".to_string());
                }),
                "must not supply a client_secret",
            ),
        ] {
            let mut client = public_client();
            mutate(&mut client);
            let err = validate(&client).expect_err("registration must be rejected");
            assert!(err.contains(needle), "expected {needle:?} in {err:?}");
        }
    }

    /// A confidential client with no secret is a client nothing can
    /// authenticate as. The old route generated one; config cannot, so this is
    /// the rejection that replaces that generation.
    #[test]
    fn a_confidential_client_without_a_secret_is_rejected() {
        let mut client = public_client();
        client.token_endpoint_auth_method = "client_secret_basic".to_string();
        let err = validate(&client).expect_err("secretless confidential client");
        assert!(err.contains("must supply a client_secret"), "{err}");
    }
}
