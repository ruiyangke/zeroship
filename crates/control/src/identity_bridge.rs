//! Control-plane identity bridge for provider-native platform principals.
//!
//! JIT provisioning is deliberately a device-approval write path, not a bearer
//! authz read path. The hot deploy guard only resolves existing links.

use std::error::Error as StdError;
use std::time::Duration;

use compio_postgres::Client;
use serde_json::Value;
use thiserror::Error;
use url::Url;
use uuid::Uuid;

const DEFAULT_SUPABASE_EMAIL_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
// Device-provisioned creators need apps:write for deploy auto-create and
// apps:deploy for publishing concrete releases.
const DEFAULT_CREATOR_GRANTS: [&str; 3] = ["apps:write", "apps:deploy", "apps:read"];

#[derive(Debug, Error)]
pub enum IdentityBridgeError {
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),
    #[error("database: {0}")]
    Database(String),
    #[error("supabase admin URL is invalid")]
    InvalidSupabaseUrl,
}

impl From<compio_postgres::Error> for IdentityBridgeError {
    fn from(err: compio_postgres::Error) -> Self {
        let msg = err.to_string();
        let full = match source_chain(&err) {
            Some(chain) => format!("{msg}: {chain}"),
            None => msg,
        };
        Self::Database(full)
    }
}

fn source_chain(err: &dyn StdError) -> Option<String> {
    let mut out = String::new();
    let mut cur = err.source();
    while let Some(e) = cur {
        if !out.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(&e.to_string());
        cur = e.source();
    }
    (!out.is_empty()).then_some(out)
}

/// Link a verified provider subject to a platform principal, creating one when
/// needed.
///
/// P-S2b: called from the device-approval handler once the browser holds a
/// GoTrue session.
pub async fn provision_or_link(
    pg: &mut Client,
    provider: &str,
    provider_subject: &str,
    email: Option<&str>,
    email_verified: bool,
) -> Result<Uuid, IdentityBridgeError> {
    let provider = provider.trim();
    let provider_subject = provider_subject.trim();
    if provider.is_empty() {
        return Err(IdentityBridgeError::InvalidInput("provider is empty"));
    }
    if provider_subject.is_empty() {
        return Err(IdentityBridgeError::InvalidInput(
            "provider_subject is empty",
        ));
    }

    let email = email.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    });

    let tx = pg.transaction().await?;

    if let Some(row) = tx
        .query_opt(
            "SELECT principal_id \
             FROM zeroship.identity_links \
             WHERE provider = $1 AND provider_subject = $2",
            &[&provider, &provider_subject],
        )
        .await?
    {
        let principal_id = row.get("principal_id");
        tx.commit().await?;
        return Ok(principal_id);
    }

    let target = match (email_verified, email) {
        (true, Some(email)) => {
            if let Some(row) = tx
                .query_opt(
                    "SELECT id \
                     FROM zeroship.users \
                     WHERE email = $1::citext",
                    &[&email],
                )
                .await?
            {
                ProvisionTarget {
                    principal_id: row.get("id"),
                    newly_created: false,
                }
            } else {
                create_verified_email_principal(&tx, email).await?
            }
        }
        _ => create_synthetic_email_principal(&tx).await?,
    };

    let inserted_link = tx
        .query_opt(
            "INSERT INTO zeroship.identity_links \
                (principal_id, provider, provider_subject, email) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (provider, provider_subject) DO NOTHING \
             RETURNING principal_id",
            &[&target.principal_id, &provider, &provider_subject, &email],
        )
        .await?;

    if inserted_link.is_none() {
        tx.rollback().await?;
        return select_existing_link(pg, provider, provider_subject).await;
    }

    if target.newly_created {
        for grant in DEFAULT_CREATOR_GRANTS {
            tx.execute(
                "INSERT INTO zeroship.principal_grants (principal_id, grant_name) \
                 VALUES ($1, $2) \
                 ON CONFLICT (principal_id, grant_name) DO NOTHING",
                &[&target.principal_id, &grant],
            )
            .await?;
        }
    }

    tx.commit().await?;
    Ok(target.principal_id)
}

struct ProvisionTarget {
    principal_id: Uuid,
    newly_created: bool,
}

