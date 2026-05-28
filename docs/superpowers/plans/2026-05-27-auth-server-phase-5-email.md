# Auth server — Phase 5 implementation plan: Magic-link + email verification + mailer

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan.

**Goal:** Ship the email-side of the auth surface. Mailer abstraction with three drivers (stdout / lettre-SMTP / Resend), magic-link login (with same-device + cross-device CSRF binding), email verification, password reset, and bounce-webhook handling. By end of Phase 5 every login method the proposal promises is in place.

**Architecture:** `Mailer` trait lives in `crates/auth/src/mailer/`. Templates via `askama` (separate dir for emails because `.html` + `.txt` pairs). All email-triggering flows (`magic_link`, `verification`, `reset`) use the same opaque-token primitive (32 bytes CSPRNG, SHA-256 stored, single-use atomic redeem). Tables already exist from Phase 1 (`auth.magic_links`, `auth.email_verifications`, `auth.email_suppressions`).

**Tech Stack:** Continuing — compio, ntex, cyper, compio-postgres. New: `lettre` (SMTP), maybe `mail-builder` for MIME assembly. Templates remain `askama`.

**References:**
- Proposal §8.4 (magic-link), §8.5 (verification), §8.6 (reset), §12 (mailer)
- Phase 2's `identity::password` for the opaque-token pattern
- Phase 4's `oauth_stash` for HMAC-cookie pattern (cross-device CSRF reuses this)

**Pre-launch posture:** no back-compat. Tables already exist; flows are new code.

**Starting point:** worktree tip post-Phase-4 (`0958b710` after `auth-phase-4` tag). Live stack still up. 391 workspace tests green.

---

## Phase 5 unit list

