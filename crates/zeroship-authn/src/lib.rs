//! Shared bearer authentication for control-like services.

pub mod platform_cli;
pub mod rate_limit;
pub mod rejection;
pub mod service_replay;

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;

use uuid::Uuid;
use zeroship_authz as authz;
use zeroship_core::auth_provider::{AuthProvider, ProviderAuthz, VerifyTokenError};
use zeroship_core::device_grant::{PLATFORM_CLI_CLIENT_ID, PLATFORM_CLI_ISSUABLE_SCOPES};
use zeroship_authz::wrapper_revocation::{family_revoked_at, revoked_after_for};

pub use rejection::AuthnRejection;

/// Every authn refusal is boxed into ntex's error container, so a handler can
/// return one directly. Build them ONLY through [`AuthnRejection`] - see that
/// module for what ntex's own helpers get wrong about `status_code()`.
pub type HttpRejection = ntex::web::Error;

#[derive(Debug)]
pub struct VerifiedPrincipal {
    pub principal_id: Uuid,
    pub token_policy: Option<authz::Policy>,
    pub mfa_verified: bool,
    pub mfa_age_seconds: Option<u32>,
    pub request_ip: Option<IpAddr>,
    pub request_id: String,
    /// This bearer is a platform CLI token for a principal with no
    /// `zeroship.identity_links` seeding marker, so it was authorized against
    /// the DEFAULT CLI grant set rather than against stored rows
    /// (see [`BearerVerifier::platform_cli_entitlement`]).
    ///
    /// A caller that can write `zeroship.principal_grants` should materialize
    /// those defaults, because until it does the operator has no rows to
    /// delete and narrowing has nothing to bite on. A caller that cannot write
    /// them ignores this: the request itself was already authorized correctly,
    /// and control will materialize on its own first sight of the principal.
    pub seed_platform_cli_grants: bool,
}

/// What a platform CLI token may actually do, resolved from the live grant
/// rows rather than from the token.
struct PlatformCliEntitlement {
    scopes: HashSet<authz::Scope>,
    /// No `zeroship.identity_links` row at all. Deliberately NOT "holds zero
    /// grants": "the operator revoked everything" is a legitimate empty
    /// entitlement and must not be re-seeded.
    unseeded: bool,
}

#[derive(Clone, Debug)]
pub struct BearerVerifier {
    control_pg: Arc<compio_postgres::Client>,
    auth_provider: Arc<AuthProvider>,
    trusted_oauth_clients: HashSet<String>,
    expected_oauth_audience: String,
}

impl BearerVerifier {
    #[must_use]
    pub fn new(
        control_pg: Arc<compio_postgres::Client>,
        auth_provider: Arc<AuthProvider>,
        trusted_oauth_clients: HashSet<String>,
        expected_oauth_audience: String,
    ) -> Self {
        Self {
            control_pg,
            auth_provider,
            trusted_oauth_clients,
            expected_oauth_audience,
        }
    }

    #[must_use]
    pub fn trusted_oauth_clients(&self) -> &HashSet<String> {
        &self.trusted_oauth_clients
    }

    pub async fn verify_bearer(
        &self,
        token: &str,
        request_ip: Option<IpAddr>,
        request_id: String,
    ) -> Result<VerifiedPrincipal, HttpRejection> {
        // The platform OP is the only issuer. A locally-signed personal access
        // token used to be tried first, ahead of this call; that was a second
        // issuance authority holding its own key, and it is gone.
        let principal = self
            .oauth_guard_from_bearer(token, request_ip, request_id)
            .await?;

        self.require_active_principal(principal.principal_id).await?;

        Ok(principal)
    }

