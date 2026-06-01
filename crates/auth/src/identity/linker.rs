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
use uuid::Uuid;
use zeroship_core::auth::hmac_sha256;

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
    /// Identity already linked to this user, or auto-linked to an
    /// OAuth-only local user. The federation callback continues to
    /// `accept_login`.
    Existing { user_id: Uuid },
    /// Brand-new account created and the identity linked to it. The
    /// federation callback continues to `accept_login`.
    Created { user_id: Uuid },
    /// Email collision with a locally-credentialed account. The federation
    /// callback MUST NOT call `accept_login`; it should 302 the browser to
    /// `/link?token=<pending_token>` so the user can confirm with their
    /// existing zeroship password. The pending token carries everything
    /// `/link` POST needs (user id, provider, subject, email, the original
    /// `login_challenge`) — signed with the stash key and 10-minute TTL.
    NeedsConfirmation {
        pending_token: String,
        existing_email: String,
        provider: String,
    },
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
/// Reuses [`crate::config::AuthConfig::stash_signing_key`] for the HMAC
/// key. The wire format is shape-distinct from
/// [`crate::ui::oauth_stash::OAuthStash`] (different field set, includes
/// `exp_unix`) so the two are not confusable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingLink {
    pub user_id: Uuid,
    pub provider: String,
    pub subject: String,
    pub email: String,
    pub login_challenge: String,
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
    /// non-UTF-8 byte sequence. Today every field is `String` / `Uuid` /
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

