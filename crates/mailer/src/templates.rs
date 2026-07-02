//! Compile-time-checked email templates.
//!
//! Each template pairs one Rust struct with one `.html` file and one `.txt`
//! file. Render with `Template::render()` from askama; the result goes into
//! `mailer::Email::{html, text}`.
//!
//! Keep templates minimal — inline CSS only, no images, no external assets.
//! Email-client rendering is wildly inconsistent; minimal is safe.

#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use askama::Template;

use crate::{Address, Email};

// ─── verify-email ────────────────────────────────────────────

#[derive(Template, Debug)]
#[template(path = "verify_email.html")]
pub struct VerifyEmailHtml<'a> {
    pub name: &'a str,
    pub link: &'a str,
    pub expires_in: &'a str, // e.g. "24 hours"
}

#[derive(Template, Debug)]
#[template(path = "verify_email.txt")]
pub struct VerifyEmailText<'a> {
    pub name: &'a str,
    pub link: &'a str,
    pub expires_in: &'a str,
}

// ─── magic-link ──────────────────────────────────────────────

#[derive(Template, Debug)]
#[template(path = "magic_link.html")]
pub struct MagicLinkHtml<'a> {
    pub name: &'a str,
    pub link: &'a str,
    pub expires_in: &'a str,          // "15 minutes"
    pub requesting_device: &'a str,   // "Chrome on macOS"
    pub requesting_location: &'a str, // "San Francisco, CA" (or "Unknown")
}

#[derive(Template, Debug)]
#[template(path = "magic_link.txt")]
pub struct MagicLinkText<'a> {
    pub name: &'a str,
    pub link: &'a str,
    pub expires_in: &'a str,
    pub requesting_device: &'a str,
    pub requesting_location: &'a str,
}

// ─── invite ──────────────────────────────────────────────────

#[derive(Template, Debug)]
#[template(path = "invite.html")]
pub struct InviteHtml<'a> {
    pub name: &'a str,
    pub link: &'a str,
    pub expires_in: &'a str,
}

#[derive(Template, Debug)]
#[template(path = "invite.txt")]
pub struct InviteText<'a> {
    pub name: &'a str,
    pub link: &'a str,
    pub expires_in: &'a str,
}

// ─── password-reset ──────────────────────────────────────────

#[derive(Template, Debug)]
#[template(path = "password_reset.html")]
pub struct PasswordResetHtml<'a> {
    pub name: &'a str,
    pub link: &'a str,
    pub expires_in: &'a str, // "1 hour"
}

#[derive(Template, Debug)]
#[template(path = "password_reset.txt")]
pub struct PasswordResetText<'a> {
    pub name: &'a str,
    pub link: &'a str,
    pub expires_in: &'a str,
}

// ─── email-change ────────────────────────────────────────────

#[derive(Template, Debug)]
#[template(path = "email_change.html")]
pub struct EmailChangeHtml<'a> {
    pub name: &'a str,
    pub new_email: &'a str,
    pub link: &'a str,
    pub expires_in: &'a str,
}

#[derive(Template, Debug)]
#[template(path = "email_change.txt")]
pub struct EmailChangeText<'a> {
    pub name: &'a str,
    pub new_email: &'a str,
    pub link: &'a str,
    pub expires_in: &'a str,
}

// ─── reauthentication ────────────────────────────────────────

#[derive(Template, Debug)]
#[template(path = "reauthentication.html")]
pub struct ReauthenticationHtml<'a> {
    pub name: &'a str,
    pub token: &'a str,
    pub expires_in: &'a str,
}

#[derive(Template, Debug)]
#[template(path = "reauthentication.txt")]
pub struct ReauthenticationText<'a> {
    pub name: &'a str,
    pub token: &'a str,
    pub expires_in: &'a str,
}

// ─── suspicious-activity ─────────────────────────────────────

#[derive(Template, Debug)]
#[template(path = "suspicious_activity.html")]
pub struct SuspiciousActivityHtml<'a> {
    pub name: &'a str,
    pub event: &'a str,               // e.g. "Multiple failed sign-in attempts"
    pub time: &'a str,                // human-readable timestamp
    pub action_link: Option<&'a str>, // optional reset-password link
}

#[derive(Template, Debug)]
#[template(path = "suspicious_activity.txt")]
pub struct SuspiciousActivityText<'a> {
    pub name: &'a str,
    pub event: &'a str,
    pub time: &'a str,
    pub action_link: Option<&'a str>,
}

// ─── account-deletion request (ISS-12) ───────────────────────

