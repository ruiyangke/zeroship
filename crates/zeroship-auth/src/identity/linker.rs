//! Account-linking after a federation callback.
//!
//! Decision tree (proposal §8.2 — defends against the
//! `failedstartup.com` domain-re-registration attack):
//!
//!   1. If `(provider, subject)` is already linked → reuse that user
//!      (`Existing`).
//!   2. Else if a local user exists with the upstream email:
//!      - if that user has a local credential (`password_hash IS NOT NULL`)
//!        we MUST NOT auto-link — return `NeedsConfirmation` with a signed
//!        token. The federation handler 302s the browser to
//!        `/link?token=…` where the user re-enters their zeroship password
//!        before the link is actually created. This is the
//!        domain-re-registration defence: someone who acquires a previously
//!        owned email at a federation provider cannot quietly take over a
//!        zeroship account that still has its original password.
//!      - if the user is OAuth-only (no local credential) but the provider
//!        is not trusted for this email, require confirmation too. A raw
//!        `email_verified` bit from the upstream provider is not enough to
//!        bind this local account to the provider identity.
//!      - if the user is OAuth-only and the provider is trusted for this
//!        email, auto-link silently and return `Existing`.
//!   3. Else if the provider is trusted for this email → create
//!      a fresh `zeroship.users` row and link the identity (`Created`).
//!   4. Otherwise refuse — we won't auto-create an account on an untrusted
//!      provider email, even when the provider says it verified the address.
//!
//! `provider_trusted_for_email` is set by the per-provider HTTP handler.
//! For Google: `email_verified == true` AND (consumer `@gmail.com` OR
//! Workspace `hd` claim present). For GitHub: the email-picker only ever
//! returns a primary + verified address, so the flag is always true.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::Client;
use serde::{Deserialize, Serialize};
use zeroship_core::auth::hmac_sha256;
use zeroship_core::UserId;

use crate::error::{AuthError, Result};
use crate::identity::email as email_validation;
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
    /// Identity already linked to this user, or auto-linked to an
    /// OAuth-only local user. The federation callback continues to
    /// its completion arm.
    Existing { user_id: UserId },
    /// Brand-new account created and the identity linked to it. The
    /// federation callback continues to its completion arm.
    Created { user_id: UserId },
    /// Email collision with a locally-credentialed account. The federation
    /// callback must 302 the browser to `/link?token=<pending_token>` so the
    /// user can confirm with their existing zeroship password. The pending
    /// token carries everything `/link` POST needs (user id, provider, subject,
    /// email, native continuation target) — signed with the stash key and
    /// 10-minute TTL.
    NeedsConfirmation {
        pending_token: String,
        existing_email: String,
        provider: String,
    },
}

/// Continuation target to carry through a pending account-link confirmation.
#[derive(Debug, Clone, Copy)]
pub enum LinkResume<'a> {
    ReturnTo(&'a str),
}

/// TTL for a `PendingLink` token, in seconds.
///
/// Matches [`crate::ui::oauth_stash::STASH_MAX_AGE_SECS`] — the user has
/// the same 10-minute window to confirm the link as they had to complete
/// the federation dance.
pub const PENDING_LINK_TTL_SECS: u64 = 600;

/// Stash-equivalent payload carried between the federation callback and
/// `/link`. Signed + base64url-encoded; opaque to the browser.
///
/// Reuses [`crate::config::AuthSettings::stash_signing_key`] for the HMAC
/// key. The wire format is shape-distinct from
/// [`crate::ui::oauth_stash::OAuthStash`] (different field set, includes
/// `exp_unix`) so the two are not confusable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingLink {
    pub user_id: UserId,
    pub provider: String,
    pub subject: String,
    pub email: String,
    pub return_to: Option<String>,
    /// Unix-seconds expiry. Signed AS PART OF the payload, so a server
    /// without clock-skew can reject expired tokens without keeping state.
    pub exp_unix: i64,
}

impl PendingLink {
    /// Encode + HMAC-sign with `key`. Returns `base64url(json).base64url(hmac)`.
    ///
    /// The JSON is the canonical wire payload; we sign its base64url form
    /// so the verifier can recompute the MAC without re-serialising
    /// (avoids any field-ordering ambiguity). Same shape as
    /// [`crate::ui::oauth_stash::OAuthStash::encode`].
    ///
    /// # Panics
    ///
    /// Panics if `serde_json` fails to serialise this struct — only
    /// possible if a non-string field is ever added that contains a
    /// non-UTF-8 byte sequence. Today every field is `String` / `UserId` /
    /// `i64`.
    #[must_use]
    pub fn encode(&self, key: &[u8]) -> String {
        let json = serde_json::to_vec(self).expect("pending link serialize");
        let b64 = URL_SAFE_NO_PAD.encode(&json);
        let mac = hmac_sha256(key, b64.as_bytes());
        let mac_b64 = URL_SAFE_NO_PAD.encode(mac);
        format!("{b64}.{mac_b64}")
    }

