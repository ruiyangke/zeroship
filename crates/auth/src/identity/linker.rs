//! Account-linking after a federation callback.
//!
//! Phase 4 U2 ships the basic find-or-create path:
//!
//!   1. If `(provider, subject)` is already linked → reuse that user.
//!   2. Else if a local user exists with the upstream email → auto-link
//!      (TEMPORARY — U4 replaces this with the `NeedsConfirmation` flow that
//!      requires password-confirm for accounts that have a local credential).
//!   3. Else if the provider is trusted for this email (verified) → create
//!      a fresh `auth.users` row and link the identity.
//!   4. Otherwise refuse — we won't auto-create an account on an unverified
//!      provider email.
//!
//! `provider_trusted_for_email` is set by the per-provider HTTP handler.
//! For Google: `email_verified == true` AND (consumer `@gmail.com` OR
//! Workspace `hd` claim present).

use compio_postgres::Client;
use uuid::Uuid;

use crate::error::{AuthError, Result};
use crate::store::{identities, users};

/// Per-callback profile distilled from the upstream `IdP`.
///
/// The HTTP layer builds this from the provider-specific identity struct
/// (e.g. [`crate::identity::oauth::google::GoogleIdentity`]) before calling
/// [`resolve_or_link`].
#[derive(Debug)]
pub struct ResolvedProfile<'a> {
    pub provider: &'a str,
    pub subject: &'a str,
    pub email: &'a str,
    pub email_verified: bool,
    pub name: Option<&'a str>,
    pub avatar_url: Option<&'a str>,
    /// Whether this provider's claim that the email is verified is good
    /// enough to auto-create a new local account. Caller decides per-IdP
    /// (Google: gmail.com or Workspace-hd; GitHub U3: only when the user's
    /// primary verified email is returned).
    pub provider_trusted_for_email: bool,
    pub raw_profile: Option<serde_json::Value>,
}

/// Outcome of the linking step.
#[derive(Debug)]
pub enum LinkOutcome {
    /// Identity already linked, or auto-linked to a pre-existing user.
    Existing { user_id: Uuid },
    /// Brand-new account created and the identity linked to it.
    Created { user_id: Uuid },
    // U4 adds: `NeedsConfirmation { pending_token: String, user_id_at_collision: Uuid }`.
}

/// Resolve the local user for a federation callback.
///
/// See module-level docs for the decision tree.
///
/// # Errors
///
/// [`AuthError::Db`] on PG failure;
/// [`AuthError::Internal`] when the provider's email is unverified and we
/// refuse to auto-create.
pub async fn resolve_or_link(
    db: &Client,
    profile: &ResolvedProfile<'_>,
) -> Result<LinkOutcome> {
    // 1. Already linked?
    if let Some(id) =
        identities::find_by_provider_subject(db, profile.provider, profile.subject).await?
    {
        return Ok(LinkOutcome::Existing { user_id: id.user_id });
    }

    // 2. Email collision with an existing local user.
    //
    // PHASE 4 U2 — naive auto-link. We assume the provider's verified email
    // claim is trustworthy enough to attach the identity to the existing
    // user. U4 replaces this branch with a NeedsConfirmation flow:
    //   - if the existing user has a `password_hash`, send the user through
    //     a "confirm your password to link Google" step before linking;
    //   - if the existing user is OAuth-only, link silently (current path).
    if let Some(user) = users::find_by_email(db, profile.email).await? {
        identities::link(
            db,
            user.id,
            profile.provider,
            profile.subject,
            Some(profile.email),
            profile.raw_profile.as_ref(),
        )
        .await?;
        return Ok(LinkOutcome::Existing { user_id: user.id });
    }

    // 3. Brand-new user — verified-email gate.
    if !profile.provider_trusted_for_email && !profile.email_verified {
        return Err(AuthError::Internal(format!(
            "refusing to auto-create account for unverified provider email: {}",
            profile.email
        )));
    }

    // Pick a default display name. The `name` claim is best; fall back to
    // the local-part of the email. Empty-local-part edge case (an email of
    // the form `@example.com`) falls back to `"user"` so we always insert a
    // non-empty value.
    let fallback = profile.email.split('@').next().unwrap_or("user");
    let name = profile.name.unwrap_or(if fallback.is_empty() { "user" } else { fallback });

    let user = users::create(db, profile.email, name, None).await?;

    // Mark email_verified_at since the provider claims verification.
    // Best-effort — if this UPDATE fails we already have the user row, so
    // surface the DB error to the caller (consistent with the rest of
    // store::*).
    if profile.email_verified {
        db.execute(
            "UPDATE auth.users SET email_verified_at = NOW() WHERE id = $1",
            &[&user.id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("set email_verified_at: {e}")))?;
    }

    // Carry the avatar through on first contact. Subsequent logins won't
    // overwrite a user-set avatar (we deliberately don't UPDATE here later).
    if let Some(avatar) = profile.avatar_url {
        db.execute(
            "UPDATE auth.users SET avatar_url = $1 WHERE id = $2 AND avatar_url IS NULL",
            &[&avatar, &user.id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("set avatar_url: {e}")))?;
    }

    identities::link(
        db,
        user.id,
        profile.provider,
        profile.subject,
        Some(profile.email),
        profile.raw_profile.as_ref(),
    )
    .await?;

    Ok(LinkOutcome::Created { user_id: user.id })
}