/// Resolve the local user for a federation callback.
///
/// See module-level docs for the decision tree.
///
/// `pending_signing_key` is the HMAC key used to sign the
/// `NeedsConfirmation` token. In production this is
/// `cfg.stash_signing_key.as_bytes()` (shared with the OAuth stash cookie).
///
/// # Errors
///
/// [`AuthError::Db`] on PG failure;
/// [`AuthError::Internal`] when the provider is not trusted for the email
/// and we refuse to auto-create.
pub async fn resolve_or_link(
    db: &Client,
    profile: &ResolvedProfile<'_>,
    login_challenge: &str,
    pending_signing_key: &[u8],
) -> Result<LinkOutcome> {
    email_validation::validate_email(profile.email)
        .map_err(|_| AuthError::Internal("invalid email".into()))?;

    // 1. Already linked?
    if let Some(id) =
        identities::find_by_provider_subject(db, profile.provider, profile.subject).await?
    {
        return Ok(LinkOutcome::Existing { user_id: id.user_id });
    }

    // 2. Email collision with an existing local user.
    if let Some(user) = users::find_by_email(db, profile.email).await? {
        // 2a. Local credential present, or provider not trusted for this
        // email → require explicit confirmation before creating the link.
        if user.password_hash.is_some() || !profile.provider_trusted_for_email {
            return pending_confirmation(
                user.id,
                profile,
                login_challenge,
                pending_signing_key,
            );
        }

        // 2b. OAuth-only existing user + provider trusted for this email:
        // auto-link is safe.
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
    let name = profile.name.unwrap_or(if fallback.is_empty() { "user" } else { fallback });

    let user = users::create(db, profile.email, name, None).await?;

    // Mark email_verified_at only when this provider is trusted for this
    // email. Raw provider `email_verified` is not enough.
    if profile.provider_trusted_for_email {
        db.execute(
            "UPDATE zeroship.users SET email_verified_at = NOW() WHERE id = $1",
            &[&user.id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("set email_verified_at: {e}")))?;
    }

    // Carry the avatar through on first contact. Subsequent logins won't
    // overwrite a user-set avatar (we deliberately don't UPDATE here later).
    if let Some(avatar) = profile.avatar_url {
        db.execute(
            "UPDATE zeroship.users SET avatar_url = $1 WHERE id = $2 AND avatar_url IS NULL",
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

fn pending_confirmation(
    user_id: Uuid,
    profile: &ResolvedProfile<'_>,
    login_challenge: &str,
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
        user_id,
        provider: profile.provider.to_string(),
        subject: profile.subject.to_string(),
        email: profile.email.to_string(),
        login_challenge: login_challenge.to_string(),
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
            user_id: Uuid::new_v4(),
            provider: "google".into(),
            subject: "sub-abc".into(),
            email: "ada@example.com".into(),
            login_challenge: "lc-xyz".into(),
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

    // ─── Live-PG tests for the decision tree ─────────────────────────────
    //
    // Skipped unless `AUTH_DB_URL` is set. Each test seeds a fresh user
    // (random email) and cleans up at the end.

    // `compio_postgres::Client` is `!Send` (Rc-backed). The future
    // therefore inherits the not-send trait — structural, not a defect.
    #[allow(dead_code, clippy::future_not_send)]
    async fn pg() -> Option<compio_postgres::Client> {
        let dsn = std::env::var("AUTH_DB_URL").ok()?;
        let (client, connection) = compio_postgres::connect(&dsn, compio_postgres::NoTls)
            .await
            .expect("connect");
        compio::runtime::spawn(async move {
            if let Err(e) = connection.run().await {
                eprintln!("linker test pg connection error: {e}");
            }
        })
        .detach();
        Some(client)
    }

    #[compio::test]
    async fn needs_confirmation_when_local_user_has_password_hash() {
        let Some(client) = pg().await else {
            eprintln!("skipping linker live-PG test (no AUTH_DB_URL)");
            return;
        };

        // Seed a fully-credentialed user (non-NULL password_hash).
        let email = format!("linker-pwd-{}@example.test", Uuid::new_v4().simple());
        let phc = crate::identity::password::hash("hunter2").expect("hash");
        let row = client
            .query_one(
                "INSERT INTO zeroship.users (email, name, password_hash) \
                 VALUES ($1::citext, $2, $3) RETURNING id",
                &[&email, &"Ada", &phc],
            )
            .await
            .expect("seed user");
        let user_id: Uuid = row.get("id");

        let profile = ResolvedProfile {
            provider: "google",
            subject: &format!("sub-{}", Uuid::new_v4().simple()),
            email: &email,
            email_verified: true,
            name: Some("Ada"),
            avatar_url: None,
            provider_trusted_for_email: true,
            raw_profile: None,
        };

        let key = b"k".repeat(32);
        let outcome = resolve_or_link(&client, &profile, "lc-test", &key)
            .await
            .expect("resolve_or_link");

        match outcome {
            LinkOutcome::NeedsConfirmation {
                pending_token,
                existing_email,
                provider,
            } => {
                assert_eq!(existing_email, email);
                assert_eq!(provider, "google");
                let decoded = PendingLink::decode(&pending_token, &key)
                    .expect("decode pending token");
                assert_eq!(decoded.user_id, user_id);
                assert_eq!(decoded.email, email);
                assert_eq!(decoded.provider, "google");
                assert_eq!(decoded.login_challenge, "lc-test");
            }
            other => panic!("expected NeedsConfirmation, got {other:?}"),
        }

        // No identity row should exist yet — the link is only created on
        // `/link` POST.
        let listed = identities::list_for_user(&client, user_id).await.expect("list");
        assert!(
            listed.is_empty(),
            "NeedsConfirmation must NOT create an identity row"
        );

        // Cleanup.
        client
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
            .await
            .ok();
    }

    #[compio::test]
    async fn auto_links_when_local_user_is_oauth_only() {
        let Some(client) = pg().await else {
            eprintln!("skipping linker live-PG test (no AUTH_DB_URL)");
            return;
        };

        // Seed an OAuth-only user (NULL password_hash) — nothing to defend.
        let email = format!("linker-oauth-{}@example.test", Uuid::new_v4().simple());
        let row = client
            .query_one(
                "INSERT INTO zeroship.users (email, name, password_hash) \
                 VALUES ($1::citext, $2, NULL) RETURNING id",
                &[&email, &"Linus"],
            )
            .await
            .expect("seed user");
        let user_id: Uuid = row.get("id");

        let subject = format!("sub-{}", Uuid::new_v4().simple());
        let profile = ResolvedProfile {
            provider: "github",
            subject: &subject,
            email: &email,
            email_verified: true,
            name: Some("Linus"),
            avatar_url: None,
            provider_trusted_for_email: true,
            raw_profile: None,
        };

        let key = b"k".repeat(32);
        let outcome = resolve_or_link(&client, &profile, "lc-test", &key)
            .await
            .expect("resolve_or_link");

        match outcome {
            LinkOutcome::Existing { user_id: got } => assert_eq!(got, user_id),
            other => panic!("expected Existing for OAuth-only user, got {other:?}"),
        }

        // The identity row MUST have been created.
        let found = identities::find_by_provider_subject(&client, "github", &subject)
            .await
            .expect("find link");
        let found = found.expect("identity should exist");
        assert_eq!(found.user_id, user_id);

        // Cleanup — FK cascade on zeroship.federated_identities.user_id catches the link.
        client
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
            .await
            .ok();
    }

    #[compio::test]
    async fn needs_confirmation_for_oauth_only_user_when_provider_untrusted_for_email() {
        let Some(client) = pg().await else {
            eprintln!("skipping linker live-PG test (no AUTH_DB_URL)");
            return;
        };

        let email = format!("linker-untrusted-{}@example.test", Uuid::new_v4().simple());
        let row = client
            .query_one(
                "INSERT INTO zeroship.users (email, name, password_hash) \
                 VALUES ($1::citext, $2, NULL) RETURNING id",
                &[&email, &"Untrusted"],
            )
            .await
            .expect("seed user");
        let user_id: Uuid = row.get("id");

        let subject = format!("sub-{}", Uuid::new_v4().simple());
        let profile = ResolvedProfile {
            provider: "google",
            subject: &subject,
            email: &email,
            email_verified: true,
            name: Some("Untrusted"),
            avatar_url: None,
            provider_trusted_for_email: false,
            raw_profile: None,
        };

        let key = b"k".repeat(32);
        let outcome = resolve_or_link(&client, &profile, "lc-test", &key)
            .await
            .expect("resolve_or_link");

        match outcome {
            LinkOutcome::NeedsConfirmation {
                pending_token,
                existing_email,
                provider,
            } => {
                assert_eq!(existing_email, email);
                assert_eq!(provider, "google");
                let decoded = PendingLink::decode(&pending_token, &key)
                    .expect("decode pending token");
                assert_eq!(decoded.user_id, user_id);
                assert_eq!(decoded.subject, subject);
                assert_eq!(decoded.email, email);
            }
            other => panic!("expected NeedsConfirmation, got {other:?}"),
        }

        let found = identities::find_by_provider_subject(&client, "google", &subject)
            .await
            .expect("find link");
        assert!(
            found.is_none(),
            "untrusted provider must not auto-link an OAuth-only account"
        );

        client
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
            .await
            .ok();
    }

    #[compio::test]
    async fn rejects_new_user_when_provider_untrusted_for_email_even_if_email_verified() {
        let Some(client) = pg().await else {
            eprintln!("skipping linker live-PG test (no AUTH_DB_URL)");
            return;
        };

        let email = format!("linker-new-untrusted-{}@example.test", Uuid::new_v4().simple());
        let subject = format!("sub-{}", Uuid::new_v4().simple());
        let profile = ResolvedProfile {
            provider: "google",
            subject: &subject,
            email: &email,
            email_verified: true,
            name: Some("Untrusted New"),
            avatar_url: None,
            provider_trusted_for_email: false,
            raw_profile: None,
        };

        let err = resolve_or_link(&client, &profile, "lc-test", b"k")
            .await
            .expect_err("untrusted provider email must not auto-create");
        assert!(
            err.to_string().contains("untrusted provider email"),
            "unexpected error: {err}"
        );

        let user_rows = client
            .query(
                "SELECT id FROM zeroship.users WHERE email = $1::citext",
                &[&email],
            )
            .await
            .expect("user select");
        assert!(user_rows.is_empty(), "must not create user row");

        let identity_rows = client
            .query(
                "SELECT id FROM zeroship.federated_identities WHERE provider = $1 AND subject = $2",
                &[&"google", &subject.as_str()],
            )
            .await
            .expect("identity select");
        assert!(identity_rows.is_empty(), "must not create identity row");
    }
}
