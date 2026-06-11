//! Auth server configuration.

use std::path::PathBuf;

use clap::Parser;
use zeroship_core::config::{
    parse_bool_flag, resolve_overlay_string, AuthSection, DEV_STASH_SIGNING_KEY, DEV_TOTP_ENC_KEY,
};

use crate::mailer::SmtpTls;
use zeroship_core::observability::ObservabilityFlags;

const DEFAULT_HYDRA_ADMIN_URL: &str = "http://127.0.0.1:4445";
const DEFAULT_HYDRA_PUBLIC_URL: &str = "https://auth.zeroship.ai";

#[derive(Clone, Parser)]
#[command(name = "zeroship-auth")]
pub struct AuthConfig {
    /// Optional shared config overlay path.
    #[arg(long = "config", env = "ZEROSHIP_CONFIG")]
    pub config_path: Option<PathBuf>,

    /// Disable well-known config auto-discovery (`/etc/zeroship/zeroship.toml`).
    #[arg(long = "no-config")]
    pub no_config: bool,

    /// Validate config (CLI + overlay + guards) and print the resolved non-secret config, then exit without starting the server.
    #[arg(long = "check-config")]
    pub check_config: bool,

    /// `--check-config` output format: `text` (default) or `json`.
    #[arg(long = "check-config-format", default_value = "text", value_parser = ["text", "json"])]
    pub check_config_format: String,

    /// Observability CLI/env overrides.
    #[command(flatten)]
    pub obs: ObservabilityFlags,

    /// Listen address. Defaults to loopback; compose passes `0.0.0.0:9092`.
    #[arg(long, env = "AUTH_ADDR", default_value = "127.0.0.1:9092")]
    pub addr: String,

    /// `PostgreSQL` DSN.
    #[arg(long, env = "AUTH_DB_URL", hide_env_values = true)]
    pub db_url: String,

    /// Hydra admin base URL (loopback).
    #[arg(long = "hydra-admin-url", env = "HYDRA_ADMIN_URL")]
    pub hydra_admin_url: Option<String>,