#[derive(Template, Debug)]
#[template(path = "account_deletion_requested.html")]
pub struct AccountDeletionRequestedHtml<'a> {
    pub name: &'a str,
    pub link: &'a str,
    pub scheduled_for: &'a str, // human-readable date, e.g. "2026-07-11"
    pub grace_days: i64,
}

#[derive(Template, Debug)]
#[template(path = "account_deletion_requested.txt")]
pub struct AccountDeletionRequestedText<'a> {
    pub name: &'a str,
    pub link: &'a str,
    pub scheduled_for: &'a str,
    pub grace_days: i64,
}

// ─── Render helper ───────────────────────────────────────────

/// Render an html+txt template pair into an `Email` with the given recipient,
/// from-address, and subject. The pair must agree on field shapes — typed at
/// the call site by the template structs.
pub fn build_email(
    to: Address,
    from: Address,
    subject: String,
    text: String,
    html: String,
    tags: Vec<String>,
) -> Email {
    Email {
        to,
        // Transactional mail renders `To:` from `to` (no envelope/header split).
        header_to: None,
        from,
        // Transactional auth mail (verify / magic-link / reset / suspicious)
        // sets neither Reply-To nor an envelope-from override: the driver uses
        // `from` as both header-From and MAIL FROM, exactly as before.
        reply_to: None,
        envelope_from: None,
        subject,
        text,
        html: Some(html),
        headers: vec![],
        tags,
        // Transactional auth mail is not re-driven through a notify ledger; the
        // billing notifier sets its own key via `Email { idempotency_key: Some(..) }`.
        idempotency_key: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use askama::Template;

    #[test]
    fn renders_verify_email_html() {
        let tpl = VerifyEmailHtml {
            name: "Alice",
            link: "https://auth.zeroship.ai/verify?token=abc123",
            expires_in: "24 hours",
        };
        let html = tpl.render().expect("render");
        assert!(html.contains("Alice"));
        assert!(html.contains("https://auth.zeroship.ai/verify?token=abc123"));
        assert!(html.contains("24 hours"));
    }

    #[test]
    fn renders_magic_link_txt() {
        let tpl = MagicLinkText {
            name: "Bob",
            link: "https://auth.zeroship.ai/magic/verify?token=xyz",
            expires_in: "15 minutes",
            requesting_device: "Chrome on macOS",
            requesting_location: "San Francisco, CA",
        };
        let text = tpl.render().expect("render");
        assert!(text.contains("Bob"));
        assert!(text.contains("Chrome on macOS"));
        assert!(text.contains("San Francisco, CA"));
    }

    #[test]
    fn renders_password_reset() {
        let html = PasswordResetHtml {
            name: "Carol",
            link: "/r/abc",
            expires_in: "1 hour",
        }
        .render()
        .expect("render");
        assert!(html.contains("Carol"));
        assert!(html.contains("Reset password"));
    }

    #[test]
    fn renders_invite() {
        let text = InviteText {
            name: "Ivy",
            link: "https://auth.zeroship.ai/auth/v1/verify?token=invite",
            expires_in: "24 hours",
        }
        .render()
        .expect("render");
        assert!(text.contains("Ivy"));
        assert!(text.contains("invite"));
    }

    #[test]
    fn renders_email_change() {
        let html = EmailChangeHtml {
            name: "Erin",
            new_email: "new@example.com",
            link: "https://auth.zeroship.ai/auth/v1/verify?token=change",
            expires_in: "24 hours",
        }
        .render()
        .expect("render");
        assert!(html.contains("Erin"));
        assert!(html.contains("new@example.com"));
        assert!(html.contains("Confirm email change"));
    }

    #[test]
    fn renders_reauthentication() {
        let text = ReauthenticationText {
            name: "Rae",
            token: "123456",
            expires_in: "10 minutes",
        }
        .render()
        .expect("render");
        assert!(text.contains("123456"));
        assert!(text.contains("10 minutes"));
    }

    #[test]
    fn renders_suspicious_activity_with_action_link() {
        let html = SuspiciousActivityHtml {
            name: "Dave",
            event: "Multiple failed sign-in attempts",
            time: "2026-05-27 14:30 UTC",
            action_link: Some("https://auth.zeroship.ai/forgot"),
        }
        .render()
        .expect("render");
        assert!(html.contains("Dave"));
        assert!(html.contains("Multiple failed sign-in attempts"));
        assert!(html.contains("Change your password"));
    }

    #[test]
    fn renders_suspicious_activity_without_action_link() {
        let html = SuspiciousActivityHtml {
            name: "Eve",
            event: "New device sign-in",
            time: "2026-05-27 14:35 UTC",
            action_link: None,
        }
        .render()
        .expect("render");
        assert!(
            !html.contains("Change your password"),
            "action button should be hidden when action_link is None"
        );
    }
}