    /// Decode + verify the HMAC, returning `Some(pending)` on a valid
    /// token (matching MAC + parseable JSON + not expired), `None`
    /// otherwise. Uses a constant-time MAC comparison.
    #[must_use]
    pub fn decode(value: &str, key: &[u8]) -> Option<Self> {
        let (b64, mac_b64) = value.split_once('.')?;
        let expected_mac = hmac_sha256(key, b64.as_bytes());
        let provided_mac = URL_SAFE_NO_PAD.decode(mac_b64).ok()?;
        if expected_mac.len() != provided_mac.len() {
            return None;
        }
        // Constant-time compare.
        let mut diff = 0u8;
        for (a, b) in expected_mac.iter().zip(provided_mac.iter()) {
            diff |= a ^ b;
        }
        if diff != 0 {
            return None;
        }
        let json = URL_SAFE_NO_PAD.decode(b64).ok()?;
        let pl: Self = serde_json::from_slice(&json).ok()?;
        if !has_return_to(pl.return_to.as_deref()) {
            return None;
        }
        // Expiry check — clock-skew-free because we only check against the
        // local wall-clock.
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        // i64 fits any plausible exp_unix; cast is safe for the near future.
        if pl.exp_unix < i64::try_from(now).unwrap_or(i64::MAX) {
            return None;
        }
        Some(pl)
    }
}

fn has_return_to(return_to: Option<&str>) -> bool {
    return_to.is_some_and(|value| !value.is_empty())
}

/// Resolve the local user for a federation callback.
///
/// See module-level docs for the decision tree.
///
/// `pending_signing_key` is the HMAC key used to sign the
/// `NeedsConfirmation` token. In production this is
/// `cfg.settings.stash_signing_key.expose_str().as_bytes()` (shared with the
/// OAuth stash cookie).
///
/// # Errors
///
/// Returns database errors from the user and identity repositories;
/// [`AuthError::Internal`] when the provider is not trusted for the email
/// and we refuse to auto-create.
#[allow(clippy::future_not_send, reason = "the ORM belongs to its compio runtime")]
pub async fn resolve_or_link(
    db: &Client,
    orm: &zeroship_data_orm::Database,
    profile: &ResolvedProfile<'_>,
    resume: LinkResume<'_>,
    pending_signing_key: &[u8],
) -> Result<LinkOutcome> {
    email_validation::validate_email(profile.email)
        .map_err(|_| AuthError::Internal("invalid email".into()))?;

    // 1. Already linked?
    if let Some(id) =
        identities::find_by_provider_subject(db, profile.provider, profile.subject).await?
    {
        return Ok(LinkOutcome::Existing {
            user_id: id.user_id,
        });
    }

    // 2. Email collision with an existing local user.
    if let Some(user) = users::find_by_email(orm, profile.email).await? {
        // 2a. Local credential present, or provider not trusted for this
        // email → require explicit confirmation before creating the link.
        if user.password_hash.is_some() || !profile.provider_trusted_for_email {
            return pending_confirmation(user.id.clone(), profile, resume, pending_signing_key);
        }

        // 2b. OAuth-only existing user + provider trusted for this email:
        // auto-link is safe.
        identities::link(
            db,
            &user.id,
            profile.provider,
            profile.subject,
            Some(profile.email),
            profile.raw_profile.as_ref(),
        )
        .await?;
        return Ok(LinkOutcome::Existing { user_id: user.id });
    }

    // 3. Brand-new user — trusted-provider gate. Do not substitute the
    // upstream provider's raw `email_verified` bit for this policy.
    if !profile.provider_trusted_for_email {
        return Err(AuthError::Internal(format!(
            "refusing to auto-create account for untrusted provider email: {}",
            profile.email
        )));
    }

    // Pick a default display name. The `name` claim is best; fall back to
    // the local-part of the email. Empty-local-part edge case (an email of
    // the form `@example.com`) falls back to `"user"` so we always insert a
    // non-empty value.
    let fallback = profile.email.split('@').next().unwrap_or("user");
    let name = profile.name.unwrap_or(if fallback.is_empty() {
        "user"
    } else {
        fallback
    });

    let user = users::create(orm, profile.email, name, None).await?;

    // Mark email_verified_at only when this provider is trusted for this
    // email. Raw provider `email_verified` is not enough.
    if profile.provider_trusted_for_email {
        db.execute(
            "UPDATE zeroship.users SET email_verified_at = NOW() WHERE id = $1",
            &[&user.id.as_str()],
        )
        .await
        .map_err(|e| AuthError::Db(format!("set email_verified_at: {e}")))?;
    }

    // Carry the avatar through on first contact. Subsequent logins won't
    // overwrite a user-set avatar (we deliberately don't UPDATE here later).
    if let Some(avatar) = profile.avatar_url {
        db.execute(
            "UPDATE zeroship.users SET avatar_url = $1 WHERE id = $2 AND avatar_url IS NULL",
            &[&avatar, &user.id.as_str()],
        )
        .await
        .map_err(|e| AuthError::Db(format!("set avatar_url: {e}")))?;
    }

    identities::link(
        db,
        &user.id,
        profile.provider,
        profile.subject,
        Some(profile.email),
        profile.raw_profile.as_ref(),
    )
    .await?;

    Ok(LinkOutcome::Created { user_id: user.id })
}