    /// Permit a non-loopback Hydra admin URL. Production deployments that
    /// enable this must protect Hydra admin externally with mTLS, firewall
    /// rules, or equivalent network policy.
    #[arg(
        long,
        env = "AUTH_ALLOW_REMOTE_HYDRA_ADMIN",
        action = clap::ArgAction::Set,
        default_value_t = false,
        default_missing_value = "true",
        num_args = 0..=1
    )]
    pub allow_remote_hydra_admin: bool,

    /// Hydra public base URL (issuer).
    #[arg(long = "hydra-public-url", env = "HYDRA_PUBLIC_URL")]
    pub hydra_public_url: Option<String>,

    /// Path to clients config TOML.
    #[arg(long, env = "AUTH_CLIENTS_CONFIG", default_value = "/etc/zeroship/auth-clients.toml")]
    pub clients_config: String,

    /// Allow first-boot JWK + client creation. Without this, an empty
    /// `hydra_jwk` set is a fatal startup error.
    #[arg(long, env = "AUTH_BOOTSTRAP")]
    pub bootstrap: bool,

    /// Dev mode: drop the Secure flag on cookies + relax secret guards.
    /// ONLY for localhost. `--dev-insecure` (no value) enables it;
    /// `--dev-insecure=false` disables a stray `ZEROSHIP_DEV_INSECURE=1`.
    #[arg(
        long = "dev-insecure",
        env = "ZEROSHIP_DEV_INSECURE",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = parse_bool_flag
    )]
    pub dev_insecure: Option<bool>,

    /// Resolved dev-insecure flag (CLI presence > env > default false).
    /// Not a CLI/env arg of its own; populated by [`AuthConfig::resolve`].
    #[arg(skip)]
    pub insecure_dev: bool,

    /// HMAC key (≥32 bytes recommended) used to sign the short-lived
    /// federation stash cookie (`__Host-zsidp_google_stash` etc.). A weak
    /// or default value lets an attacker forge stash cookies and bypass
    /// the OAuth state/PKCE check, so production deployments MUST set
    /// this explicitly. Empty default keeps the dev sentinel out of
    /// `--help`; the dev fallback (`DEV_STASH_SIGNING_KEY`) is applied in
    /// code under `--dev-insecure`.
    #[arg(
        long,
        env = "AUTH_STASH_SIGNING_KEY",
        default_value = "",
        hide_env_values = true
    )]
    pub stash_signing_key: String,

    /// AES-256-GCM key material for encrypting the TOTP shared secret at rest
    /// (ISS-11). Sourced like every other auth secret (CLI/env > `[secrets]`
    /// file reference). MUST decode (hex or base64url) to ≥32 bytes — validated
    /// at boot by `validate_master_key_material`, identical to the bundle/master
    /// key posture. A weak or absent key means an attacker with DB read access
    /// recovers every user's TOTP seed and can mint valid codes, so production
    /// MUST set it. Empty default keeps the dev sentinel out of `--help`; the
    /// dev fallback (`DEV_TOTP_ENC_KEY`) is applied in code under
    /// `--dev-insecure`. The encryption AAD binds the row's `user_id`, so a
    /// ciphertext lifted onto another user's row fails to decrypt.
    ///
    /// OPERATOR NOTE (flagged for review): this is a DEDICATED key, NOT derived
    /// from the stash/pairwise secrets, so 2FA seeds rotate independently of the
    /// session-signing material. Rotating it without re-encrypting existing
    /// `totp_credentials` rows invalidates every enrolled secret (users must
    /// re-enroll); add it to the `[secrets]` rotation grace path when key
    /// rotation lands.
    #[arg(
        long = "totp-enc-key",
        env = "AUTH_TOTP_ENC_KEY",
        default_value = "",
        hide_env_values = true
    )]
    pub totp_enc_key: String,

    /// Console origin(s) allowed to FRAME the login/signup/consent documents
    /// via CSP `frame-ancestors` (immersive iframe login, design §4.3/§10.1).
    /// The framed-route security headers emit
    /// `frame-ancestors 'self' <these origins>` and DROP `X-Frame-Options`;
    /// every other route keeps `XFO: DENY` + `frame-ancestors 'none'`. This is
    /// the browser-enforced anti-clickjacking gate that replaced the deleted
    /// gateway credential-oracle first-party gate. Each entry is an exact origin
    /// (`scheme://host[:port]`); NO wildcards (a `https://*.zeroship.ai` would
    /// re-admit every creator app and defeat the property). Empty (the default)
    /// ⇒ no origin is admitted, so the framing relax is a no-op and the strict
    /// `frame-ancestors 'none'` default holds (dev / single-origin deployments).
    /// Production sets exactly the console origin (e.g.
    /// `https://console.zeroship.ai`).
    ///
    /// Each entry MUST be SAME-SITE (same registrable domain / eTLD+1) with the
    /// auth issuer host ([`Self::public_url`]). The framed login's CSRF cookie is
    /// `SameSite=Strict`, so it only reaches the in-frame POST when the console
    /// shares the issuer's registrable domain; a cross-registrable-domain console
    /// would silently break framed login (cookie withheld). [`Self::resolve`]
    /// fail-closes by DROPPING any non-same-site origin — that deployment falls
    /// back to popup login rather than getting a relaxed `frame-ancestors` that
    /// can't actually authenticate (review finding I7).
    #[arg(
        long = "frame-ancestor-origin",
        env = "FRAME_ANCESTOR_ORIGINS",
        value_delimiter = ','
    )]
    pub frame_ancestor_origins: Vec<String>,

    // ─── Google OAuth (optional — federation routes registered only when set) ───
    /// Google OAuth 2.0 client ID. Without it, `/oauth/google/*` routes are
    /// not registered (auth still boots).
    #[arg(long, env = "AUTH_GOOGLE_CLIENT_ID")]
    pub google_client_id: Option<String>,

    /// Google OAuth 2.0 client secret.
    #[arg(long, env = "AUTH_GOOGLE_CLIENT_SECRET", hide_env_values = true)]
    pub google_client_secret: Option<String>,

    /// Redirect URI registered with Google. Must match the value configured in
    /// the Google Cloud Console exactly.
    #[arg(
        long,
        env = "AUTH_GOOGLE_REDIRECT_URI",
        default_value = "https://auth.zeroship.ai/oauth/google/callback"
    )]
    pub google_redirect_uri: String,

    /// Google's authorize endpoint. Overridable so the e2e tests can point
    /// at an in-process [`tests/common/mock_provider`] instead of the real
    /// Google. Production deployments should leave the default in place.
    #[arg(
        long,
        env = "AUTH_GOOGLE_AUTH_URL",
        default_value = "https://accounts.google.com/o/oauth2/v2/auth"
    )]
    pub google_auth_url: String,

    /// Google's token endpoint. Overridable for tests; production leaves
    /// the default.
    #[arg(
        long,
        env = "AUTH_GOOGLE_TOKEN_URL",
        default_value = "https://oauth2.googleapis.com/token"
    )]
    pub google_token_url: String,

    /// Google's JWKS endpoint. Overridable for tests; production leaves
    /// the default.
    #[arg(
        long,
        env = "AUTH_GOOGLE_JWKS_URL",
        default_value = "https://www.googleapis.com/oauth2/v3/certs"
    )]
    pub google_jwks_url: String,

    /// Expected `iss` claim on Google ID tokens. Overridable for tests
    /// (mock provider uses its own loopback base URL); production leaves
    /// the default — Google emits this exact string per its OIDC
    /// discovery document.
    #[arg(
        long,
        env = "AUTH_GOOGLE_ISSUER",
        default_value = "https://accounts.google.com"
    )]
    pub google_issuer: String,

    // ─── GitHub OAuth (optional) ───
    /// GitHub OAuth App client ID. Without it, `/oauth/github/*` routes are
    /// not registered (auth still boots).
    #[arg(long, env = "AUTH_GITHUB_CLIENT_ID")]
    pub github_client_id: Option<String>,

    /// GitHub OAuth App client secret.
    #[arg(long, env = "AUTH_GITHUB_CLIENT_SECRET", hide_env_values = true)]
    pub github_client_secret: Option<String>,

    /// Callback URL registered on the GitHub OAuth App. Must match what's set
    /// in the app's settings.
    #[arg(
        long,
        env = "AUTH_GITHUB_REDIRECT_URI",
        default_value = "https://auth.zeroship.ai/oauth/github/callback"
    )]
    pub github_redirect_uri: String,

    /// GitHub's authorize endpoint. Overridable for the federation e2e
    /// tests; production deployments leave the default in place.
    #[arg(
        long,
        env = "AUTH_GITHUB_AUTHORIZE_URL",
        default_value = "https://github.com/login/oauth/authorize"
    )]
    pub github_authorize_url: String,

    /// GitHub's token endpoint. Overridable for tests.
    #[arg(
        long,
        env = "AUTH_GITHUB_TOKEN_URL",
        default_value = "https://github.com/login/oauth/access_token"
    )]
    pub github_token_url: String,

    /// GitHub's `/user` endpoint. Overridable for tests.
    #[arg(
        long,
        env = "AUTH_GITHUB_USER_URL",
        default_value = "https://api.github.com/user"
    )]
    pub github_user_url: String,

    /// GitHub's `/user/emails` endpoint. Overridable for tests.
    #[arg(
        long,
        env = "AUTH_GITHUB_EMAILS_URL",
        default_value = "https://api.github.com/user/emails"
    )]
    pub github_emails_url: String,

    // ─── Mailer (driver selection + per-driver creds) ────────────────────
    /// Mailer driver: `stdout` (dev default) | `smtp` | `resend`.
    #[arg(long, env = "AUTH_MAILER", default_value = "stdout")]
    pub mailer: String,

    /// SMTP relay hostname (required when `--mailer=smtp`).
    #[arg(long, env = "AUTH_SMTP_HOST")]
    pub smtp_host: Option<String>,

    /// SMTP port. Defaults to 587 (STARTTLS); use 465 for implicit SMTPS.
    #[arg(long, env = "AUTH_SMTP_PORT", default_value = "587")]
    pub smtp_port: u16,

    /// SMTP username (optional — server may allow unauthenticated relays).
    #[arg(long, env = "AUTH_SMTP_USERNAME")]
    pub smtp_username: Option<String>,

    /// SMTP password (paired with `--smtp-username`).
    #[arg(long, env = "AUTH_SMTP_PASSWORD", hide_env_values = true)]
    pub smtp_password: Option<String>,

    /// Transport encryption for the transactional SMTP leg:
    /// `starttls` (default, port 587) | `implicit` (SMTPS, port 465) |
    /// `plaintext` (no TLS — dev/test sinks like mailpit on :1025 ONLY).
    #[arg(
        long = "smtp-tls",
        env = "AUTH_SMTP_TLS",
        value_enum,
        default_value_t = SmtpTls::Starttls
    )]
    pub smtp_tls: SmtpTls,

    /// Resend API key (required when `--mailer=resend`).
    #[arg(long, env = "AUTH_RESEND_API_KEY", hide_env_values = true)]
    pub resend_api_key: Option<String>,

    /// `From` address every transactional mail uses.
    #[arg(
        long,
        env = "AUTH_MAIL_FROM_EMAIL",
        default_value = "auth@zeroship.ai"
    )]
    pub mail_from_email: String,

    /// `From` display name every transactional mail uses.
    #[arg(long, env = "AUTH_MAIL_FROM_NAME", default_value = "zeroship")]
    pub mail_from_name: String,

    /// Public, externally-reachable origin of this auth server. Used when
    /// constructing absolute URLs embedded in outbound email (e.g. the
    /// magic-link href). Distinct from [`Self::addr`] — that's the bind
    /// address (`0.0.0.0:9092` in prod, which is NOT a real origin).
    ///
    /// Defaults to the dev-loopback value; production deployments MUST
    /// override with the auth host, including scheme + (optional) port.
    #[arg(
        long,
        env = "AUTH_PUBLIC_URL",
        default_value = "http://localhost:9092"
    )]
    pub public_url: String,

    // ─── Relay email (Slice 5 — app → user one-way forwarding) ───────────
    /// Relay alias domain. Aliases are minted as `{token}@{relay_domain}`
    /// (lowercase). Dev: `relay.zeroship.localhost`; prod: `relay.zeroship.ai`
    /// (sub-spec §9). The inbound webhook resolves `OriginalRecipient` against
    /// this domain and the forward builder rewrites `From`/`Reply-To` to it.
    #[arg(
        long = "relay-domain",
        env = "RELAY_DOMAIN",
        default_value = "relay.zeroship.localhost"
    )]
    pub relay_domain: String,

    /// HTTP Basic-auth username the inbound provider (Postmark Inbound) must
    /// present on every `POST /webhooks/relay-inbound`. When unset, the handler
    /// 401s so an unverified POST cannot drive a forward/suppression spoof
    /// (sub-spec §4.3 step 1).
    #[arg(long = "relay-inbound-user", env = "AUTH_RELAY_INBOUND_USER")]
    pub relay_inbound_user: Option<String>,

    /// HTTP Basic-auth password paired with [`Self::relay_inbound_user`].
    #[arg(
        long = "relay-inbound-password",
        env = "AUTH_RELAY_INBOUND_PASSWORD",
        hide_env_values = true
    )]
    pub relay_inbound_password: Option<String>,

    /// Relay-forward mailer driver: `smtp` (default — the forward path needs
    /// envelope-from control) | `stdout` (dev terminal). `resend` is REJECTED
    /// for this role (it cannot pin envelope-from, sub-spec §3.2/§5.2a). This
    /// is a SECOND, dedicated mailer distinct from [`Self::mailer`] so the relay
    /// sends from the relay-domain identity (§5.5 reputation isolation).
    #[arg(
        long = "relay-forward-mailer",
        env = "AUTH_RELAY_FORWARD_MAILER",
        default_value = "smtp"
    )]
    pub relay_forward_mailer: String,

    /// Relay-forward SMTP host (required when `--relay-forward-mailer=smtp`).
    /// Independent of [`Self::smtp_host`] — the relay sending identity is
    /// distinct from the transactional one (sub-spec §5.2a).
    #[arg(long = "relay-smtp-host", env = "AUTH_RELAY_SMTP_HOST")]
    pub relay_smtp_host: Option<String>,

    /// Relay-forward SMTP port. Defaults to 587 (STARTTLS).
    #[arg(long = "relay-smtp-port", env = "AUTH_RELAY_SMTP_PORT", default_value = "587")]
    pub relay_smtp_port: u16,

    /// Relay-forward SMTP username (optional — dev sinks need none).
    #[arg(long = "relay-smtp-username", env = "AUTH_RELAY_SMTP_USERNAME")]
    pub relay_smtp_username: Option<String>,

    /// Relay-forward SMTP password (paired with `--relay-smtp-username`).
    #[arg(
        long = "relay-smtp-password",
        env = "AUTH_RELAY_SMTP_PASSWORD",
        hide_env_values = true
    )]
    pub relay_smtp_password: Option<String>,

    /// Transport encryption for the relay-forward SMTP leg:
    /// `starttls` (default, port 587) | `implicit` (SMTPS, port 465) |
    /// `plaintext` (no TLS). Dev sinks (mailpit on :1025) use `plaintext`;
    /// prod uses `starttls`.
    #[arg(
        long = "relay-smtp-tls",
        env = "AUTH_RELAY_SMTP_TLS",
        value_enum,
        default_value_t = SmtpTls::Starttls
    )]
    pub relay_smtp_tls: SmtpTls,

    // ─── Postmark webhook (bounce/complaint receiver, P5-U7) ─────────────
    /// HTTP Basic-auth username Postmark must present on every
    /// `POST /webhooks/postmark` request. Configured per-server in the
    /// Postmark dashboard's webhook settings. When unset, the webhook
    /// handler responds with 401 so misrouted traffic doesn't silently
    /// succeed.
    #[arg(long, env = "AUTH_POSTMARK_WEBHOOK_USER")]
    pub postmark_webhook_user: Option<String>,

    /// HTTP Basic-auth password paired with [`Self::postmark_webhook_user`].
    /// See that field for the rationale.
    #[arg(
        long,
        env = "AUTH_POSTMARK_WEBHOOK_PASSWORD",
        hide_env_values = true
    )]
    pub postmark_webhook_password: Option<String>,

    // ─── Cron (P6-U1: jwk_rotation; future units add audit retention) ───
    /// Days between JWK rotations. Once a set's `zeroship.cron_state` row is
    /// older than this, the next cron tick prepends fresh keys and they
    /// become the active signers (hydra signs with the head of the
    /// list). 90 days mirrors the OIDC operator handbook default.
    #[arg(long, env = "AUTH_JWK_ROTATION_DAYS", default_value = "90")]
    pub jwk_rotation_days: i64,

    /// Days to retain outgoing keys past their rotation. Old keys are
    /// retired once `(rotation_days + retain_days)` has elapsed since
    /// the most recent rotation — long enough for any access token
    /// signed by the outgoing key to expire (default 31 ≫ 1 h access
    /// token TTL, ≫ typical refresh window).
    #[arg(long, env = "AUTH_JWK_RETAIN_DAYS", default_value = "31")]
    pub jwk_retain_days: i64,

    /// Cron tick interval in seconds. Default 86400 (24 h). Operators
    /// drop this to seconds in staging/integration tests so a cron
    /// behaviour change is observable inside a single test run.
    #[arg(long, env = "AUTH_CRON_TICK_SECS", default_value = "86400")]
    pub cron_tick_secs: u64,

    /// Audit-retention sweeper tick interval in seconds. Default 3600
    /// (hourly). The sweep itself is cheap (one indexed DELETE per
    /// bucket) so hourly cadence keeps the table close to its hot-tier
    /// shape without making the sweeper hot. Operators can drop this
    /// for tests; production should leave the default.
    #[arg(
        long,
        env = "AUTH_AUDIT_RETENTION_CHECK_SECS",
        default_value = "3600"
    )]
    pub audit_retention_check_secs: u64,
}