    async fn platform_revoked_after_for(
        &self,
        client_id: &str,
        sub: &str,
    ) -> Result<Option<i64>, HttpRejection> {
        // Control already performs a user-row lookup for every bearer. Read the
        // family marker in the same request rather than caching it: deletion
        // cancellation must never revive an older platform token through a
        // stale negative or earlier-positive cache entry.
        revoked_after_for(&self.control_pg, client_id, sub)
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    client_id,
                    sub,
                    "control: token revocation lookup failed"
                );
                AuthnRejection::unauthorized("revocation_check_failed").into()
            })
    }

    /// Refuse a platform bearer whose token family has been revoked.
    ///
    /// Public so that every path accepting a platform OAuth bearer runs THIS
    /// check rather than a second copy of it. Control's device-approval handler
    /// is the other caller; it used to accept `ProviderAuthz::OAuthScope`
    /// unconditionally, which let a revoked bearer approve a fresh device grant
    /// and mint a token with a new `iat` that outran the marker.
    ///
    /// Missing `client_id` or `iat` is a REFUSAL, not a pass: without them
    /// there is nothing to compare a marker against.
    ///
    /// # Errors
    ///
    /// A 401 rejection when the family is revoked, when the claims needed to
    /// decide are absent, or when the marker lookup itself fails (fail closed).
    pub async fn reject_revoked_platform_token(
        &self,
        client_id: Option<&str>,
        sub: &str,
        iat: Option<u64>,
    ) -> Result<(), HttpRejection> {
        let (Some(client_id), Some(iat)) = (client_id, iat) else {
            return Err(AuthnRejection::unauthorized("token_revocation_claims_missing").into());
        };
        let iat = i64::try_from(iat)
            .map_err(|_| AuthnRejection::unauthorized("invalid_token_iat"))?;
        let revoked_after = self.platform_revoked_after_for(client_id, sub).await?;
        if family_revoked_at(revoked_after, iat) {
            return Err(AuthnRejection::unauthorized("token_revoked").into());
        }
        Ok(())
    }

    /// Require a principal row that is still eligible to authenticate.
    ///
    /// This is public for the non-bearer device paths. Every bearer accepted by
    /// [`Self::verify_bearer`] passes this check at one shared convergence point,
    /// so a future bearer class cannot omit owner lifecycle validation.
    ///
    /// # Errors
    ///
    /// A 401 rejection when the principal is missing, disabled, pending
    /// deletion, or anonymized.
    /// A lookup failure is a 500 rejection and never authenticates the caller.
    pub async fn require_active_principal(
        &self,
        principal_id: Uuid,
    ) -> Result<(), HttpRejection> {
        let rows = self
            .control_pg
            .query(ACTIVE_PRINCIPAL_SQL, &[&principal_id])
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    principal_id = %principal_id,
                    "control: principal eligibility lookup failed"
                );
                AuthnRejection::internal("principal_eligibility_lookup_failed")
            })?;
        if rows.is_empty() {
            return Err(AuthnRejection::unauthorized("principal_inactive").into());
        }
        Ok(())
    }

    async fn oauth_guard_from_bearer(
        &self,
        token: &str,
        request_ip: Option<IpAddr>,
        request_id: String,
    ) -> Result<VerifiedPrincipal, HttpRejection> {
        let verified = self
            .auth_provider
            .verify_token(token)
            .await
            .map_err(|err| match err {
                VerifyTokenError::InactiveToken => AuthnRejection::unauthorized("inactive_token"),
                VerifyTokenError::MissingSubject => {
                    AuthnRejection::unauthorized("missing_oauth_sub")
                }
                VerifyTokenError::MissingIssuer | VerifyTokenError::UnknownIssuer(_) => {
                    AuthnRejection::unauthorized("unknown_oauth_issuer")
                }
                VerifyTokenError::PlatformVerification(err) => {
                    tracing::warn!(error = %err, "control: platform token verify failed");
                    AuthnRejection::unauthorized("platform_token_verification_failed")
                }
            })?;
        let (principal_id, token_policy, seed_platform_cli_grants) = match &verified.provider_authz
        {
            ProviderAuthz::OAuthScope(raw_scope) => {
                if !verified.aud.as_ref().is_some_and(|audiences| {
                    audiences
                        .iter()
                        .any(|audience| audience == &self.expected_oauth_audience)
                }) {
                    return Err(AuthnRejection::unauthorized("wrong_audience").into());
                }
                self.reject_revoked_platform_token(
                    verified.client_id.as_deref(),
                    &verified.provider_subject,
                    verified.iat,
                )
                .await?;

                let principal_id = Uuid::parse_str(&verified.provider_subject)
                    .map_err(|_| AuthnRejection::unauthorized("invalid_oauth_sub"))?;
                let mut scopes = authz::parse_scope_string(raw_scope)
                    .map_err(|_| AuthnRejection::unauthorized("invalid_oauth_scope"))?;
                let mut seed = false;
                if verified.client_id.as_deref() == Some(PLATFORM_CLI_CLIENT_ID) {
                    let entitlement = self.platform_cli_entitlement(principal_id).await?;
                    seed = entitlement.unseeded;
                    scopes.retain(|scope| entitlement.scopes.contains(scope));
                }
                (principal_id, authz::scopes_to_policy(&scopes), seed)
            }
            ProviderAuthz::GoTrueRole(role) => {
                if role != "authenticated" {
                    return Err(AuthnRejection::unauthorized("unauthenticated_gotrue_role").into());
                }

                let principal_id =
                    self.resolve_supabase_principal(&verified.provider_subject).await?;
                let grants = self.load_principal_grants(principal_id).await?;
                let raw_scope = grants.join(" ");
                let token_policy = policy_from_scope_string(&raw_scope, "invalid_principal_grant")?;
                (principal_id, token_policy, false)
            }
        };

        Ok(VerifiedPrincipal {
            principal_id,
            token_policy: Some(token_policy),
            mfa_verified: false,
            mfa_age_seconds: None,
            request_ip,
            request_id,
            seed_platform_cli_grants,
        })
    }

    async fn resolve_supabase_principal(
        &self,
        provider_subject: &str,
    ) -> Result<Uuid, HttpRejection> {
        let rows = self
            .control_pg
            .query(
                "SELECT principal_id \
                 FROM zeroship.identity_links \
                 WHERE provider = 'supabase' AND provider_subject = $1",
                &[&provider_subject],
            )
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    "control: supabase identity link lookup failed"
                );
                AuthnRejection::internal("identity_link_lookup_failed")
            })?;
        let row = rows
            .first()
            .ok_or_else(|| AuthnRejection::unauthorized("unlinked_supabase_principal"))?;
        Ok(row.get("principal_id"))
    }

    /// Resolve the live entitlement a platform CLI token is capped to.
    ///
    /// The OP caps a device-grant token to the client REGISTRATION and cannot
    /// do more: `db/migrations-ts/20260702000900_grants.ts:55` gives
    /// `zeroship_auth` SELECT on `zeroship.principal_grants` and no write
    /// anywhere near it. So the token's `scope` claim is a coarse ceiling, and
    /// this is where an operator's narrowing takes effect - per request, which
    /// means a DELETE reaches the token already in the creator's hand and not
    /// merely the next login.
    ///
    /// This is a pure read and needs no privilege beyond what
    /// `zeroship_control` already holds on both tables (`grants.ts:65`).
    ///
    /// An UNSEEDED principal falls back to the default CLI set rather than to
    /// nothing. `zeroship login` is an OP-only conversation, so control's
    /// first sight of a platform-native creator IS this request; intersecting
    /// with the empty table would 403 every creator's first command. The
    /// fallback is not a second source of truth - it is exactly the set
    /// [`platform_cli::materialize_default_grants`] writes, which
    /// the caller triggers on [`PlatformCliEntitlement::unseeded`].
    async fn platform_cli_entitlement(
        &self,
        principal_id: Uuid,
    ) -> Result<PlatformCliEntitlement, HttpRejection> {
        let row = self
            .control_pg
            .query_one(
                "SELECT EXISTS ( \
                     SELECT 1 FROM zeroship.identity_links WHERE principal_id = $1 \
                 ) AS seeded, \
                 ( \
                     SELECT string_agg(grant_name, ' ' ORDER BY grant_name) \
                     FROM zeroship.principal_grants WHERE principal_id = $1 \
                 ) AS granted",
                &[&principal_id],
            )
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    "control: platform CLI entitlement lookup failed"
                );
                AuthnRejection::internal("principal_grant_lookup_failed")
            })?;

        if !row.get::<_, bool>("seeded") {
            return Ok(PlatformCliEntitlement {
                scopes: PLATFORM_CLI_ISSUABLE_SCOPES
                    .iter()
                    .filter_map(|grant| authz::Scope::parse(grant).ok())
                    .collect(),
                unseeded: true,
            });
        }

        // An unparseable grant name grants nothing rather than rejecting the
        // request. The rows are operator-written free text with no CHECK
        // constraint, so a typo must cost the creator that one scope, not
        // every scope they legitimately hold.
        let scopes = row
            .get::<_, Option<String>>("granted")
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|grant| match authz::Scope::parse(grant) {
                Ok(scope) => Some(scope),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        principal_id = %principal_id,
                        "control: ignoring unknown principal grant"
                    );
                    None
                }
            })
            .collect();
        Ok(PlatformCliEntitlement {
            scopes,
            unseeded: false,
        })
    }

    async fn load_principal_grants(&self, principal_id: Uuid) -> Result<Vec<String>, HttpRejection> {
        let rows = self
            .control_pg
            .query(
                "SELECT grant_name \
                 FROM zeroship.principal_grants \
                 WHERE principal_id = $1 \
                 ORDER BY grant_name",
                &[&principal_id],
            )
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    "control: principal grant lookup failed"
                );
                AuthnRejection::internal("principal_grant_lookup_failed")
            })?;
        Ok(rows
            .iter()
            .map(|row| row.get::<_, String>("grant_name"))
            .collect())
    }
}

fn policy_from_scope_string(
    raw_scope: &str,
    code: &'static str,
) -> Result<authz::Policy, HttpRejection> {
    let scopes =
        authz::parse_scope_string(raw_scope).map_err(|_| AuthnRejection::unauthorized(code))?;
    Ok(authz::scopes_to_policy(&scopes))
}

const ACTIVE_PRINCIPAL_SQL: &str =
    "SELECT 1 FROM zeroship.users \
     WHERE id = $1 \
       AND disabled_at IS NULL \
       AND deletion_requested_at IS NULL \
       AND deletion_scheduled_for IS NULL \
       AND anonymized_at IS NULL";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_principal_query_blocks_every_hard_lifecycle_state() {
        for column in [
            "disabled_at",
            "anonymized_at",
            "deletion_requested_at",
            "deletion_scheduled_for",
        ] {
            assert!(
                ACTIVE_PRINCIPAL_SQL.contains(&format!("{column} IS NULL")),
                "missing active lifecycle predicate for {column}"
            );
        }
        assert!(!ACTIVE_PRINCIPAL_SQL.contains("locked_until"));
    }
}