| # | Unit | Files | Time |
|---|---|---|---|
| U1 | Mailer trait + StdoutMailer + Email type + suppression check | mailer/{mod,types,stdout}.rs + store/suppressions.rs | 40 min |
| U2 | SMTP (lettre) + Resend drivers | mailer/{smtp,resend}.rs | 60 min |
| U3 | Email templates (verify, magic, reset, suspicious-activity) | mailer/templates/*.{html,txt} | 30 min |
| U4 | Magic-link login (same-device + cross-device) | identity/magic_link.rs + ui/magic.rs | 90 min |
| U5 | Signup-time email verification | identity/verification.rs + integration into /signup | 50 min |
| U6 | Password reset flow | identity/password_reset.rs + /forgot + /reset | 50 min |
| U7 | Bounce/complaint webhooks | mailer/bounce.rs + /webhooks/{postmark,ses-sns} | 40 min |
| U8 | Phase 5 close-out + tag | – | 10 min |

Total: ~6h, ~12 commits.

---

# Unit U1 · Mailer trait + StdoutMailer + suppression

## Task U1.1 · Mailer trait + Email type

Files:
- Create `crates/auth/src/mailer/mod.rs` (declares submodules + the trait)
- Create `crates/auth/src/mailer/types.rs` — `Email`, `Address`, `MessageId`, `MailerError`
- Create `crates/auth/src/mailer/stdout.rs` — `StdoutMailer` impl
- Create `crates/auth/src/store/suppressions.rs` — `auth.email_suppressions` CRUD (`is_suppressed(email)`, `add(email, reason)`)
- Modify `crates/auth/src/lib.rs` and `crates/auth/src/store/mod.rs` to export

```rust
//! crates/auth/src/mailer/types.rs

use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Email {
    pub to: Address,
    pub from: Address,
    pub subject: String,
    pub text: String,
    pub html: Option<String>,
    pub headers: Vec<(String, String)>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Address {
    pub email: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MessageId(pub String);

#[derive(Debug, Error)]
pub enum MailerError {
    #[error("suppressed recipient: {0}")]
    Suppressed(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("config: {0}")]
    Config(String),
}
```

```rust
//! crates/auth/src/mailer/mod.rs

pub mod stdout;
pub mod types;

use async_trait::async_trait;
use compio_postgres::Client;

use crate::error::Result as AuthResult;
use crate::store::suppressions;
pub use types::{Address, Email, MailerError, MessageId};

#[async_trait]
pub trait Mailer: Send + Sync + std::fmt::Debug {
    /// Send an email. Implementations MUST check `auth.email_suppressions`
    /// before transport and return `MailerError::Suppressed` if the recipient
    /// is on the suppression list.
    async fn send(&self, db: &Client, msg: Email) -> std::result::Result<MessageId, MailerError>;
}

/// Helper that all impls call before transport.
///
/// # Errors
///
/// `MailerError::Suppressed` if the recipient is in `auth.email_suppressions`.
pub async fn check_suppression(db: &Client, email: &str) -> std::result::Result<(), MailerError> {
    suppressions::is_suppressed(db, email).await
        .map_err(|e| MailerError::Transport(format!("suppression check: {e}")))?
        .then(|| Err::<(), _>(MailerError::Suppressed(email.to_string())))
        .unwrap_or(Ok(()))
}
```

```rust
//! crates/auth/src/mailer/stdout.rs

use async_trait::async_trait;
use compio_postgres::Client;

use crate::mailer::{check_suppression, Email, Mailer, MailerError, MessageId};

#[derive(Debug, Default)]
pub struct StdoutMailer;

#[async_trait]
impl Mailer for StdoutMailer {
    async fn send(&self, db: &Client, msg: Email) -> Result<MessageId, MailerError> {
        check_suppression(db, &msg.to.email).await?;
        tracing::info!(
            target: "auth.mailer.stdout",
            to = %msg.to.email,
            from = %msg.from.email,
            subject = %msg.subject,
            tags = ?msg.tags,
            "stdout mailer: send",
        );
        eprintln!("\n=== MAIL ===\nTo: {} <{}>\nFrom: {} <{}>\nSubject: {}\n\n{}\n=== END ===\n",
            msg.to.name.as_deref().unwrap_or(""), msg.to.email,
            msg.from.name.as_deref().unwrap_or(""), msg.from.email,
            msg.subject, msg.text);
        Ok(MessageId(format!("stdout-{}", uuid::Uuid::new_v4().simple())))
    }
}
```

## Task U1.2 · Suppressions CRUD

```rust
//! crates/auth/src/store/suppressions.rs

use compio_postgres::Client;
use crate::error::{AuthError, Result};

pub async fn is_suppressed(conn: &Client, email: &str) -> Result<bool> {
    let rows = conn.query(
        "SELECT 1 FROM auth.email_suppressions WHERE email = $1::citext",
        &[&email],
    ).await.map_err(|e| AuthError::Db(format!("is_suppressed: {e}")))?;
    Ok(!rows.is_empty())
}

pub async fn add(conn: &Client, email: &str, reason: &str, provider_msg: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO auth.email_suppressions (email, reason, provider_msg) \
         VALUES ($1::citext, $2, $3) \
         ON CONFLICT (email) DO UPDATE SET reason = EXCLUDED.reason, provider_msg = EXCLUDED.provider_msg",
        &[&email, &reason, &provider_msg],
    ).await.map_err(|e| AuthError::Db(format!("suppressions::add: {e}")))?;
    Ok(())
}
```

## Task U1.3 · Tests

- Unit-test the StdoutMailer (drop test if it requires too much state)
- Live-PG test: add an entry to suppressions, verify `is_suppressed` returns true, verify StdoutMailer returns Suppressed for that address

## Commit

```
auth: mailer/{mod,types,stdout} + store/suppressions — Mailer trait + suppression-checking StdoutMailer
```

---

# Unit U2 · SMTP + Resend drivers

## Task U2.1 · `lettre`-based SMTP driver

File: `crates/auth/src/mailer/smtp.rs`.

```rust
//! SMTP transport via `lettre`. Configurable host/port/credentials/tls.

use async_trait::async_trait;
use compio_postgres::Client;
use lettre::{
    transport::smtp::{authentication::Credentials, AsyncSmtpTransport},
    AsyncTransport, Message, Tokio1Executor,
};

use crate::mailer::{check_suppression, Email, Mailer, MailerError, MessageId};

#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub use_starttls: bool,
}

#[derive(Debug)]
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl SmtpMailer {
    pub fn new(cfg: &SmtpConfig) -> Result<Self, MailerError> {
        // Construct lettre transport — see lettre docs for the right builder
        // (relay vs unencrypted_localhost vs starttls).
        // ...
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, db: &Client, msg: Email) -> Result<MessageId, MailerError> {
        check_suppression(db, &msg.to.email).await?;
        let message = build_lettre_message(&msg)?;
        let response = self.transport.send(message).await
            .map_err(|e| MailerError::Transport(format!("smtp send: {e}")))?;
        Ok(MessageId(format!("smtp-{:?}", response.code())))
    }
}

fn build_lettre_message(msg: &Email) -> Result<Message, MailerError> {
    use lettre::message::header::ContentType;
    // ... build Message with multipart text+html if both present
}
```

**Gotcha:** `lettre`'s async transport is `Tokio1Executor`. The auth crate is compio. This is one of the rare places we touch tokio — but it's confined to lettre's transport, called from our spawn_blocking. Wrap `transport.send(message).await` in a compio `spawn_blocking` if the future is non-Send (likely it is). Alternative: use `lettre`'s blocking transport (`SmtpTransport`) inside `spawn_blocking`. This is the cleanest path — pick blocking transport.

```rust
let response = compio::runtime::spawn_blocking(move || {
    self.transport.send(&message)
}).await
    .map_err(|e| MailerError::Transport(format!("smtp join: {e}")))?
    .map_err(|e| MailerError::Transport(format!("smtp send: {e}")))?;
```

(Switch to `lettre::SmtpTransport` not `AsyncSmtpTransport`. Adjust types accordingly.)

## Task U2.2 · Resend driver (HTTP API via cyper)

File: `crates/auth/src/mailer/resend.rs`.

```rust
//! Resend transactional-email API (https://resend.com).
//! Wire format: POST https://api.resend.com/emails with Bearer api-key.

use async_trait::async_trait;
use compio_postgres::Client;
use serde::{Deserialize, Serialize};

use crate::mailer::{check_suppression, Email, Mailer, MailerError, MessageId};

#[derive(Debug, Clone)]
pub struct ResendConfig {
    pub api_key: String,
}

#[derive(Debug)]
pub struct ResendMailer {
    cfg: ResendConfig,
}

impl ResendMailer {
    pub fn new(cfg: ResendConfig) -> Self { Self { cfg } }
}

#[async_trait]
impl Mailer for ResendMailer {
    async fn send(&self, db: &Client, msg: Email) -> Result<MessageId, MailerError> {
        check_suppression(db, &msg.to.email).await?;

        #[derive(Serialize)]
        struct ResendRequest { from: String, to: String, subject: String, text: String, html: Option<String> }
        #[derive(Deserialize)]
        struct ResendResponse { id: String }

        let body = serde_json::to_vec(&ResendRequest {
            from: format_address(&msg.from),
            to: msg.to.email.clone(),
            subject: msg.subject.clone(),
            text: msg.text.clone(),
            html: msg.html.clone(),
        }).map_err(|e| MailerError::Transport(format!("resend encode: {e}")))?;

        let client = cyper::Client::new();
        let resp = client
            .request(http::Method::POST, "https://api.resend.com/emails")
            .map_err(|e| MailerError::Transport(format!("resend build: {e}")))?
            .header("authorization", &format!("Bearer {}", self.cfg.api_key))
            .map_err(|e| MailerError::Transport(format!("resend auth: {e}")))?
            .header("content-type", "application/json")
            .map_err(|e| MailerError::Transport(format!("resend ct: {e}")))?
            .body(body)
            .send()
            .await
            .map_err(|e| MailerError::Transport(format!("resend send: {e}")))?;

        let status = resp.status().as_u16();
        let body = resp.text().await
            .map_err(|e| MailerError::Transport(format!("resend read: {e}")))?;
        if !(200..300).contains(&status) {
            return Err(MailerError::Transport(format!("resend {status}: {body}")));
        }
        let parsed: ResendResponse = serde_json::from_str(&body)
            .map_err(|e| MailerError::Transport(format!("resend parse: {e}")))?;
        Ok(MessageId(parsed.id))
    }
}

fn format_address(a: &Address) -> String {
    match &a.name {
        Some(n) => format!("{n} <{}>", a.email),
        None => a.email.clone(),
    }
}
```

## Task U2.3 · Config + driver selection

Modify `crates/auth/src/config.rs::AuthConfig`:

```rust
#[arg(long, env = "AUTH_MAILER", default_value = "stdout")]
pub mailer: String,    // "stdout" | "smtp" | "resend"

// SMTP-specific
#[arg(long, env = "AUTH_SMTP_HOST")]
pub smtp_host: Option<String>,
#[arg(long, env = "AUTH_SMTP_PORT", default_value = "587")]
pub smtp_port: u16,
#[arg(long, env = "AUTH_SMTP_USERNAME")]
pub smtp_username: Option<String>,
#[arg(long, env = "AUTH_SMTP_PASSWORD")]
pub smtp_password: Option<String>,

// Resend-specific
#[arg(long, env = "AUTH_RESEND_API_KEY")]
pub resend_api_key: Option<String>,

// Sender address
#[arg(long, env = "AUTH_MAIL_FROM_EMAIL", default_value = "auth@zeroship.ai")]
pub mail_from_email: String,
#[arg(long, env = "AUTH_MAIL_FROM_NAME", default_value = "zeroship")]
pub mail_from_name: String,
```

In `main.rs`, factory-construct the right mailer based on `cfg.mailer`:

```rust
let mailer: Arc<dyn Mailer> = match cfg.mailer.as_str() {
    "stdout" => Arc::new(StdoutMailer),
    "smtp" => Arc::new(SmtpMailer::new(&SmtpConfig {
        host: cfg.smtp_host.clone().ok_or(AuthError::Config("smtp_host missing".into()))?,
        port: cfg.smtp_port,
        username: cfg.smtp_username.clone(),
        password: cfg.smtp_password.clone(),
        use_starttls: true,
    })?),
    "resend" => Arc::new(ResendMailer::new(ResendConfig {
        api_key: cfg.resend_api_key.clone().ok_or(AuthError::Config("resend_api_key missing".into()))?,
    })),
    other => return Err(AuthError::Config(format!("unknown mailer: {other}"))),
};
```

Thread `Arc<dyn Mailer>` through GateState as `State<Arc<dyn Mailer>>`. Handlers consume it for sending verification / magic / reset emails.

## Commit

Two sub-commits (one per driver) OR one combined commit. Combined is simpler:

```
auth: mailer — SMTP (lettre) + Resend (cyper) drivers + driver factory
```

---

# Unit U3 · Email templates

Files (all under `crates/auth/src/mailer/templates/`):
- `verify_email.html` + `.txt`
- `magic_link.html` + `.txt`
- `password_reset.html` + `.txt`
- `suspicious_activity.html` + `.txt`

`askama.toml` (already exists from P2-U3) needs `dirs` updated to include `src/mailer/templates`:

```toml
[general]
dirs = ["src/ui/templates", "src/mailer/templates"]
```

Template structs in `mailer/templates.rs` (new):

```rust
#[derive(Template, Debug)]
#[template(path = "verify_email.html")]
pub struct VerifyEmailHtml<'a> { pub name: &'a str, pub link: &'a str, pub expires_in: &'a str }

#[derive(Template, Debug)]
#[template(path = "verify_email.txt")]
pub struct VerifyEmailText<'a> { pub name: &'a str, pub link: &'a str, pub expires_in: &'a str }

// ... + magic_link, password_reset, suspicious_activity
```

Helper function `render_email(html_template, text_template)` returns `(text, html)` pair.

Keep templates minimal — inline-CSS, no images, plain links. Email-client compatibility is hard; minimal is best.

Example `magic_link.html`:

```html
<!DOCTYPE html>
<html><body style="font-family: sans-serif; max-width: 480px; margin: 0 auto; padding: 1rem">
<h2>Sign in to zeroship</h2>
<p>Hi {{ name }},</p>
<p>Click the link below to sign in to your zeroship account. The link expires in {{ expires_in }}.</p>
<p><a href="{{ link }}" style="display:inline-block;padding:0.75rem 1.25rem;background:#111;color:#fff;text-decoration:none;border-radius:4px">Sign in to zeroship</a></p>
<p style="font-size:0.875rem;color:#666">Or copy + paste this link into your browser:<br><code>{{ link }}</code></p>
<p style="font-size:0.875rem;color:#666">If you didn't request this, you can ignore this email — no action is needed.</p>
</body></html>
```

## Commit

```
auth: mailer/templates — verify-email, magic-link, password-reset, suspicious-activity (html + txt)
```

---

# Unit U4 · Magic-link login

The headline Phase 5 feature. Three sub-commits.

## Task U4.1 · `identity::magic_link` — issue + redeem (atomic single-use)

File: `crates/auth/src/identity/magic_link.rs`.

```rust
//! Magic-link login. 32-byte opaque CSPRNG token, SHA-256-stored, 15min TTL,
//! atomic single-use redemption. CSRF-nonce binding for cross-device defense.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::Client;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::error::{AuthError, Result};

pub const TTL_MINUTES: i64 = 15;

#[derive(Debug, Clone)]
pub struct IssuedToken {
    pub raw: String,            // base64url; what goes in the email
    pub csrf_nonce: String,     // matched against __Host-zsidp_magic_csrf cookie
}

#[derive(Debug, Clone)]
pub struct RedeemedToken {
    pub email: String,
    pub csrf_nonce: String,
    pub purpose: String,
}

/// Issue a new magic-link token. Stores the SHA-256 hash + CSRF nonce + email
/// in `auth.magic_links`. Old unconsumed tokens for the same email are marked
/// consumed (lowers attack surface).
pub async fn issue(db: &Client, email: &str, purpose: &str) -> Result<IssuedToken> {
    // Mark stale tokens consumed.
    db.execute(
        "UPDATE auth.magic_links SET consumed_at = NOW() \
         WHERE email = $1::citext AND consumed_at IS NULL",
        &[&email],
    ).await.map_err(|e| AuthError::Db(format!("magic_link stale-cleanup: {e}")))?;

    // Generate token + CSRF nonce.
    let mut token_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    let raw = URL_SAFE_NO_PAD.encode(token_bytes);

    let mut csrf_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut csrf_bytes);
    let csrf_nonce = URL_SAFE_NO_PAD.encode(csrf_bytes);

    let token_hash = Sha256::digest(raw.as_bytes());

    db.execute(
        "INSERT INTO auth.magic_links \
            (token_hash, email, csrf_nonce, purpose, expires_at) \
         VALUES ($1, $2::citext, $3, $4, NOW() + ($5::text || ' minutes')::interval)",
        &[&token_hash.as_slice(), &email, &csrf_nonce.as_str(), &purpose, &TTL_MINUTES.to_string()],
    ).await.map_err(|e| AuthError::Db(format!("magic_link insert: {e}")))?;

    Ok(IssuedToken { raw, csrf_nonce })
}

/// Redeem a token atomically (single UPDATE...RETURNING).
/// Returns the redeemed token's metadata if successful. None if missing/expired/already-consumed.
pub async fn redeem(db: &Client, raw_token: &str) -> Result<Option<RedeemedToken>> {
    let token_hash = Sha256::digest(raw_token.as_bytes());
    let rows = db.query(
        "UPDATE auth.magic_links \
         SET consumed_at = NOW() \
         WHERE token_hash = $1 \
           AND consumed_at IS NULL \
           AND expires_at > NOW() \
         RETURNING email::text, csrf_nonce, purpose",
        &[&token_hash.as_slice()],
    ).await.map_err(|e| AuthError::Db(format!("magic_link redeem: {e}")))?;

    Ok(rows.first().map(|r| RedeemedToken {
        email: r.get("email"),
        csrf_nonce: r.get("csrf_nonce"),
        purpose: r.get("purpose"),
    }))
}
```

Unit tests (live PG):
- Issue + redeem happy path
- Second redeem returns None (single-use)
- Expired token returns None
- Stale-cleanup on new issue marks old tokens consumed

Commit: `auth: identity/magic_link — issue + atomic single-use redeem (15min TTL, CSRF-nonce)`

## Task U4.2 · `/magic/start` POST + magic-link email send

User submits email on the login page (or a dedicated /magic-link entry point). Server issues a token, sends the email, sets the CSRF cookie. Always returns 200 with "if X is a known address, we've sent a link" (enumeration defense).

Wire `/login` POST to handle a magic-link path: if the form has `intent=magic` (no password), branch to magic-link issuance instead of password verify.

Files:
- Modify `crates/auth/src/ui/login.rs` — branch on `intent=magic`
- Or create `crates/auth/src/ui/magic.rs` for the dedicated handlers
- Update `login.html` to add a "Email me a sign-in link" button

```rust
pub async fn issue_magic_link(
    cookies: HeaderValue,
    form_email: &str,
    login_challenge: &str,
    cfg: &AuthConfig,
    db: &Client,
    mailer: Arc<dyn Mailer>,
) -> HttpResponse {
    // 1. Rate-limit (per-email, per-IP).
    // 2. Issue token (always — even if email doesn't have an account).
    let issued = match magic_link::issue(db, form_email, "login").await { /* ... */ };
    // 3. Build email — embed link with login_challenge so redeem can continue OIDC flow.
    let link = format!("{}/magic/verify?t={}&login_challenge={}",
                        cfg.public_url, issued.raw, login_challenge);
    let body_html = MagicLinkHtml { name: form_email.split('@').next().unwrap_or(""), link: &link, expires_in: "15 minutes" }
        .render().unwrap_or_default();
    let body_text = MagicLinkText { name: form_email.split('@').next().unwrap_or(""), link: &link, expires_in: "15 minutes" }
        .render().unwrap_or_default();
    // 4. Send via mailer (suppressed errors silently logged; UI still shows "sent" message).
    let _ = mailer.send(db, Email { /* ... */ }).await;
    // 5. Set __Host-zsidp_magic_csrf cookie with issued.csrf_nonce.
    // 6. Render "check your email" page.
    // ...
}
```

## Task U4.3 · `/magic/verify` GET + redeem (same-device + cross-device)

File: `crates/auth/src/ui/magic.rs::verify`.

```rust
pub async fn verify(
    req: HttpRequest,
    query: Query<VerifyQuery>,
    cfg: State<Arc<AuthConfig>>,
    db: State<Arc<Client>>,
    admin: State<Arc<HydraAdmin>>,
) -> HttpResponse {
    // 1. Redeem the token atomically.
    let redeemed = match magic_link::redeem(&db, &query.t).await {
        Ok(Some(r)) => r,
        _ => return render_error("link invalid or expired", None),
    };

    // 2. Check the CSRF cookie.
    let cookie_h = req.headers().get(http::header::COOKIE)
        .and_then(|v| v.to_str().ok()).unwrap_or("");
    let cookie_nonce = parse_magic_csrf_cookie(cookie_h);

    if cookie_nonce.as_deref() == Some(redeemed.csrf_nonce.as_str()) {
        // SAME-DEVICE PATH: mint session, accept_login, redirect.
        // ... (look up user by email; auto-create if signup purpose)
    } else {
        // CROSS-DEVICE PATH: store a short-lived 6-digit code on a new
        // auth.magic_completions table. Show the code on this page. The
        // requesting device polls a separate endpoint until it sees the
        // code, then prompts the user to type it back.
        // ...
    }
}
```

For cross-device, add a new table:

```sql
CREATE TABLE IF NOT EXISTS auth.magic_completions (
    csrf_nonce       TEXT PRIMARY KEY,
    code             TEXT NOT NULL,       -- 6 digit string
    email            CITEXT NOT NULL,
    login_challenge  TEXT NOT NULL,
    expires_at       TIMESTAMPTZ NOT NULL,
    consumed_at      TIMESTAMPTZ
)
```

The flow:
1. Cross-device redeem stores `(csrf_nonce, code, email, login_challenge, expires_at = NOW() + 5min)`
2. Redeeming device shows the 6-digit code with instructions
3. Requesting device polls `/magic/await?nonce=<csrf>` until the row appears with a code
4. Requesting device prompts user to enter the code
5. User types the code → POSTs `/magic/complete?nonce=<csrf>&code=XYZ` → looks up + atomic-consume the row → accept_login

This is non-trivial. Sub-commits:
- U4.3a — same-device path only (commit, ship as half-functional)
- U4.3b — cross-device interstitial + polling + complete
- U4.3c — wire into /login UI as the "Email me a link" entry

Or as a single commit if the implementer can do it cleanly.

## Commit messages

```
auth: ui/magic — same-device verify path (CSRF-cookie match → accept_login)
auth: ui/magic — cross-device verify with 6-digit-code interstitial
auth: /login — "Email me a sign-in link" entry point
```

---

# Unit U5 · Email verification

Same primitive as magic-link, different `purpose='verification'`, 24h TTL.

Files:
- Create `crates/auth/src/identity/verification.rs` (issue + redeem)
- Modify `crates/auth/src/ui/signup.rs` — on signup success, issue verification token + send email
- Create `crates/auth/src/ui/verify.rs` — `/verify?t=...` GET handler that redeems + sets `auth.users.email_verified_at = NOW()`

The 24h TTL means a NEW table is overkill — reuse `auth.email_verifications` (already exists from Phase 1). The primitive in `verification.rs` is a thin wrapper around the same hash-and-redeem pattern from `magic_link.rs`. **Extract** the shared core if the duplication is too painful — but per the user-facing scope (different purpose, different TTL, different table), keeping them separate is fine.

Phase 5 ships the verification email + redemption. The "7-day hard wall on unverified users" (proposal §8.5) is deferred to Phase 6.

## Commit

```
auth: identity/verification + /verify — signup-time email verification (24h TTL)
```

---

# Unit U6 · Password reset

Files:
- Create `crates/auth/src/identity/password_reset.rs` (issue + redeem)
- Create `crates/auth/src/ui/forgot.rs` (`/forgot` GET + POST — always returns 200)
- Create `crates/auth/src/ui/reset.rs` (`/reset?t=...` GET + POST — set new password)

Same primitive (hash-and-redeem), 1h TTL, `purpose='reset'`. Reuses `auth.magic_links` (different purpose).

`/reset` POST:
1. CSRF check
2. Redeem token
3. Look up user by `redeemed.email`
4. Validate new password (≥ 15 chars, HIBP check)
5. Argon2id hash via spawn_blocking
6. `UPDATE auth.users SET password_hash = $1, updated_at = NOW() WHERE id = $2`
7. Audit `password_changed`
8. Redirect to `/login`

Templates: `forgot.html` + `reset.html` (askama, follow login.html / signup.html shape).

## Commit

```
auth: identity/password_reset + /forgot + /reset — 1h reset token, NIST-policy new password
```

---

# Unit U7 · Bounce / complaint webhooks

Files:
- Create `crates/auth/src/mailer/bounce.rs` (HMAC-signature verification for Postmark + SNS message-signing for SES)
- Create webhook handlers in `crates/auth/src/ui/webhooks.rs`

Endpoints:
- `POST /webhooks/postmark` — Postmark signs with `X-Postmark-Webhook-Signature` (HMAC-SHA1 over body with shared secret)
- `POST /webhooks/ses-sns` — SES posts via SNS; verify the SNS message signature (HTTPS GET to SigningCertURL, RSA-SHA1 verify)

On verified bounce/complaint: call `suppressions::add(email, reason)`.

For Phase 5 simplicity, **SES-SNS signature verification is complex** (requires fetching the cert and validating). Defer the full impl with a TODO; ship the Postmark webhook (simpler) fully. SES via SNS can be a Phase 6 hardening item.

## Commit

```
auth: webhooks — Postmark bounce/complaint handler (SES-SNS deferred to Phase 6)
```

---

# Unit U8 · Phase 5 close-out

- Full test sweep
- Clippy count
- `cargo build --workspace`
- Milestone commit:

```
auth: Phase 5 complete — magic-link + email verification + password reset + mailer

Email-side complete. Phase 6 (JWK rotation cron, DPoP, audit retention,
load test, security review, deploy runbook) is the final phase.
```

- Tag `auth-phase-5`

---

# Future phases

- **Phase 6** — JWK rotation cron; DPoP at the gateway; audit-log retention sweeper; load test; security review; `docs/runbooks/auth-deploy.md`.

---

# Self-review

**Spec coverage** — proposal §8.4 (magic-link, incl. cross-device), §8.5 (verification), §8.6 (reset), §12 (mailer), §15 (audit events for these flows) all mapped.

**Placeholder scan** — no TBDs. Some implementation outlines are sparse (build_lettre_message, SNS sig verification) — implementer fills in.

**Scope** — Phase 5 ships every flow that requires sending an email. The 7-day hard wall on unverified users and SES-SNS sig verification are deferred to Phase 6.

# Execution handoff

Plan saved to `docs/superpowers/plans/2026-05-27-auth-server-phase-5-email.md`.
Execute via subagent-driven-development.