/// True when `origin` is a CONCRETE, frameable-ancestor origin safe to splice
/// into a CSP `frame-ancestors` source-list: an exact `scheme://host[:port]`
/// with no wildcard and no CSP/header-breaking characters (design §6.2 "no
/// wildcards" + §4.3 fail-closed serialization).
///
/// Rejected (so they never widen the allowlist nor break the header value):
/// - empty / whitespace-only;
/// - any `*` (e.g. `https://*.zeroship.ai`, the bare `*`) — a wildcard would
///   re-admit every creator app and defeat the one-embedder property;
/// - CSP source-list / header-injecting bytes (whitespace inside the token,
///   `;`, `,`, control chars, non-ASCII);
/// - the CSP keyword forms (`'self'`, `'none'`, `data:`, `blob:`, …) — those are
///   not deployment-supplied ancestor origins (`'self'` is added by the builder
///   itself);
/// - anything without an explicit `http://` / `https://` scheme.
///
/// This is deliberately stricter than a full URL parse: it is an allowlist of
/// the exact shape we emit. A rejected entry is dropped at config-resolve, so
/// the live header builder only ever sees concrete origins.
#[must_use]
fn is_concrete_frame_ancestor_origin(origin: &str) -> bool {
    let o = origin.trim();
    if o.is_empty() {
        return false;
    }
    // Must be an explicit http(s) origin — not a CSP keyword/scheme source.
    let rest = match o.strip_prefix("https://").or_else(|| o.strip_prefix("http://")) {
        Some(rest) => rest,
        None => return false,
    };
    if rest.is_empty() {
        return false;
    }
    // No wildcard, no CSP-list / header-injecting bytes anywhere, ASCII-only.
    !o.chars().any(|c| {
        c == '*'
            || c == ';'
            || c == ','
            || c == ' '
            || c == '\t'
            || c.is_control()
            || !c.is_ascii()
    })
}