async fn create_verified_email_principal(
    tx: &compio_postgres::Transaction<'_>,
    email: &str,
) -> Result<ProvisionTarget, IdentityBridgeError> {
    if let Some(row) = tx
        .query_opt(
            "INSERT INTO zeroship.users (email, email_verified_at, name) \
             VALUES ($1::citext, NOW(), 'Supabase Platform Principal') \
             ON CONFLICT (email) DO NOTHING \
             RETURNING id",
            &[&email],
        )
        .await?
    {
        return Ok(ProvisionTarget {
            principal_id: row.get("id"),
            newly_created: true,
        });
    }

    let row = tx
        .query_one(
            "SELECT id \
             FROM zeroship.users \
             WHERE email = $1::citext",
            &[&email],
        )
        .await?;
    Ok(ProvisionTarget {
        principal_id: row.get("id"),
        newly_created: false,
    })
}

async fn create_synthetic_email_principal(
    tx: &compio_postgres::Transaction<'_>,
) -> Result<ProvisionTarget, IdentityBridgeError> {
    let principal_id = Uuid::new_v4();
    let email = format!("identity-{principal_id}@zeroship.localhost");
    tx.execute(
        "INSERT INTO zeroship.users (id, email, name) \
         VALUES ($1, $2::citext, 'Supabase Platform Principal')",
        &[&principal_id, &email],
    )
    .await?;
    Ok(ProvisionTarget {
        principal_id,
        newly_created: true,
    })
}

async fn select_existing_link(
    pg: &Client,
    provider: &str,
    provider_subject: &str,
) -> Result<Uuid, IdentityBridgeError> {
    let row = pg
        .query_one(
            "SELECT principal_id \
             FROM zeroship.identity_links \
             WHERE provider = $1 AND provider_subject = $2",
            &[&provider, &provider_subject],
        )
        .await?;
    Ok(row.get("principal_id"))
}

/// Fetch authoritative GoTrue email confirmation state.
///
/// This returns `Ok(false)` on transport failure, timeout, non-2xx, body-read
/// failure, or malformed JSON. That is the deliberate fail-closed behavior:
/// the caller can pass this bool straight into [`provision_or_link`] and an
/// unavailable admin API can never enable email-based account merge.
pub async fn fetch_email_verified(
    supabase_url: &str,
    service_role_key: &str,
    provider_subject: &str,
) -> Result<bool, IdentityBridgeError> {
    let Ok(url) = gotrue_admin_user_url(supabase_url, provider_subject) else {
        return Ok(false);
    };
    let bearer = format!("Bearer {service_role_key}");
    let client = cyper::Client::new();
    let Ok(builder) = client.get(&url) else {
        return Ok(false);
    };
    let Ok(builder) = builder.header("authorization", &bearer) else {
        return Ok(false);
    };
    let Ok(builder) = builder.header("apikey", service_role_key) else {
        return Ok(false);
    };

    let response = match compio::time::timeout(
        DEFAULT_SUPABASE_EMAIL_LOOKUP_TIMEOUT,
        builder.send(),
    )
    .await
    {
        Ok(Ok(response)) => response,
        _ => return Ok(false),
    };

    let status = response.status().as_u16();
    let Ok(bytes) = response.bytes().await else {
        return Ok(false);
    };
    if !(200..300).contains(&status) {
        return Ok(false);
    }

    Ok(parse_gotrue_admin_email_verified(&bytes).unwrap_or(false))
}

fn gotrue_admin_user_url(
    supabase_url: &str,
    provider_subject: &str,
) -> Result<String, IdentityBridgeError> {
    let mut url = Url::parse(supabase_url).map_err(|_| IdentityBridgeError::InvalidSupabaseUrl)?;
    url.set_query(None);
    url.set_fragment(None);
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| IdentityBridgeError::InvalidSupabaseUrl)?;
        segments.pop_if_empty();
        segments.extend(["auth", "v1", "admin", "users", provider_subject]);
    }
    Ok(url.to_string())
}

#[must_use]
pub fn parse_gotrue_admin_email_verified(body: &[u8]) -> Option<bool> {
    let json: Value = serde_json::from_slice(body).ok()?;
    Some(
        json.get("email_confirmed_at")
            .is_some_and(|value| !value.is_null()),
    )
}