fn pending_confirmation(
    user_id: UserId,
    profile: &ResolvedProfile<'_>,
    resume: LinkResume<'_>,
    pending_signing_key: &[u8],
) -> Result<LinkOutcome> {
    let exp_unix = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| AuthError::Internal(format!("clock: {e}")))?
            .as_secs()
            + PENDING_LINK_TTL_SECS,
    )
    .unwrap_or(i64::MAX);
    let pending = PendingLink {
        user_id: user_id.clone(),
        provider: profile.provider.to_string(),
        subject: profile.subject.to_string(),
        email: profile.email.to_string(),
        return_to: match resume {
            LinkResume::ReturnTo(value) => Some(value.to_string()),
        },
        exp_unix,
    };
    Ok(LinkOutcome::NeedsConfirmation {
        pending_token: pending.encode(pending_signing_key),
        existing_email: profile.email.to_string(),
        provider: profile.provider.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pending() -> PendingLink {
        PendingLink {
            user_id: UserId::mint(),
            provider: "google".into(),
            subject: "sub-abc".into(),
            email: "ada@example.com".into(),
            return_to: Some(
                "/oauth2/authorize?client_id=oac_123&redirect_uri=https%3A%2F%2Fapp.test%2Fcb"
                    .into(),
            ),
            exp_unix: i64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 600,
            )
            .unwrap(),
        }
    }

    #[test]
    fn pending_link_roundtrip() {
        let key = b"k".repeat(32);
        let pl = sample_pending();
        let encoded = pl.encode(&key);
        assert!(encoded.contains('.'));
        let decoded = PendingLink::decode(&encoded, &key).expect("decode");
        assert_eq!(decoded, pl);
    }

    #[test]
    fn pending_link_rejects_without_return_to() {
        let key = b"k".repeat(32);
        let mut neither = sample_pending();
        neither.return_to = None;
        assert!(PendingLink::decode(&neither.encode(&key), &key).is_none());
    }

    #[test]
    fn pending_link_rejects_tampering() {
        let key = b"k".repeat(32);
        let pl = sample_pending();
        let mut tampered = pl.encode(&key);
        // Flip the last byte to corrupt either the MAC or the payload —
        // both must be rejected.
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        assert!(PendingLink::decode(&tampered, &key).is_none());
    }

    #[test]
    fn pending_link_rejects_expired() {
        let key = b"k".repeat(32);
        let mut pl = sample_pending();
        // 1 second in the past — well past the immediate-now boundary so
        // even slow CI clocks see it as expired.
        pl.exp_unix = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap()
            - 1;
        let encoded = pl.encode(&key);
        assert!(PendingLink::decode(&encoded, &key).is_none());
    }

    #[test]
    fn pending_link_rejects_wrong_key() {
        let pl = sample_pending();
        let encoded = pl.encode(b"key-one-32-bytes-padded-padded-X");
        assert!(PendingLink::decode(&encoded, b"key-two-32-bytes-padded-padded-X").is_none());
    }

}

#[cfg(test)]
mod resolution_tests;