/// Conservative registrable-domain (eTLD+1) approximation for SameSite scoping.
///
/// We deliberately avoid a Public-Suffix-List dependency (offline-build
/// safety) and instead compute a FAIL-CLOSED registrable domain:
/// - a single-label host (e.g. `localhost`) is its own registrable domain;
/// - otherwise the registrable domain is the host's last two labels
///   (`auth.zeroship.ai` → `zeroship.ai`).
///
/// This is intentionally narrower than a true PSL eTLD+1 for exotic
/// multi-part public suffixes (`a.co.uk` → `co.uk` here, not `a.co.uk`): the
/// consequence is that the same-site guard is *stricter*, never *looser* —
/// it can only drop a legitimate origin, never admit a cross-site one. For
/// zeroship's `*.zeroship.ai` / `*.zeroship.localhost` topologies it is exact.
#[must_use]
fn registrable_domain(host: &str) -> String {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() <= 1 {
        return host;
    }
    let n = labels.len();
    format!("{}.{}", labels[n - 2], labels[n - 1])
}

/// True when a (concrete) frame-ancestor `origin` is SAME-SITE with the auth
/// `issuer_host` — i.e. shares the same registrable domain (eTLD+1).
///
/// SameSite cookie scoping ignores scheme and port and keys on the
/// registrable domain, so the `SameSite=Strict` `__Host-zsidp_csrf` cookie set
/// by the issuer reaches the in-frame POST only when the framing (console)
/// origin is same-site. Any non-same-site embedder must be dropped from the
/// `frame-ancestors` relax (login falls back to popup) — keeping it would
/// either silently break framed login (cookie withheld) or invite a
/// `Strict→None` downgrade that re-opens cross-site CSRF (review finding I7).
///
/// Fails closed: an unparseable/host-less candidate returns `false`.
#[must_use]
fn is_same_site_with_issuer(origin: &str, issuer_host: &str) -> bool {
    let Ok(url) = url::Url::parse(origin.trim()) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let issuer_host = issuer_host.trim();
    if issuer_host.is_empty() {
        return false;
    }
    registrable_domain(host) == registrable_domain(issuer_host)
}

impl AuthConfig {
    /// Resolve runtime state from the parsed CLI/env + the shared `[auth]`
    /// file overlay.
    ///
    /// Hydra URLs follow CLI/env > file > default precedence via the shared
    /// [`resolve_overlay_string`] primitive (same idiom as control). The
    /// dev-insecure flag resolves CLI presence > env > default-false: a CLI
    /// `--dev-insecure=false` overrides a stray `ZEROSHIP_DEV_INSECURE=1`.
    ///
    /// Under `--dev-insecure` an empty stash key falls back to the shared
    /// [`DEV_STASH_SIGNING_KEY`] in code (the clap default is empty so no
    /// secret leaks into `--help`); outside dev the empty key is rejected by
    /// `validate_stash_key` before this fallback would matter.
    pub fn resolve(&mut self, auth: AuthSection) {
        self.insecure_dev = self.dev_insecure.unwrap_or(false);
        if self.insecure_dev && self.stash_signing_key.is_empty() {
            self.stash_signing_key = DEV_STASH_SIGNING_KEY.to_string();
        }
        // TOTP at-rest key dev fallback (ISS-11), same posture as the stash key:
        // the clap default is empty (no secret in --help); under --dev-insecure
        // an empty value falls back to the shared dev sentinel. Outside dev the
        // empty key is rejected at boot by `validate_master_key_material`.
        if self.insecure_dev && self.totp_enc_key.is_empty() {
            self.totp_enc_key = DEV_TOTP_ENC_KEY.to_string();
        }
        // Console framing allowlist (immersive iframe login, §4.3/§10.1).
        // Precedence mirrors the deployment-injection pattern: a non-empty
        // CLI/env (`--frame-ancestor-origin` / `FRAME_ANCESTOR_ORIGINS`) wins;
        // otherwise fall back to the shared `[auth].frame_ancestor_origins`
        // file overlay; otherwise stay empty (relax is a no-op → strict
        // default). Empty CLI/env entries (a stray trailing comma) are dropped,
        // and NON-CONCRETE origins (wildcards, bad scheme, control chars) are
        // rejected here so a misconfiguration can never widen the
        // `frame-ancestors` allowlist (§6.2) nor produce an un-serializable CSP
        // header value (§4.3 `static_insert` fallback).
        // The auth issuer host the SameSite=Strict `__Host-zsidp_csrf` cookie is
        // scoped to. Every admitted frame-ancestor MUST be same-site (eTLD+1)
        // with it (review finding I7) — otherwise the Strict cookie is withheld
        // on the in-frame POST (framed login silently breaks) and the only
        // "fixes" are insecure (Strict→None re-opens cross-site CSRF). A
        // non-same-site origin is therefore DROPPED here so the relaxed
        // `frame-ancestors` never admits it; that deployment falls back to popup.
        let issuer_host = url::Url::parse(&self.public_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        let keep = |o: &String| {
            is_concrete_frame_ancestor_origin(o) && is_same_site_with_issuer(o, &issuer_host)
        };
        self.frame_ancestor_origins.retain(|o| keep(o));
        if self.frame_ancestor_origins.is_empty() {
            if let Some(origins) = auth.frame_ancestor_origins {
                self.frame_ancestor_origins =
                    origins.into_iter().filter(|o| keep(o)).collect();
            }
        }
        self.hydra_admin_url = Some(resolve_overlay_string(
            self.hydra_admin_url.take(),
            auth.hydra_admin_url,
            Some(DEFAULT_HYDRA_ADMIN_URL),
        ));
        self.hydra_public_url = Some(resolve_overlay_string(
            self.hydra_public_url.take(),
            auth.hydra_public_url,
            Some(DEFAULT_HYDRA_PUBLIC_URL),
        ));
    }

    /// Resolved Hydra admin API base URL.
    #[must_use]
    pub fn hydra_admin_url(&self) -> &str {
        self.hydra_admin_url
            .as_deref()
            .unwrap_or(DEFAULT_HYDRA_ADMIN_URL)
    }

    /// Resolved Hydra public issuer/base URL.
    #[must_use]
    pub fn hydra_public_url(&self) -> &str {
        self.hydra_public_url
            .as_deref()
            .unwrap_or(DEFAULT_HYDRA_PUBLIC_URL)
    }

    /// External origin of this auth server (no trailing slash). Returns
    /// [`Self::public_url`] with any trailing `/` trimmed so callers can
    /// freely concatenate `/magic/verify?…`.
    #[must_use]
    pub fn public_url(&self) -> String {
        self.public_url.trim_end_matches('/').to_string()
    }
}

impl std::fmt::Debug for AuthConfig {
    /// Hand-written so secrets (DSN, stash/OAuth/SMTP/Resend keys, webhook
    /// creds) never appear in `{:?}` output. The derive would print raw
    /// `String` secrets in plaintext (S2), so it is deliberately absent.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("addr", &self.addr)
            .field("db_url", &"<redacted>")
            .field("hydra_admin_url", &self.hydra_admin_url)
            .field("hydra_public_url", &self.hydra_public_url)
            .field("allow_remote_hydra_admin", &self.allow_remote_hydra_admin)
            .field("clients_config", &self.clients_config)
            .field("bootstrap", &self.bootstrap)
            .field("dev_insecure", &self.dev_insecure)
            .field("insecure_dev", &self.insecure_dev)
            .field("stash_signing_key", &"<redacted>")
            .field("totp_enc_key", &"<redacted>")
            .field("frame_ancestor_origins", &self.frame_ancestor_origins)
            .field("google_client_id", &self.google_client_id)
            .field("google_client_secret", &"<redacted>")
            .field("github_client_id", &self.github_client_id)
            .field("github_client_secret", &"<redacted>")
            .field("mailer", &self.mailer)
            .field("smtp_password", &"<redacted>")
            .field("resend_api_key", &"<redacted>")
            .field("public_url", &self.public_url)
            .field("postmark_webhook_password", &"<redacted>")
            .field("relay_domain", &self.relay_domain)
            .field("relay_forward_mailer", &self.relay_forward_mailer)
            .field("relay_inbound_password", &"<redacted>")
            .field("relay_smtp_password", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use zeroship_core::config::FileConfig;

    static HYDRA_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn write(name: &str, contents: &str) -> Self {
            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

            let path = std::env::temp_dir().join(format!(
                "zeroship-auth-config-{name}-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(path.as_path(), contents).expect("write temp config");
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.path.as_path());
        }
    }

    fn set_env_opt(key: &str, value: Option<&str>) {
        if let Some(value) = value {
            std::env::set_var(key, value);
        } else {
            std::env::remove_var(key);
        }
    }

    fn restore_env(key: &str, value: Option<OsString>) {
        if let Some(value) = value {
            std::env::set_var(key, value);
        } else {
            std::env::remove_var(key);
        }
    }

    fn with_hydra_env<T>(
        hydra_admin_url: Option<&str>,
        hydra_public_url: Option<&str>,
        f: impl FnOnce() -> T,
    ) -> T {
        let _guard = HYDRA_ENV_LOCK.lock().expect("hydra env lock poisoned");
        let old_admin = std::env::var_os("HYDRA_ADMIN_URL");
        let old_public = std::env::var_os("HYDRA_PUBLIC_URL");

        set_env_opt("HYDRA_ADMIN_URL", hydra_admin_url);
        set_env_opt("HYDRA_PUBLIC_URL", hydra_public_url);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        restore_env("HYDRA_ADMIN_URL", old_admin);
        restore_env("HYDRA_PUBLIC_URL", old_public);

        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn resolve_from_file(mut cfg: AuthConfig) -> AuthConfig {
        let file = FileConfig::load(cfg.config_path.as_deref()).expect("load config file");
        cfg.resolve(file.auth);
        cfg
    }

    fn test_config() -> AuthConfig {
        AuthConfig::parse_from(["zeroship-auth", "--db-url", "postgres://test"])
    }

    // The stash validator now lives in `zeroship_core::config`; these tests
    // assert auth's resolved key + flag interact with it correctly (S4).
    // `DEV_STASH_SIGNING_KEY` is already in scope via `use super::*`.
    use zeroship_core::config::validate_stash_key;

    #[test]
    fn stash_key_empty_default_rejected_in_production() {
        let cfg = test_config();
        // Empty default (no secret printed in --help) is rejected outside dev.
        assert!(validate_stash_key(&cfg.stash_signing_key, cfg.insecure_dev).is_err());
    }

    #[test]
    fn stash_key_empty_default_accepted_in_dev() {
        let mut cfg = test_config();
        cfg.insecure_dev = true;

        assert!(validate_stash_key(&cfg.stash_signing_key, cfg.insecure_dev).is_ok());
    }

    #[test]
    fn stash_key_short_rejected_in_production() {
        let mut cfg = test_config();
        cfg.stash_signing_key = "a".repeat(20);

        assert!(validate_stash_key(&cfg.stash_signing_key, cfg.insecure_dev).is_err());
    }

    #[test]
    fn stash_key_strong_accepted_in_production() {
        let mut cfg = test_config();
        cfg.stash_signing_key = "0123456789abcdef0123456789abcdef".to_string();

        assert!(validate_stash_key(&cfg.stash_signing_key, cfg.insecure_dev).is_ok());
    }

    // (d) The dev stash default is no longer a clap `default_value`, so it
    // cannot leak in `--help`. The empty default must still be rejected by the
    // shared validator outside dev.
    #[test]
    fn stash_key_default_is_empty_not_dev_sentinel() {
        let cfg = test_config();
        assert_eq!(cfg.stash_signing_key, "");
        assert_ne!(cfg.stash_signing_key, DEV_STASH_SIGNING_KEY);
        // The empty default is rejected outside dev (no clap default secret).
        assert!(validate_stash_key(&cfg.stash_signing_key, false).is_err());
    }

    // (a) `--dev-insecure` (bare) and `ZEROSHIP_DEV_INSECURE` both enable the
    // resolved flag, and a CLI `--dev-insecure=false` overrides a stray env=1.
    #[test]
    fn dev_insecure_cli_bare_enables() {
        let mut cfg =
            AuthConfig::parse_from(["zeroship-auth", "--db-url", "postgres://test", "--dev-insecure"]);
        cfg.resolve(AuthSection::default());
        assert!(cfg.insecure_dev);
    }

    #[test]
    fn dev_insecure_cli_explicit_true_enables() {
        let mut cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--dev-insecure=true",
        ]);
        cfg.resolve(AuthSection::default());
        assert!(cfg.insecure_dev);
    }

    #[test]
    fn dev_insecure_env_enables_and_cli_false_overrides() {
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().expect("env lock");
        let old = std::env::var_os("ZEROSHIP_DEV_INSECURE");
        std::env::set_var("ZEROSHIP_DEV_INSECURE", "1");

        // env=1 alone enables.
        let mut cfg = AuthConfig::parse_from(["zeroship-auth", "--db-url", "postgres://test"]);
        cfg.resolve(AuthSection::default());
        assert!(cfg.insecure_dev, "ZEROSHIP_DEV_INSECURE=1 should enable");

        // CLI presence overrides env: --dev-insecure=false disables it.
        let mut cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--dev-insecure=false",
        ]);
        cfg.resolve(AuthSection::default());
        assert!(
            !cfg.insecure_dev,
            "--dev-insecure=false must override ZEROSHIP_DEV_INSECURE=1"
        );

        restore_env("ZEROSHIP_DEV_INSECURE", old);
    }

    // (b) The old `--insecure-dev` / `AUTH_INSECURE_DEV` flag is gone.
    #[test]
    fn old_insecure_dev_flag_is_rejected() {
        let err = AuthConfig::try_parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--insecure-dev",
        ])
        .expect_err("--insecure-dev no longer accepted");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    // ─── SMTP TLS mode (Bug 2 regression) ────────────────────────────────
    // The old `--smtp-starttls`/`--relay-smtp-starttls` were bare `bool` flags
    // with `default_value = "true"`: passing a value (`=false`) errored and the
    // bare flag could only ever set `true`, so there was NO way to disable TLS
    // on the command line (only via env). These value-enum flags fix that AND
    // add the previously-missing `plaintext` arm for dev/test sinks.

    #[test]
    fn smtp_tls_defaults_to_starttls() {
        let cfg = test_config();
        assert_eq!(cfg.smtp_tls, SmtpTls::Starttls);
        assert_eq!(cfg.relay_smtp_tls, SmtpTls::Starttls);
    }

    #[test]
    fn smtp_tls_accepts_plaintext_value_on_cli() {
        // Pre-fix `--smtp-starttls=false` errored ("unexpected value"); the
        // valued enum parses an explicit mode, including the new plaintext arm.
        let cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--smtp-tls",
            "plaintext",
            "--relay-smtp-tls",
            "plaintext",
        ]);
        assert_eq!(cfg.smtp_tls, SmtpTls::Plaintext);
        assert_eq!(cfg.relay_smtp_tls, SmtpTls::Plaintext);
    }

    #[test]
    fn smtp_tls_accepts_implicit_and_starttls_values() {
        let cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--smtp-tls=implicit",
            "--relay-smtp-tls=starttls",
        ]);
        assert_eq!(cfg.smtp_tls, SmtpTls::Implicit);
        assert_eq!(cfg.relay_smtp_tls, SmtpTls::Starttls);
    }

    #[test]
    fn smtp_tls_rejects_unknown_mode() {
        let err = AuthConfig::try_parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--smtp-tls",
            "nope",
        ])
        .expect_err("unknown smtp-tls mode must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn old_smtp_starttls_flag_is_rejected() {
        // No back-compat: the bare bool flag is gone, replaced by `--smtp-tls`.
        let err = AuthConfig::try_parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--smtp-starttls",
        ])
        .expect_err("--smtp-starttls no longer accepted");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn allow_remote_hydra_admin_flag_accepts_bare_switch() {
        let cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--allow-remote-hydra-admin",
        ]);

        assert!(cfg.allow_remote_hydra_admin);
    }

    #[test]
    fn allow_remote_hydra_admin_flag_accepts_true_value() {
        let cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--allow-remote-hydra-admin=true",
        ]);

        assert!(cfg.allow_remote_hydra_admin);
    }

    // Immersive iframe login (design §4.3/§10.1): the `--frame-ancestor-origin`
    // CLI flag is comma-splittable and repeatable, and a non-empty CLI value
    // WINS over the `[auth].frame_ancestor_origins` overlay; an empty CLI falls
    // back to the overlay; empty entries (a stray trailing comma) are dropped.
    #[test]
    fn frame_ancestor_origins_cli_comma_split_and_repeatable() {
        let cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--frame-ancestor-origin",
            "https://console.zeroship.ai,https://staging.zeroship.ai",
            "--frame-ancestor-origin",
            "https://preview.zeroship.ai",
        ]);
        assert_eq!(
            cfg.frame_ancestor_origins,
            vec![
                "https://console.zeroship.ai".to_string(),
                "https://staging.zeroship.ai".to_string(),
                "https://preview.zeroship.ai".to_string(),
            ]
        );
    }

    #[test]
    fn frame_ancestor_origins_default_empty() {
        let cfg = test_config();
        assert!(cfg.frame_ancestor_origins.is_empty());
    }

    #[test]
    fn frame_ancestor_origins_cli_wins_over_overlay() {
        let mut cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            // Issuer same-site with the test origins so the I7 guard is a no-op
            // here; this test isolates CLI-vs-overlay precedence.
            "--public-url",
            "https://auth.zeroship.ai",
            "--frame-ancestor-origin",
            "https://console.zeroship.ai",
        ]);
        cfg.resolve(AuthSection {
            frame_ancestor_origins: Some(vec!["https://overlay.zeroship.ai".to_string()]),
            ..AuthSection::default()
        });
        assert_eq!(
            cfg.frame_ancestor_origins,
            vec!["https://console.zeroship.ai".to_string()],
            "a non-empty CLI/env value must win over the [auth] overlay"
        );
    }

    // §6.2 "no wildcards": a misconfigured `*` / `https://*.zeroship.ai` (or any
    // non-concrete origin) must be REJECTED at config-resolve, so it can never
    // reach the `frame-ancestors` builder and re-admit every creator app, and so
    // `static_insert` can never hit its un-serializable fallback (§4.3).
    #[test]
    fn frame_ancestor_origins_rejects_wildcards_and_non_concrete() {
        let mut cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            // Same-site issuer so this test isolates the wildcard/non-concrete
            // rejection (not the I7 same-site drop).
            "--public-url",
            "https://auth.zeroship.ai",
            "--frame-ancestor-origin",
            // Mixed: one valid origin + several poison entries.
            "https://console.zeroship.ai,https://*.zeroship.ai,*,'self',data:,\
             ftp://console.zeroship.ai,https://a b.zeroship.ai,console.zeroship.ai",
        ]);
        cfg.resolve(AuthSection::default());
        assert_eq!(
            cfg.frame_ancestor_origins,
            vec!["https://console.zeroship.ai".to_string()],
            "only the concrete http(s) origin survives; wildcards / keywords / \
             bad schemes / space-bearing tokens are dropped"
        );
    }

    #[test]
    fn frame_ancestor_origins_rejects_wildcards_from_overlay_too() {
        let mut cfg = test_config(); // no CLI value → overlay path
        // Same-site issuer so this test isolates wildcard/keyword rejection.
        cfg.public_url = "https://auth.zeroship.ai".to_string();
        cfg.resolve(AuthSection {
            frame_ancestor_origins: Some(vec![
                "https://*.zeroship.ai".to_string(), // wildcard — dropped
                "https://console.zeroship.ai".to_string(), // kept
                "'none'".to_string(),                // CSP keyword — dropped
            ]),
            ..AuthSection::default()
        });
        assert_eq!(
            cfg.frame_ancestor_origins,
            vec!["https://console.zeroship.ai".to_string()],
            "the overlay path applies the same wildcard/keyword rejection"
        );
    }

    #[test]
    fn concrete_frame_ancestor_origin_predicate() {
        // Accept exact http(s) origins (with/without port).
        assert!(is_concrete_frame_ancestor_origin("https://console.zeroship.ai"));
        assert!(is_concrete_frame_ancestor_origin(
            "https://console.zeroship.localhost:8443"
        ));
        assert!(is_concrete_frame_ancestor_origin("http://localhost:5173"));
        // Reject every non-concrete / poison form.
        for bad in [
            "",
            "   ",
            "*",
            "https://*.zeroship.ai",
            "'self'",
            "'none'",
            "data:",
            "console.zeroship.ai",                // no scheme
            "ftp://console.zeroship.ai",          // wrong scheme
            "https://a.zeroship.ai https://b.ai", // embedded space (two sources)
            "https://a.zeroship.ai;script-src *", // CSP injection
            "https://a.zeroship.ai,https://b.ai", // comma (list)
        ] {
            assert!(
                !is_concrete_frame_ancestor_origin(bad),
                "{bad:?} must be rejected as a frame-ancestor origin"
            );
        }
    }

    #[test]
    fn frame_ancestor_origins_overlay_used_when_cli_empty() {
        let mut cfg = test_config(); // no --frame-ancestor-origin
        // Same-site issuer so this test isolates the empty-CLI→overlay fallback.
        cfg.public_url = "https://auth.zeroship.ai".to_string();
        cfg.resolve(AuthSection {
            frame_ancestor_origins: Some(vec![
                "https://overlay.zeroship.ai".to_string(),
                String::new(), // dropped
            ]),
            ..AuthSection::default()
        });
        assert_eq!(
            cfg.frame_ancestor_origins,
            vec!["https://overlay.zeroship.ai".to_string()],
            "an empty CLI value must fall back to the [auth] overlay (empties dropped)"
        );
    }

    // I7 (latent guard): the SameSite=Strict `__Host-zsidp_csrf` cookie reaches
    // the in-frame POST only when the framing (console) origin is SAME-SITE
    // (same registrable domain / eTLD+1) with the auth issuer host. A
    // cross-registrable-domain console silently breaks framed login (cookie
    // withheld) and tempts a `Strict→None` downgrade that re-opens cross-site
    // CSRF. The resolve guard MUST drop any frame-ancestor origin that is not
    // same-site with `public_url`'s host, so the relaxed `frame-ancestors` never
    // admits a non-same-site embedder (it falls back to popup / strict default).
    #[test]
    fn frame_ancestor_origins_drops_non_same_site_with_issuer() {
        let mut cfg = test_config();
        // Concrete prod issuer host: registrable domain `zeroship.ai`.
        cfg.public_url = "https://auth.zeroship.ai".to_string();
        cfg.resolve(AuthSection {
            frame_ancestor_origins: Some(vec![
                // Same-site with the issuer (same registrable domain) — KEPT.
                "https://console.zeroship.ai".to_string(),
                // Concrete + wildcard-free, but a DIFFERENT registrable domain —
                // NOT same-site, so the Strict CSRF cookie would be withheld in
                // the frame. MUST be dropped.
                "https://console.zeroship-eu.com".to_string(),
                // A look-alike suffix that merely *contains* the issuer domain as
                // a substring but is a different registrable domain — dropped.
                "https://console.zeroship.ai.evil.com".to_string(),
            ]),
            ..AuthSection::default()
        });
        assert_eq!(
            cfg.frame_ancestor_origins,
            vec!["https://console.zeroship.ai".to_string()],
            "only the origin same-site (eTLD+1) with the auth issuer survives; \
             cross-registrable-domain consoles are dropped (relax → popup)"
        );
    }

    // The same-site guard also applies on the CLI path (CLI value wins over the
    // overlay but is still subject to the same-site drop).
    #[test]
    fn frame_ancestor_origins_cli_drops_non_same_site() {
        let mut cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--public-url",
            "https://auth.zeroship.ai",
            "--frame-ancestor-origin",
            "https://console.zeroship.ai,https://attacker.example.com",
        ]);
        cfg.resolve(AuthSection::default());
        assert_eq!(
            cfg.frame_ancestor_origins,
            vec!["https://console.zeroship.ai".to_string()],
            "a non-same-site CLI origin is dropped by the issuer-same-site guard"
        );
    }

    // Dev loopback: issuer host `localhost`; a `localhost` console (any port) is
    // same-site, but a real registrable-domain console is not.
    #[test]
    fn frame_ancestor_origins_loopback_issuer_keeps_localhost_only() {
        let mut cfg = test_config(); // public_url defaults to http://localhost:9092
        cfg.resolve(AuthSection {
            frame_ancestor_origins: Some(vec![
                "http://localhost:5173".to_string(),         // same-site (localhost)
                "https://console.zeroship.ai".to_string(),   // not same-site
            ]),
            ..AuthSection::default()
        });
        assert_eq!(
            cfg.frame_ancestor_origins,
            vec!["http://localhost:5173".to_string()],
            "with a loopback issuer only localhost consoles are same-site"
        );
    }

    #[test]
    fn same_site_predicate_matches_real_topologies() {
        // Same registrable domain → same-site.
        assert!(is_same_site_with_issuer(
            "https://console.zeroship.ai",
            "auth.zeroship.ai"
        ));
        assert!(is_same_site_with_issuer(
            "https://console.zeroship.ai",
            "zeroship.ai"
        ));
        assert!(is_same_site_with_issuer(
            "https://auth.zeroship.ai",
            "auth.zeroship.ai"
        ));
        // Multi-label dev suffix.
        assert!(is_same_site_with_issuer(
            "https://console.zeroship.localhost:8443",
            "auth.zeroship.localhost"
        ));
        // Loopback issuer: only localhost consoles.
        assert!(is_same_site_with_issuer("http://localhost:5173", "localhost"));
        assert!(!is_same_site_with_issuer(
            "https://console.zeroship.ai",
            "localhost"
        ));
        // Different registrable domains → NOT same-site (fail closed).
        assert!(!is_same_site_with_issuer(
            "https://console.zeroship-eu.com",
            "auth.zeroship.ai"
        ));
        // Suffix-substring look-alike must NOT pass (must match on label
        // boundary, not substring).
        assert!(!is_same_site_with_issuer(
            "https://console.zeroship.ai.evil.com",
            "auth.zeroship.ai"
        ));
        assert!(!is_same_site_with_issuer(
            "https://evilzeroship.ai",
            "auth.zeroship.ai"
        ));
        // Unparseable / non-origin candidate fails closed.
        assert!(!is_same_site_with_issuer("not-a-url", "auth.zeroship.ai"));
    }

    #[test]
    fn auth_file_overlay_supplies_hydra_urls_when_unset() {
        with_hydra_env(None, None, || {
            let file = TempFile::write(
                "auth-overlay.toml",
                r#"
[auth]
hydra_admin_url = "http://hydra-file:4445"
hydra_public_url = "https://hydra-file.example"
"#,
            );

            let cfg = resolve_from_file(AuthConfig::parse_from([
                "zeroship-auth",
                "--db-url",
                "postgres://test",
                "--config",
                file.path.to_str().expect("utf-8 temp path"),
            ]));

            assert_eq!(cfg.hydra_admin_url(), "http://hydra-file:4445");
            assert_eq!(cfg.hydra_public_url(), "https://hydra-file.example");
        });
    }

    #[test]
    fn cli_hydra_urls_override_file_overlay() {
        with_hydra_env(None, None, || {
            let file = TempFile::write(
                "auth-cli-override.toml",
                r#"
[auth]
hydra_admin_url = "http://hydra-file:4445"
hydra_public_url = "https://hydra-file.example"
"#,
            );

            let cfg = resolve_from_file(AuthConfig::parse_from([
                "zeroship-auth",
                "--db-url",
                "postgres://test",
                "--config",
                file.path.to_str().expect("utf-8 temp path"),
                "--hydra-admin-url",
                "http://hydra-cli:4445",
                "--hydra-public-url",
                "https://hydra-cli.example",
            ]));

            assert_eq!(cfg.hydra_admin_url(), "http://hydra-cli:4445");
            assert_eq!(cfg.hydra_public_url(), "https://hydra-cli.example");
        });
    }

    #[test]
    fn env_hydra_urls_override_file_overlay() {
        with_hydra_env(
            Some("http://hydra-env:4445"),
            Some("https://hydra-env.example"),
            || {
                let file = TempFile::write(
                    "auth-env-override.toml",
                    r#"
[auth]
hydra_admin_url = "http://hydra-file:4445"
hydra_public_url = "https://hydra-file.example"
"#,
                );

                let cfg = resolve_from_file(AuthConfig::parse_from([
                    "zeroship-auth",
                    "--db-url",
                    "postgres://test",
                    "--config",
                    file.path.to_str().expect("utf-8 temp path"),
                ]));

                assert_eq!(cfg.hydra_admin_url(), "http://hydra-env:4445");
                assert_eq!(cfg.hydra_public_url(), "https://hydra-env.example");
            },
        );
    }
}
