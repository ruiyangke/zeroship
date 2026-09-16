//! Auth server configuration.
//!
//! Three types, one job each:
//!
//! * [`AuthSettings`] is the generated declaration. Every value the auth service
//!   resolves - operational, secret and command control alike - is spelled ONCE
//!   here, as a canonical name; its flag, its `ZEROSHIP_AUTH_*` environment name
//!   and its overlay path are projected from that name rather than written out
//!   again. A `Secret<T>` field generates a `-file` PATH flag and nothing else,
//!   so no credential can travel through this process's argument vector.
//! * [`AuthCli`] is the command line: the generated carrier under the binary's
//!   own command name.
//! * [`AuthConfig`] is the RESOLVED configuration the rest of the crate reads.
//!   It exists because resolution is not purely mechanical here: the
//!   frame-ancestor allowlist is filtered fail-closed against the issuer host
//!   before anything can read it.

use std::path::{Path, PathBuf};

use clap::Parser;
use zeroship_core::config::{
    zeroship_config, AuthProviderKind, BootstrapControl, CheckFormat, CommandControl,
    ConfigResolveError, GeneratedConfig, ObservabilityControls, Operational, OverlaySelector,
    Secret,
};
use zeroship_core::observability::LogFormat;
use zeroship_mailer::SmtpTls;

/// Tracing directive applied when nothing supplies `observability.log_filter`.
pub const DEFAULT_LOG_FILTER: &str = "info,zeroship_auth=debug";

/// Every operational value an auth-service launch resolves, plus the bootstrap
/// and command controls the shared boot dance needs.
#[zeroship_config(binary = "zeroship-auth", scope = "auth")]
#[derive(Debug, Clone)]
pub struct AuthSettings {
    /// Optional shared config overlay path.
    #[config(shared = CONFIG)]
    pub config: BootstrapControl<Option<PathBuf>>,

    /// Disable well-known config auto-discovery (`/etc/zeroship/zeroship.toml`).
    #[config(shared = NO_CONFIG)]
    pub no_config: BootstrapControl<bool>,

    /// Validate config (CLI + overlay + guards) and print the resolved non-secret
    /// config, then exit without starting the server.
    #[config(shared = CHECK_CONFIG)]
    pub check_config: CommandControl<bool>,

    /// Output format for `--check-config`.
    #[config(shared = CHECK_CONFIG_FORMAT, default = CheckFormat::Text)]
    pub check_config_format: CommandControl<CheckFormat>,

    /// `EnvFilter` directive for the tracing subscriber.
    #[config(shared = OBSERVABILITY_LOG_FILTER, default = DEFAULT_LOG_FILTER.to_owned())]
    pub log_filter: Operational<String>,

    /// Tracing output format; `auto` picks pretty on a TTY and json otherwise.
    #[config(shared = OBSERVABILITY_LOG_FORMAT, default = LogFormat::Auto)]
    pub log_format: Operational<LogFormat>,

    /// Listen address. Defaults to loopback; compose passes `0.0.0.0:9092`.
    #[config(name = "auth.addr", default = "127.0.0.1:9092".to_owned())]
    pub addr: Operational<String>,

    /// `PostgreSQL` DSN for the auth service.
    ///
    /// Secret-classed on its GRAMMAR, not on whether a given value happens to
    /// carry a password: a DSN admits userinfo, so one spelling of it must be
    /// treated as credential-bearing everywhere.
    #[config(name = "auth.database_url")]
    pub database_url: Secret<String>,

    /// Platform auth provider backend.
    ///
    /// The canonical name is `auth.provider`, NOT `auth.auth_provider`: the
    /// scope already says `auth`, and the stuttering form would project to
    /// `ZEROSHIP_AUTH_AUTH_PROVIDER`.
    ///
    /// SHARED with control, which verifies the tokens this service's choice
    /// causes to be issued. It reads the same `ZEROSHIP_AUTH_PROVIDER` and the
    /// same `AuthProviderKind`; auth serves the provider, control trusts its
    /// issuer, and neither restates the vocabulary.
    #[arg(value_enum)]
    #[config(shared = AUTH_PROVIDER, default = AuthProviderKind::Native)]
    pub provider: Operational<AuthProviderKind>,

    /// Supabase Auth / GoTrue base URL used when `--provider=supabase`.
    /// Empty means unset.
    #[config(shared = AUTH_SUPABASE_URL, default = String::new())]
    pub supabase_url: Operational<String>,

    /// Supabase anon API key used by the browser-side GoTrue login. Empty means
    /// unset.
    ///
    /// OPERATIONAL, not `Secret<T>`, and that is a deliberate classification:
    /// this key is served to every browser that loads the GoTrue login, so it is
    /// published by design. The tracked ops overlay already carries it under
    /// `[auth]` rather than `[secrets]`. The visible consequence is that
    /// `--check-config` now prints whether it is set AND its value, like any
    /// other operational value.
    #[config(shared = AUTH_SUPABASE_ANON_KEY, default = String::new())]
    pub supabase_anon_key: Operational<String>,

    /// Standard-Webhooks symmetric secret for `GoTrue`'s Send Email hook.
    ///
    /// Set this to the same `v1,whsec_<base64>` value supplied to `GoTrue` via
    /// `GOTRUE_HOOK_SEND_EMAIL_SECRETS`. When unset, the hook endpoint
    /// fail-closes with 401.
    #[config(name = "auth.gotrue_email_hook_secret")]
    pub gotrue_email_hook_secret: Secret<String>,

    /// Control-plane base URL used by browser-mediated auth flows.
    #[config(shared = CONTROL_URL, default = "http://localhost:9090".to_owned())]
    pub control_url: Operational<String>,

    /// PEM/DER file holding this service's own ed25519 private key.
    ///
    /// The credential the auth service presents to the control plane, and the
    /// ONLY one it holds for that hop. One call needs it today: the erasure
    /// preflight (`crate::control_client::erasure_preflight`), which asks the
    /// control plane whether a human is the last owner of an organization -- a
    /// question the auth service cannot answer for itself, because
    /// `zeroship_auth` holds no privilege on any organization table.
    ///
    /// NOT the shared control key: that key is one identity four other
    /// processes hold, so presenting it would make the process that renders the
    /// login form indistinguishable from the gateway and the worker at
    /// control's door -- and would hand it the route table, the version feed
    /// and both reconcile triggers along with the one route it needs. An
    /// assertion names `svc/auth` under a key only this process holds, and
    /// control's allowlist decides what that reaches.
    ///
    /// Empty REFUSES THE BOOT, in `ServiceKeyring::load`. An auth service that
    /// started without it would answer every liveness probe while every account
    /// deletion failed its precondition check.
    #[config(name = "auth.service_key_file", default = PathBuf::new())]
    pub service_key_file: Operational<PathBuf>,

    /// JWKS-shaped FILE holding the public key of every peer service.
    ///
    /// One document is handed to every service. `crates/zeroship-core/src/service_peers.rs`
    /// carries the shape, why the keys are configured rather than fetched from
    /// a peer, and why a shared document grants nothing beyond the ability to
    /// check a signature.
    #[config(name = "auth.service_peers_file", default = PathBuf::new())]
    pub service_peers_file: Operational<PathBuf>,

    /// Audience fixed onto access tokens minted for the platform CLI.
    #[config(shared = OAUTH_AUDIENCE, default = "control.zeroship.ai".to_owned())]
    pub oauth_audience: Operational<String>,


    /// HMAC key (>=32 bytes) used to sign the short-lived federation stash
    /// cookie (`__Host-zsidp_google_stash` etc.). A weak or absent value lets an
    /// attacker forge stash cookies and bypass the `OAuth` state/PKCE check, so
    /// production deployments MUST set it. There is no compiled default - a
    /// secret cannot have one - and the startup strength guard rejects an unset
    /// key with its own message.
    #[config(name = "auth.stash_signing_key")]
    pub stash_signing_key: Secret<String>,

    /// AES-256-GCM key material for encrypting the TOTP shared secret at rest.
    /// MUST decode (hex or base64url) to >=32 bytes - validated at
    /// boot by `validate_master_key_material`, identical to the bundle/master
    /// key posture. A weak or absent key means an attacker with DB read access
    /// recovers every user's TOTP seed and can mint valid codes, so production
    /// MUST set it. The encryption AAD binds the row's `user_id`, so a
    /// ciphertext lifted onto another user's row fails to decrypt.
    ///
    /// OPERATOR NOTE: this is a DEDICATED key, NOT derived from the
    /// stash/pairwise secrets, so 2FA seeds rotate independently of the
    /// session-signing material. Rotating it without re-encrypting existing
    /// `totp_credentials` rows invalidates every enrolled secret (users must
    /// re-enroll).
    #[config(name = "auth.totp_enc_key")]
    pub totp_enc_key: Secret<String>,

    /// Console origin(s) allowed to FRAME the login/signup/consent documents.
    ///
    /// RAW, as supplied. Read [`AuthConfig::frame_ancestor_origins`] instead:
    /// that is the list after the fail-closed concrete + same-site filter, and
    /// it is the only one safe to splice into a CSP header. See the filter's
    /// documentation on [`is_concrete_frame_ancestor_origin`] and
    /// [`is_same_site_with_issuer`].
    #[arg(value_delimiter = ',')]
    #[config(name = "auth.frame_ancestor_origins", default = Vec::new())]
    pub frame_ancestor_origins: Operational<Vec<String>>,

    /// Google OAuth 2.0 client ID. Empty means the `/oauth/google/*` routes are
    /// not registered (auth still boots).
    #[config(name = "auth.google_client_id", default = String::new())]
    pub google_client_id: Operational<String>,

    /// Google OAuth 2.0 client secret. Unset leaves the Google arm disabled.
    #[config(name = "auth.google_client_secret")]
    pub google_client_secret: Secret<String>,

    /// Redirect URI registered with Google. Must match the value configured in
    /// the Google Cloud Console exactly.
    #[config(
        name = "auth.google_redirect_uri",
        default = "https://auth.zeroship.ai/oauth/google/callback".to_owned()
    )]
    pub google_redirect_uri: Operational<String>,

    /// Google's authorize endpoint. Overridable so the e2e tests can point
    /// at an in-process `tests/common/mock_provider` instead of the real
    /// Google. Production deployments should leave the default in place.
    #[config(
        name = "auth.google_auth_url",
        default = "https://accounts.google.com/o/oauth2/v2/auth".to_owned()
    )]
    pub google_auth_url: Operational<String>,

    /// Google's token endpoint. Overridable for tests; production leaves
    /// the default.
    #[config(
        name = "auth.google_token_url",
        default = "https://oauth2.googleapis.com/token".to_owned()
    )]
    pub google_token_url: Operational<String>,

    /// Google's JWKS endpoint. Overridable for tests; production leaves
    /// the default.
    #[config(
        name = "auth.google_jwks_url",
        default = "https://www.googleapis.com/oauth2/v3/certs".to_owned()
    )]
    pub google_jwks_url: Operational<String>,

    /// Expected `iss` claim on Google ID tokens. Overridable for tests
    /// (mock provider uses its own loopback base URL); production leaves
    /// the default - Google emits this exact string per its OIDC
    /// discovery document.
    #[config(
        name = "auth.google_issuer",
        default = "https://accounts.google.com".to_owned()
    )]
    pub google_issuer: Operational<String>,

    /// GitHub OAuth App client ID. Empty means the `/oauth/github/*` routes are
    /// not registered (auth still boots).
    #[config(name = "auth.github_client_id", default = String::new())]
    pub github_client_id: Operational<String>,

    /// GitHub OAuth App client secret. Unset leaves the GitHub arm disabled.
    #[config(name = "auth.github_client_secret")]
    pub github_client_secret: Secret<String>,

    /// Callback URL registered on the GitHub OAuth App. Must match what's set
    /// in the app's settings.
    #[config(
        name = "auth.github_redirect_uri",
        default = "https://auth.zeroship.ai/oauth/github/callback".to_owned()
    )]
    pub github_redirect_uri: Operational<String>,

    /// GitHub's authorize endpoint. Overridable for the federation e2e
    /// tests; production deployments leave the default in place.
    #[config(
        name = "auth.github_authorize_url",
        default = "https://github.com/login/oauth/authorize".to_owned()
    )]
    pub github_authorize_url: Operational<String>,

    /// GitHub's token endpoint. Overridable for tests.
    #[config(
        name = "auth.github_token_url",
        default = "https://github.com/login/oauth/access_token".to_owned()
    )]
    pub github_token_url: Operational<String>,

    /// GitHub's `/user` endpoint. Overridable for tests.
    #[config(
        name = "auth.github_user_url",
        default = "https://api.github.com/user".to_owned()
    )]
    pub github_user_url: Operational<String>,

    /// GitHub's `/user/emails` endpoint. Overridable for tests.
    #[config(
        name = "auth.github_emails_url",
        default = "https://api.github.com/user/emails".to_owned()
    )]
    pub github_emails_url: Operational<String>,

    /// Mailer driver: `stdout` (dev default) | `smtp` | `resend`.
    #[config(name = "auth.mailer", default = "stdout".to_owned())]
    pub mailer: Operational<String>,

    /// Resend API key (required when `--mailer=resend`).
    #[config(name = "auth.resend_api_key")]
    pub resend_api_key: Secret<String>,

    /// SMTP relay hostname (required when `--mailer=smtp`). Empty means unset.
    #[config(name = "auth.smtp_host", default = String::new())]
    pub smtp_host: Operational<String>,

    /// SMTP port. Defaults to 587 (STARTTLS); use 465 for implicit SMTPS.
    #[config(name = "auth.smtp_port", default = 587)]
    pub smtp_port: Operational<u16>,

    /// SMTP username (optional - server may allow unauthenticated relays).
    /// Empty means unset.
    #[config(name = "auth.smtp_username", default = String::new())]
    pub smtp_username: Operational<String>,

    /// SMTP password (paired with `--smtp-username`).
    #[config(name = "auth.smtp_password")]
    pub smtp_password: Secret<String>,

    /// Transport encryption for the transactional SMTP leg:
    /// `starttls` (default, port 587) | `implicit` (SMTPS, port 465) |
    /// `plaintext` (no TLS - dev/test sinks like mailpit on :1025 ONLY).
    #[arg(value_enum)]
    #[config(name = "auth.smtp_tls", default = SmtpTls::Starttls)]
    pub smtp_tls: Operational<SmtpTls>,

    /// `From` address every transactional mail uses.
    #[config(name = "auth.mail_from_email", default = "auth@zeroship.ai".to_owned())]
    pub mail_from_email: Operational<String>,

    /// `From` display name every transactional mail uses.
    #[config(name = "auth.mail_from_name", default = "zeroship".to_owned())]
    pub mail_from_name: Operational<String>,

    /// Public, externally-reachable origin of this auth server. Used when
    /// constructing absolute URLs embedded in outbound email (e.g. the
    /// magic-link href). Distinct from [`Self::addr`] - that's the bind
    /// address (`0.0.0.0:9092` in prod, which is NOT a real origin).
    ///
    /// Defaults to the dev-loopback value; production deployments MUST
    /// override with the auth host, including scheme + (optional) port.
    /// Read it through [`AuthConfig::public_url`], which trims a trailing `/`.
    #[config(name = "auth.public_url", default = "http://localhost:9092".to_owned())]
    pub public_url: Operational<String>,

    /// Maximum dedicated refresh-family database sessions per auth worker.
    ///
    /// These sessions are used only for OP refresh-token root issuance,
    /// rotation, revoke, and refresh-family sweeps. The pool is deliberately
    /// small: excess concurrent refresh-family transactions wait instead of
    /// opening unbounded PostgreSQL backends. Read it through
    /// [`AuthConfig::refresh_pool_size`], which floors it at 1.
    #[config(name = "auth.refresh_pool_size", default = 4)]
    pub refresh_pool_size: Operational<usize>,

    /// Relay alias domain. Aliases are minted as `{token}@{relay_domain}`
    /// (lowercase). Dev: `relay.zeroship.localhost`; prod: `relay.zeroship.ai`
    /// (sub-spec §9). The inbound webhook resolves `OriginalRecipient` against
    /// this domain and the forward builder rewrites `From`/`Reply-To` to it.
    #[config(
        name = "auth.relay_domain",
        default = "relay.zeroship.localhost".to_owned()
    )]
    pub relay_domain: Operational<String>,

    /// HTTP Basic-auth username the inbound provider (Postmark Inbound) must
    /// present on every `POST /webhooks/relay-inbound`. When empty, the handler
    /// 401s so an unverified POST cannot drive a forward/suppression spoof.
    #[config(name = "auth.relay_inbound_user", default = String::new())]
    pub relay_inbound_user: Operational<String>,

    /// HTTP Basic-auth password paired with [`Self::relay_inbound_user`].
    #[config(name = "auth.relay_inbound_password")]
    pub relay_inbound_password: Secret<String>,

    /// Relay-forward mailer driver: `smtp` (default - the forward path needs
    /// envelope-from control) | `stdout` (dev terminal). `resend` is REJECTED
    /// for this role (it cannot pin envelope-from, sub-spec §3.2/§5.2a). This
    /// is a SECOND, dedicated mailer distinct from [`Self::mailer`] so the relay
    /// sends from the relay-domain identity (§5.5 reputation isolation).
    #[config(name = "auth.relay_forward_mailer", default = "smtp".to_owned())]
    pub relay_forward_mailer: Operational<String>,

    /// Relay-forward SMTP host (required when `--relay-forward-mailer=smtp`).
    /// Independent of [`Self::smtp_host`] - the relay sending identity is
    /// distinct from the transactional one (sub-spec §5.2a). Empty means unset.
    #[config(name = "auth.relay_smtp_host", default = String::new())]
    pub relay_smtp_host: Operational<String>,

    /// Relay-forward SMTP port. Defaults to 587 (STARTTLS).
    #[config(name = "auth.relay_smtp_port", default = 587)]
    pub relay_smtp_port: Operational<u16>,

    /// Relay-forward SMTP username (optional - dev sinks need none). Empty
    /// means unset.
    #[config(name = "auth.relay_smtp_username", default = String::new())]
    pub relay_smtp_username: Operational<String>,

    /// Relay-forward SMTP password (paired with `--relay-smtp-username`).
    #[config(name = "auth.relay_smtp_password")]
    pub relay_smtp_password: Secret<String>,

    /// Transport encryption for the relay-forward SMTP leg:
    /// `starttls` (default, port 587) | `implicit` (SMTPS, port 465) |
    /// `plaintext` (no TLS). Dev sinks (mailpit on :1025) use `plaintext`;
    /// prod uses `starttls`.
    #[arg(value_enum)]
    #[config(name = "auth.relay_smtp_tls", default = SmtpTls::Starttls)]
    pub relay_smtp_tls: Operational<SmtpTls>,

    /// HTTP Basic-auth username Postmark must present on every
    /// `POST /webhooks/postmark` request. Configured per-server in the
    /// Postmark dashboard's webhook settings. When empty, the webhook
    /// handler responds with 401 so misrouted traffic doesn't silently
    /// succeed.
    #[config(name = "auth.postmark_webhook_user", default = String::new())]
    pub postmark_webhook_user: Operational<String>,

    /// HTTP Basic-auth password paired with [`Self::postmark_webhook_user`].
    /// See that field for the rationale.
    #[config(name = "auth.postmark_webhook_password")]
    pub postmark_webhook_password: Secret<String>,

    // ---- key FILES ------------------------------------------------------
    //
    // Every field below names a PATH and stays `Operational<PathBuf>`, which is
    // NOT an oversight: a path to a secret is not itself a secret, and each of
    // these loaders does two things an in-memory `Secret<String>` cannot.
    //
    // 1. It reads RAW BYTES (`std::fs::read`) because the material may not be
    //    UTF-8 - a PKCS#8 DER key, a 32-byte random salt, a raw broker master
    //    secret. Routing that through a `String` would reject the documented
    //    generation recipe (`head -c 32 /dev/urandom > file`), and the newline
    //    trim every string-tier secret gets would silently change the value the
    //    salt and broker derivations hash.
    // 2. It calls `reject_insecure_permissions(path)` - the group/world-readable
    //    refusal that mirrors OpenSSH's treatment of a private key. That check
    //    has nothing to look at once the material is a string in memory, so
    //    converting these would DELETE a live protection.
    //
    // See `oidc/signing.rs` (`load_ed25519_from_path`, `load_pairwise_salt_secret`,
    // `load_broker_master_secret`) and `oidc/refresh.rs` (`read_secret_file`,
    // `load_hash_keyring`). An empty path means "unset"; read each through the
    // matching [`AuthConfig`] accessor, which returns `None` for one.
    /// Platform OP Ed25519 private signing key file (PEM/PKCS#8 or DER).
    ///
    /// Required on real boot. The key is loaded into auth-service memory and
    /// never stored in Postgres; `zeroship.signing_keys` receives only the
    /// matching public JWK metadata.
    #[config(name = "auth.signing_key_file", default = PathBuf::new())]
    pub signing_key_file: Operational<PathBuf>,

    /// Platform OP pairwise subject salt source file.
    ///
    /// Required on real boot. The file contents are fed through
    /// `derive_pairwise_salt`, then used with `(user_id, sector_identifier)` to
    /// mint cross-app-unlinkable `pws_...` subjects.
    #[config(name = "auth.pairwise_salt_file", default = PathBuf::new())]
    pub pairwise_salt_file: Operational<PathBuf>,

    /// Platform broker master-secret source file.
    ///
    /// Required on real boot. Brokered `OAuth` clients do not store per-client
    /// secret hashes; the OP derives a per-client broker secret from this
    /// >=256-bit master secret and the `client_id`, then verifies the gateway's
    /// presented secret on the authorization-code grant.
    #[config(name = "auth.broker_secret_file", default = PathBuf::new())]
    pub broker_secret_file: Operational<PathBuf>,

    /// Previous platform broker master-secret source file for rotation.
    ///
    /// Optional. During a rolling rotation, code exchanges may authenticate
    /// against either the current or previous broker master secret. Remove this
    /// after every gateway has rolled to the current secret.
    #[config(name = "auth.broker_secret_previous_file", default = PathBuf::new())]
    pub broker_secret_previous_file: Operational<PathBuf>,

    /// Refresh-token HMAC keyring file.
    ///
    /// The OP stores only HMAC-SHA256 refresh-token verifiers in Postgres.
    /// This file is the out-of-DB keyring used to mint/verify those hashes, and
    /// it is a MULTI-LINE `version:key` document, not one opaque value.
    #[config(name = "auth.refresh_hash_key_file", default = PathBuf::new())]
    pub refresh_hash_key_file: Operational<PathBuf>,

    /// Refresh-token idempotency-cache AEAD key source file.
    ///
    /// Used only to seal the bounded lost-response retry cache stored on a
    /// rotated predecessor row.
    #[config(name = "auth.refresh_idem_key_file", default = PathBuf::new())]
    pub refresh_idem_key_file: Operational<PathBuf>,

    /// Cron tick interval in seconds. Default 86400 (24 h). Operators
    /// drop this to seconds in staging/integration tests so a cron
    /// behaviour change is observable inside a single test run.
    #[config(name = "auth.cron_tick_secs", default = 86_400)]
    pub cron_tick_secs: Operational<u64>,

    /// Audit-retention sweeper tick interval in seconds. Default 3600
    /// (hourly). The sweep itself is cheap (one indexed DELETE per
    /// bucket) so hourly cadence keeps the table close to its hot-tier
    /// shape without making the sweeper hot. Operators can drop this
    /// for tests; production should leave the default.
    #[config(name = "auth.audit_retention_check_secs", default = 3_600)]
    pub audit_retention_check_secs: Operational<u64>,
}

impl OverlaySelector for AuthSettingsSources {
    fn overlay_path(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    fn allow_discovery(&self) -> bool {
        !self.no_config
    }
}

impl ObservabilityControls for AuthSettings {
    fn log_filter(&self) -> &str {
        self.log_filter.get()
    }

    fn log_format(&self) -> LogFormat {
        *self.log_format.get()
    }
}

/// zeroship-auth command line.
///
/// `Debug` is safe to derive: the generated carrier holds operational values
/// and, for every secret, a file PATH - never the material in it.
#[derive(Clone, Debug, Parser)]
#[command(name = "zeroship-auth")]
pub struct AuthCli {
    /// Every value an auth launch resolves, generated from one declaration.
    #[command(flatten)]
    pub settings: AuthSettingsSources,
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

/// The resolved auth-server configuration the rest of the crate reads.
///
/// Built by [`Self::from_resolved`], which is the ONLY constructor. That matters
/// for one field: [`Self::frame_ancestor_origins`] is the fail-closed FILTERED
/// allowlist, and the constructor is where the filter runs. The raw list on
/// [`AuthSettings`] is still reachable, so the filter is not enforced by the
/// type - it is enforced by every consumer reading the accessor, which is what
/// `frame_ancestor_origins_are_filtered_before_any_consumer_sees_them` in this
/// module's tests pins.
///
/// `Debug` is DERIVED. Every credential-bearing field is a `Secret<T>`, and
/// that type cannot format its material - not the value, not a prefix, not its
/// length. The one exception is `supabase_anon_key`, deliberately NOT redacted:
/// it is served to every browser that loads the GoTrue login, so calling it
/// secret would claim a protection it does not have.
#[derive(Clone, Debug)]
pub struct AuthConfig {
    /// Every resolved value: operational, secret, and command control.
    pub settings: AuthSettings,

    /// Frame-ancestor origins AFTER the concrete + same-site filter.
    frame_ancestor_origins: Vec<String>,
}

impl AuthConfig {
    /// Build the resolved configuration and run auth's own resolve-time guards.
    ///
    /// The generated resolver has already applied `CLI > env > overlay >
    /// compiled default` to every value. What is left is the part that is not
    /// mechanical:
    ///
    /// * the frame-ancestor allowlist is filtered fail-closed - non-concrete
    ///   origins and origins that are not same-site with the issuer host are
    ///   DROPPED, never carried into a CSP header;
    /// * selecting the Supabase provider without its URL and anon key is a
    ///   startup error rather than a half-configured boot.
    ///
    /// # Errors
    ///
    /// Returns a message naming the missing Supabase inputs.
    pub fn from_resolved(settings: AuthSettings) -> Result<Self, String> {
        // The auth issuer host the SameSite=Strict `__Host-zsidp_csrf` cookie is
        // scoped to. Every admitted frame-ancestor MUST be same-site (eTLD+1)
        // with it (review finding I7) - otherwise the Strict cookie is withheld
        // on the in-frame POST (framed login silently breaks) and the only
        // "fixes" are insecure (Strict->None re-opens cross-site CSRF). A
        // non-same-site origin is therefore DROPPED here so the relaxed
        // `frame-ancestors` never admits it; that deployment falls back to popup.
        // NON-CONCRETE origins (wildcards, bad scheme, control chars) are
        // rejected for the same reason: a misconfiguration must never widen the
        // allowlist (design 6.2) nor produce an un-serializable CSP header value.
        let issuer_host = url::Url::parse(settings.public_url.get())
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        let frame_ancestor_origins = settings
            .frame_ancestor_origins
            .get()
            .iter()
            .filter(|origin| {
                is_concrete_frame_ancestor_origin(origin)
                    && is_same_site_with_issuer(origin, &issuer_host)
            })
            .cloned()
            .collect();

        let resolved = Self {
            settings,
            frame_ancestor_origins,
        };

        if resolved.auth_provider() == AuthProviderKind::Supabase {
            let mut missing = Vec::new();
            if resolved.supabase_url().is_none() {
                missing.push("ZEROSHIP_AUTH_SUPABASE_URL / --supabase-url");
            }
            if resolved.supabase_anon_key().is_none() {
                missing.push("ZEROSHIP_AUTH_SUPABASE_ANON_KEY / --supabase-anon-key");
            }
            if !missing.is_empty() {
                return Err(format!(
                    "ZEROSHIP_AUTH_PROVIDER=supabase requires {}",
                    missing.join(" and ")
                ));
            }
        }

        Ok(resolved)
    }

    /// Parse a command line, resolve it against `overlay`, and run the guards.
    ///
    /// # Errors
    ///
    /// Returns the clap error for an unparsable command line, and the
    /// resolve-time message otherwise.
    pub fn try_parse_and_resolve<I, T>(
        args: I,
        overlay: Option<&toml::Value>,
    ) -> Result<Self, String>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let cli = AuthCli::try_parse_from(args).map_err(|err| err.to_string())?;
        Self::try_from_cli(cli, overlay)
    }

    /// Resolve an already-parsed command line against `overlay`.
    ///
    /// # Errors
    ///
    /// Returns the generated resolver's value-free diagnostic, or auth's own
    /// resolve-time message.
    pub fn try_from_cli(cli: AuthCli, overlay: Option<&toml::Value>) -> Result<Self, String> {
        let settings = AuthSettings::resolve_config(cli.settings, overlay)
            .map_err(|err: ConfigResolveError| err.to_string())?;
        Self::from_resolved(settings)
    }

    /// Parse and resolve with no overlay, panicking on invalid input.
    ///
    /// The convenience the crate's tests and in-process fixtures use; real boot
    /// goes through [`Self::try_from_cli`] so an invalid configuration exits
    /// cleanly instead of unwinding.
    ///
    /// # Panics
    ///
    /// On an unparsable command line or a failed resolve-time guard.
    #[must_use]
    pub fn parse_from<I, T>(args: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        Self::try_parse_and_resolve(args, None).expect("resolve auth config")
    }

    /// Frame-ancestor origins after the fail-closed filter. The ONLY list safe
    /// to splice into a CSP `frame-ancestors` source-list.
    #[must_use]
    pub fn frame_ancestor_origins(&self) -> &[String] {
        &self.frame_ancestor_origins
    }

    /// Resolved auth-provider backend.
    #[must_use]
    pub fn auth_provider(&self) -> AuthProviderKind {
        *self.settings.provider.get()
    }

    /// Resolved Supabase Auth / GoTrue base URL; `None` when unset.
    #[must_use]
    pub fn supabase_url(&self) -> Option<&str> {
        non_empty(self.settings.supabase_url.get())
    }

    /// Resolved Supabase anon API key; `None` when unset.
    #[must_use]
    pub fn supabase_anon_key(&self) -> Option<&str> {
        non_empty(self.settings.supabase_anon_key.get())
    }

    /// Resolved control-plane base URL.
    #[must_use]
    pub fn control_url(&self) -> &str {
        self.settings.control_url.get()
    }

    /// Google OAuth client ID; `None` when unset, which is what keeps the
    /// `/oauth/google/*` routes unregistered.
    #[must_use]
    pub fn google_client_id(&self) -> Option<&str> {
        non_empty(self.settings.google_client_id.get())
    }

    /// GitHub OAuth client ID; `None` when unset, which is what keeps the
    /// `/oauth/github/*` routes unregistered.
    #[must_use]
    pub fn github_client_id(&self) -> Option<&str> {
        non_empty(self.settings.github_client_id.get())
    }

    /// Transactional SMTP host; `None` when unset.
    #[must_use]
    pub fn smtp_host(&self) -> Option<&str> {
        non_empty(self.settings.smtp_host.get())
    }

    /// Transactional SMTP username; `None` when the relay needs no auth.
    #[must_use]
    pub fn smtp_username(&self) -> Option<&str> {
        non_empty(self.settings.smtp_username.get())
    }

    /// Relay-forward SMTP host; `None` when unset.
    #[must_use]
    pub fn relay_smtp_host(&self) -> Option<&str> {
        non_empty(self.settings.relay_smtp_host.get())
    }

    /// Relay-forward SMTP username; `None` when the sink needs no auth.
    #[must_use]
    pub fn relay_smtp_username(&self) -> Option<&str> {
        non_empty(self.settings.relay_smtp_username.get())
    }

    /// Relay inbound webhook Basic-auth username; `None` fail-closes the
    /// handler with 401.
    #[must_use]
    pub fn relay_inbound_user(&self) -> Option<&str> {
        non_empty(self.settings.relay_inbound_user.get())
    }

    /// Postmark webhook Basic-auth username; `None` fail-closes the handler
    /// with 401.
    #[must_use]
    pub fn postmark_webhook_user(&self) -> Option<&str> {
        non_empty(self.settings.postmark_webhook_user.get())
    }

    /// Refresh-family pool size, floored at 1: a zero-sized pool would deadlock
    /// every refresh-token transaction rather than merely serialising them.
    #[must_use]
    pub fn refresh_pool_size(&self) -> usize {
        (*self.settings.refresh_pool_size.get()).max(1)
    }

    /// External origin of this auth server (no trailing slash). Returns
    /// `auth.public_url` with any trailing `/` trimmed so callers can
    /// freely concatenate `/magic/verify?...`.
    #[must_use]
    pub fn public_url(&self) -> String {
        self.settings.public_url.get().trim_end_matches('/').to_string()
    }

    /// Platform OP Ed25519 signing key file; `None` when unset.
    #[must_use]
    pub fn signing_key_file(&self) -> Option<&Path> {
        non_empty_path(self.settings.signing_key_file.get())
    }

    /// Pairwise-subject salt source file; `None` when unset.
    #[must_use]
    pub fn pairwise_salt_file(&self) -> Option<&Path> {
        non_empty_path(self.settings.pairwise_salt_file.get())
    }

    /// Broker master-secret source file; `None` when unset.
    #[must_use]
    pub fn broker_secret_file(&self) -> Option<&Path> {
        non_empty_path(self.settings.broker_secret_file.get())
    }

    /// PREVIOUS broker master-secret source file; `None` outside a rotation.
    #[must_use]
    pub fn broker_secret_previous_file(&self) -> Option<&Path> {
        non_empty_path(self.settings.broker_secret_previous_file.get())
    }

    /// Refresh-token HMAC keyring file; `None` when unset.
    #[must_use]
    pub fn refresh_hash_key_file(&self) -> Option<&Path> {
        non_empty_path(self.settings.refresh_hash_key_file.get())
    }

    /// Refresh idempotency-cache AEAD key file; `None` when unset.
    #[must_use]
    pub fn refresh_idem_key_file(&self) -> Option<&Path> {
        non_empty_path(self.settings.refresh_idem_key_file.get())
    }

    /// Public issuer URL for the self-contained OAuth/OIDC provider.
    ///
    /// Protocol endpoints are mounted under the fixed `/oauth2` prefix so the
    /// OP is reverse-proxyable without owning the host root. This is the single
    /// source for the `iss` stamped into tokens and discovery metadata.
    #[must_use]
    pub fn op_issuer_url(&self) -> String {
        format!("{}/oauth2", self.public_url())
    }
}

/// Treat an empty operational string as "unset".
///
/// Every optional auth string defaults to `String::new()` rather than
/// `Option<String>`: `Operational<T>`'s supply set includes a compiled default,
/// so "absent" has one spelling across the flag, the environment, the overlay
/// and the default. Trimming here keeps a whitespace-only value from reading as
/// configured.
fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Treat an empty operational path as "unset", for the same reason as
/// [`non_empty`]: `Operational<PathBuf>` always has a compiled default, so the
/// empty path is how a key FILE says nobody supplied one. No trim here - a path
/// is not a token, and whitespace can be part of a real one.
fn non_empty_path(value: &Path) -> Option<&Path> {
    (!value.as_os_str().is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn overlay(toml_text: &str) -> toml::Value {
        toml::from_str(toml_text).expect("fixture overlay")
    }

    fn test_config() -> AuthConfig {
        AuthConfig::parse_from(["zeroship-auth"])
    }

    /// Resolve a command line against a `[auth]` overlay fragment.
    fn resolve_with(args: &[&str], toml_text: &str) -> AuthConfig {
        let mut argv = Vec::from(["zeroship-auth"]);
        argv.extend_from_slice(args);
        AuthConfig::try_parse_and_resolve(argv, Some(&overlay(toml_text)))
            .expect("resolve auth config")
    }

    // ---- the canonical projection -------------------------------------

    #[test]
    fn every_auth_environment_name_is_a_canonical_projection() {
        // The invariant: every environment name the settings carrier exposes is
        // a canonical `ZEROSHIP_*` projection. A rename must not leave the old
        // `AUTH_*` spelling working, or a deployment that still exports it keeps
        // booting and nobody learns the name changed.
        let envs = AuthSettingsSources::command()
            .get_arguments()
            .filter_map(|arg| arg.get_env().map(|env| env.to_string_lossy().into_owned()))
            .collect::<Vec<_>>();
        assert!(!envs.is_empty(), "the settings must carry environment names");
        for env in &envs {
            assert!(
                env.starts_with("ZEROSHIP_"),
                "{env} is not a canonical projection"
            );
        }
        assert!(envs.contains(&"ZEROSHIP_AUTH_PUBLIC_URL".to_owned()));
        assert!(envs.contains(&"ZEROSHIP_AUTH_FRAME_ANCESTOR_ORIGINS".to_owned()));
        // `auth.provider`, not `auth.auth_provider`: the stutter would have
        // produced ZEROSHIP_AUTH_AUTH_PROVIDER.
        assert!(envs.contains(&"ZEROSHIP_AUTH_PROVIDER".to_owned()));
        // Shared identities keep their unprefixed canonical names, so auth,
        // gateway and worker read ONE variable for the control-plane URL.
        assert!(envs.contains(&"ZEROSHIP_CONTROL_URL".to_owned()));

        // The whole command line, not just the settings carrier: every argument
        // is checked, so no `AUTH_*` spelling can survive as a working alias for
        // a retired setting.
        let all_envs = AuthCli::command()
            .get_arguments()
            .filter_map(|arg| arg.get_env().map(|env| env.to_string_lossy().into_owned()))
            .collect::<Vec<_>>();
        for retired in [
            "AUTH_DB_URL",
            "AUTH_STASH_SIGNING_KEY",
            "AUTH_TOTP_ENC_KEY",
            "CONTROL_KEY",
            "AUTH_SIGNING_KEY_FILE",
            "AUTH_PAIRWISE_SALT_FILE",
            "AUTH_BROKER_SECRET_FILE",
            "REFRESH_HASH_KEY_FILE",
            "REFRESH_IDEM_KEY_FILE",
        ] {
            assert!(
                !all_envs.iter().any(|env| env == retired),
                "{retired} survived the conversion as a working alias"
            );
        }
        // The one-variable control for the loop above: the file-PATH settings
        // DO carry an environment name, at the canonical spelling. Without it,
        // a build that dropped those arguments entirely would also pass.
        assert!(all_envs.contains(&"ZEROSHIP_AUTH_SIGNING_KEY_FILE".to_owned()));
    }

    // A `Secret<T>` generates a `-file` PATH flag and NO value flag. That is the
    // property that keeps credential material out of this process's argument
    // vector, where any other user on the box can read it from /proc.
    #[test]
    fn no_secret_has_a_value_flag_and_each_has_a_file_flag() {
        let command = AuthCli::command();
        let longs = command
            .get_arguments()
            .filter_map(|arg| arg.get_long().map(str::to_owned))
            .collect::<Vec<_>>();

        for (value_flag, file_flag) in [
            ("db-url", "database-url-file"),
            ("database-url", "database-url-file"),
            ("stash-signing-key", "stash-signing-key-file"),
            ("totp-enc-key", "totp-enc-key-file"),
            ("google-client-secret", "google-client-secret-file"),
            ("github-client-secret", "github-client-secret-file"),
            ("smtp-password", "smtp-password-file"),
            ("resend-api-key", "resend-api-key-file"),
            ("gotrue-email-hook-secret", "gotrue-email-hook-secret-file"),
            ("relay-inbound-password", "relay-inbound-password-file"),
            ("relay-smtp-password", "relay-smtp-password-file"),
            ("postmark-webhook-password", "postmark-webhook-password-file"),
        ] {
            assert!(
                !longs.iter().any(|long| long == value_flag),
                "--{value_flag} would carry secret material through argv"
            );
            assert!(
                longs.iter().any(|long| long == file_flag),
                "--{file_flag} is missing, so the secret has no CLI source at all"
            );
        }

        // Does NOT cover the environment tier: a secret's canonical env name is
        // read by the generated resolver, not by clap, so it is deliberately
        // absent from this metadata. The config_env_tier integration target
        // runs `--check-config` against a set variable.
    }

    #[test]
    fn check_config_is_a_flag_with_no_environment_source() {
        // A stray environment variable must not be able to turn a running auth
        // server into a config dump that exits before serving.
        let command = AuthSettingsSources::command();
        for id in ["check_config", "check_config_format"] {
            let arg = command
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .unwrap_or_else(|| panic!("no argument {id}"));
            assert_eq!(arg.get_env(), None, "{id} must not read the environment");
        }
    }

    #[test]
    fn discovery_is_on_unless_no_config_is_passed() {
        let plain =
            AuthSettingsSources::try_parse_from(["zeroship-auth"]).expect("bare parse");
        assert!(plain.allow_discovery());
        assert_eq!(plain.overlay_path(), None);

        let suppressed =
            AuthSettingsSources::try_parse_from(["zeroship-auth", "--no-config"])
                .expect("no-config parse");
        assert!(!suppressed.allow_discovery());

        let explicit = AuthSettingsSources::try_parse_from([
            "zeroship-auth",
            "--config",
            "/etc/zeroship/zeroship.toml",
        ])
        .expect("config parse");
        assert_eq!(
            explicit.overlay_path(),
            Some(std::path::Path::new("/etc/zeroship/zeroship.toml"))
        );

        // Does not cover the environment tier; clap merges it into the same
        // carriers and a process-wide env mutation would race sibling tests.
    }

    // ---- provider selection -------------------------------------------

    #[test]
    fn auth_provider_defaults_to_native() {
        let cfg = test_config();
        assert_eq!(cfg.auth_provider(), AuthProviderKind::Native);
        assert!(cfg.supabase_url().is_none());
        assert!(cfg.supabase_anon_key().is_none());
        assert_eq!(cfg.control_url(), "http://localhost:9090");
    }

    #[test]
    fn supabase_provider_requires_url_and_anon_key() {
        let err = AuthConfig::try_parse_and_resolve(
            [
                "zeroship-auth",
                "--provider",
                "supabase",
            ],
            None,
        )
        .expect_err("supabase provider must fail closed without GoTrue config");

        assert!(err.contains("ZEROSHIP_AUTH_SUPABASE_URL"), "{err}");
        assert!(err.contains("ZEROSHIP_AUTH_SUPABASE_ANON_KEY"), "{err}");
    }

    #[test]
    fn supabase_provider_resolves_with_required_fields() {
        let cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--provider",
            "supabase",
            "--supabase-url",
            "https://project.supabase.test",
            "--supabase-anon-key",
            "anon-test-key",
            "--control-url",
            "https://control.zeroship.test",
        ]);

        assert_eq!(cfg.auth_provider(), AuthProviderKind::Supabase);
        assert_eq!(cfg.supabase_url(), Some("https://project.supabase.test"));
        assert_eq!(cfg.supabase_anon_key(), Some("anon-test-key"));
        assert_eq!(cfg.control_url(), "https://control.zeroship.test");
    }

    #[test]
    fn the_overlay_supplies_supabase_config_at_the_canonical_paths() {
        // The generated resolver walks the canonical dotted path itself, so the
        // overlay keys ARE `auth.provider` / `auth.supabase_url` and the
        // control-plane URL is the SHARED root-level `control_url`, not a second
        // `[auth]` copy of it.
        let cfg = resolve_with(
            &[],
            r#"
control_url = "https://control-file.zeroship.test"

[auth]
provider = "supabase"
supabase_url = "https://project.supabase.test"
supabase_anon_key = "anon-file-key"
"#,
        );

        assert_eq!(cfg.auth_provider(), AuthProviderKind::Supabase);
        assert_eq!(cfg.supabase_url(), Some("https://project.supabase.test"));
        assert_eq!(cfg.supabase_anon_key(), Some("anon-file-key"));
        assert_eq!(cfg.control_url(), "https://control-file.zeroship.test");
    }

    #[test]
    fn the_flag_wins_over_the_overlay_and_untouched_overlay_values_survive() {
        let cfg = resolve_with(
            &["--mailer", "resend"],
            "[auth]\nmailer = \"smtp\"\nsmtp_port = 2525\n",
        );
        assert_eq!(cfg.settings.mailer.get(), "resend", "the flag must win");
        assert_eq!(
            *cfg.settings.smtp_port.get(),
            2525,
            "the untouched overlay value must survive"
        );

        // Does not cover the environment tier: clap merges it into the same
        // carrier, and a process-wide env mutation would race sibling tests.
    }

    // ---- secret strength guards (unchanged policy) ---------------------

    use zeroship_core::config::{validate_secret_material, validate_stash_key, SourceKind};

    /// A secret in the shape an in-memory literal (env or TOML) resolves to.
    fn supplied(material: &str) -> Secret<String> {
        Secret::supplied(SourceKind::Env, Some(material.to_owned()))
    }

    // The guard reads the RESOLVED secret through `validate_secret_material`
    // now rather than a plain `String`, so these three cases pin that the
    // wrapper softened none of them.
    #[test]
    fn stash_key_unset_is_rejected() {
        let cfg = test_config();
        assert!(!cfg.settings.stash_signing_key.is_configured());
        assert!(
            validate_secret_material(&cfg.settings.stash_signing_key, |v| validate_stash_key(
                "ZEROSHIP_AUTH_STASH_SIGNING_KEY",
                v,
            )).is_err()
        );
    }

    #[test]
    fn stash_key_short_is_rejected() {
        let mut cfg = test_config();
        cfg.settings.stash_signing_key = supplied(&"a".repeat(20));
        assert!(
            validate_secret_material(&cfg.settings.stash_signing_key, |v| validate_stash_key(
                "ZEROSHIP_AUTH_STASH_SIGNING_KEY",
                v,
            )).is_err()
        );
    }

    #[test]
    fn stash_key_strong_is_accepted() {
        let mut cfg = test_config();
        cfg.settings.stash_signing_key = supplied("0123456789abcdef0123456789abcdef");
        assert!(
            validate_secret_material(&cfg.settings.stash_signing_key, |v| validate_stash_key(
                "ZEROSHIP_AUTH_STASH_SIGNING_KEY",
                v,
            )).is_ok()
        );
    }

    // A `Secret<T>` cannot declare a compiled default - the macro refuses one -
    // so there is no sentinel to leak in `--help` and nothing to mistake for a
    // configured key. An unsupplied secret exposes NO material at all.
    #[test]
    fn an_unsupplied_stash_key_exposes_no_material() {
        let cfg = test_config();
        assert_eq!(cfg.settings.stash_signing_key.expose_secret(), None);
        assert_eq!(cfg.settings.stash_signing_key.source(), None);
    }

    #[test]
    fn dev_insecure_flag_is_rejected() {
        let err = AuthCli::try_parse_from([
            "zeroship-auth",
            "--dev-insecure",
        ])
        .map(|_| ())
        .expect_err("--dev-insecure must not be accepted");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn a_bare_parse_supplies_neither_startup_key() {
        // The half of "the obsolete relaxation variable is ignored" that needs
        // no environment: no supply tier and no default hands auth either key,
        // so a bare parse leaves both unconfigured and the startup guards
        // reject them.
        //
        // The ENVIRONMENT half - `ZEROSHIP_DEV_INSECURE=1` reaching no tier -
        // is in `crates/zeroship-auth/tests/config_env_tier.rs`, against the real binary
        // with `Command::env`. It cannot live here: observing an environment
        // tier in-process means putting the variable in THIS process, where
        // every other test would then parse against it.
        let cfg = test_config();
        assert!(!cfg.settings.stash_signing_key.is_configured());
        assert!(!cfg.settings.totp_enc_key.is_configured());
    }

    #[test]
    fn old_insecure_dev_flag_is_rejected() {
        let err = AuthCli::try_parse_from([
            "zeroship-auth",
            "--insecure-dev",
        ])
        .map(|_| ())
        .expect_err("--insecure-dev no longer accepted");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    // ---- SMTP transport mode -------------------------------------------

    #[test]
    fn smtp_tls_defaults_to_starttls() {
        let cfg = test_config();
        assert_eq!(*cfg.settings.smtp_tls.get(), SmtpTls::Starttls);
        assert_eq!(*cfg.settings.relay_smtp_tls.get(), SmtpTls::Starttls);
    }

    #[test]
    fn smtp_tls_accepts_plaintext_value_on_cli() {
        let cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--smtp-tls",
            "plaintext",
            "--relay-smtp-tls",
            "plaintext",
        ]);
        assert_eq!(*cfg.settings.smtp_tls.get(), SmtpTls::Plaintext);
        assert_eq!(*cfg.settings.relay_smtp_tls.get(), SmtpTls::Plaintext);
    }

    #[test]
    fn smtp_tls_accepts_implicit_and_starttls_values() {
        let cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--smtp-tls=implicit",
            "--relay-smtp-tls=starttls",
        ]);
        assert_eq!(*cfg.settings.smtp_tls.get(), SmtpTls::Implicit);
        assert_eq!(*cfg.settings.relay_smtp_tls.get(), SmtpTls::Starttls);
    }

    #[test]
    fn smtp_tls_rejects_unknown_mode() {
        let err = AuthCli::try_parse_from([
            "zeroship-auth",
            "--smtp-tls",
            "sslv3",
        ])
        .map(|_| ())
        .expect_err("unknown TLS mode must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn smtp_tls_overlay_spelling_matches_the_flag_spelling() {
        // The `Deserialize` impl and `ValueEnum` must accept the SAME token, or
        // an operator moving a value from the flag into the overlay gets a
        // startup error for a value the CLI took happily.
        let cfg = resolve_with(&[], "[auth]\nsmtp_tls = \"plaintext\"\n");
        assert_eq!(*cfg.settings.smtp_tls.get(), SmtpTls::Plaintext);
    }

    #[test]
    fn old_smtp_starttls_flag_is_rejected() {
        let err = AuthCli::try_parse_from([
            "zeroship-auth",
            "--smtp-starttls",
        ])
        .map(|_| ())
        .expect_err("--smtp-starttls no longer accepted");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    // ---- the frame-ancestor allowlist ----------------------------------

    #[test]
    fn frame_ancestor_origins_cli_comma_split_and_repeatable() {
        let cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--public-url",
            "https://auth.zeroship.ai",
            "--frame-ancestor-origins",
            "https://console.zeroship.ai,https://staging.zeroship.ai",
            "--frame-ancestor-origins",
            "https://preview.zeroship.ai",
        ]);
        assert_eq!(
            cfg.frame_ancestor_origins(),
            [
                "https://console.zeroship.ai".to_string(),
                "https://staging.zeroship.ai".to_string(),
                "https://preview.zeroship.ai".to_string(),
            ]
        );
    }

    #[test]
    fn frame_ancestor_origins_default_empty() {
        assert!(test_config().frame_ancestor_origins().is_empty());
    }

    #[test]
    fn frame_ancestor_origins_cli_wins_over_overlay() {
        let cfg = resolve_with(
            &[
                // Issuer same-site with the test origins so the I7 guard is a
                // no-op here; this test isolates CLI-vs-overlay precedence.
                "--public-url",
                "https://auth.zeroship.ai",
                "--frame-ancestor-origins",
                "https://console.zeroship.ai",
            ],
            "[auth]\nframe_ancestor_origins = [\"https://overlay.zeroship.ai\"]\n",
        );
        assert_eq!(
            cfg.frame_ancestor_origins(),
            ["https://console.zeroship.ai".to_string()],
            "a CLI/env value must win over the [auth] overlay"
        );
    }

    #[test]
    fn frame_ancestor_origins_overlay_used_when_no_flag_is_given() {
        let cfg = resolve_with(
            &["--public-url", "https://auth.zeroship.ai"],
            "[auth]\nframe_ancestor_origins = [\"https://overlay.zeroship.ai\", \"\"]\n",
        );
        assert_eq!(
            cfg.frame_ancestor_origins(),
            ["https://overlay.zeroship.ai".to_string()],
            "with no flag the overlay supplies the list (empties dropped)"
        );
    }

    // Section 6.2 "no wildcards": a misconfigured `*` / `https://*.zeroship.ai`
    // (or any non-concrete origin) must be REJECTED at config-resolve, so it can
    // never reach the `frame-ancestors` builder and re-admit every creator app,
    // and so `static_insert` can never hit its un-serializable fallback (4.3).
    #[test]
    fn frame_ancestor_origins_rejects_wildcards_and_non_concrete() {
        let cfg = AuthConfig::parse_from([
            "zeroship-auth",
            // Same-site issuer so this test isolates the wildcard/non-concrete
            // rejection (not the I7 same-site drop).
            "--public-url",
            "https://auth.zeroship.ai",
            "--frame-ancestor-origins",
            // Mixed: one valid origin + several poison entries.
            "https://console.zeroship.ai,https://*.zeroship.ai,*,'self',data:,\
             ftp://console.zeroship.ai,https://a b.zeroship.ai,console.zeroship.ai",
        ]);
        assert_eq!(
            cfg.frame_ancestor_origins(),
            ["https://console.zeroship.ai".to_string()],
            "only the concrete http(s) origin survives; wildcards / keywords / \
             bad schemes / space-bearing tokens are dropped"
        );
    }

    #[test]
    fn frame_ancestor_origins_rejects_wildcards_from_overlay_too() {
        let cfg = resolve_with(
            // Same-site issuer so this test isolates wildcard/keyword rejection.
            &["--public-url", "https://auth.zeroship.ai"],
            r#"
[auth]
frame_ancestor_origins = [
  "https://*.zeroship.ai",
  "https://console.zeroship.ai",
  "'none'",
]
"#,
        );
        assert_eq!(
            cfg.frame_ancestor_origins(),
            ["https://console.zeroship.ai".to_string()],
            "the overlay path applies the same wildcard/keyword rejection"
        );
    }

    // I7 (latent guard): the SameSite=Strict `__Host-zsidp_csrf` cookie reaches
    // the in-frame POST only when the framing (console) origin is SAME-SITE
    // (same registrable domain / eTLD+1) with the auth issuer host. A
    // cross-registrable-domain console silently breaks framed login (cookie
    // withheld) and tempts a `Strict->None` downgrade that re-opens cross-site
    // CSRF. The resolve guard MUST drop any frame-ancestor origin that is not
    // same-site with `public_url`'s host, so the relaxed `frame-ancestors` never
    // admits a non-same-site embedder (it falls back to popup / strict default).
    #[test]
    fn frame_ancestor_origins_drops_non_same_site_with_issuer() {
        let cfg = resolve_with(
            // Concrete prod issuer host: registrable domain `zeroship.ai`.
            &["--public-url", "https://auth.zeroship.ai"],
            r#"
[auth]
frame_ancestor_origins = [
  # Same-site with the issuer (same registrable domain) - KEPT.
  "https://console.zeroship.ai",
  # Concrete + wildcard-free, but a DIFFERENT registrable domain - NOT
  # same-site, so the Strict CSRF cookie would be withheld in the frame.
  "https://console.zeroship-eu.com",
  # A look-alike suffix that merely CONTAINS the issuer domain as a
  # substring but is a different registrable domain - dropped.
  "https://console.zeroship.ai.evil.com",
]
"#,
        );
        assert_eq!(
            cfg.frame_ancestor_origins(),
            ["https://console.zeroship.ai".to_string()],
            "only the origin same-site (eTLD+1) with the auth issuer survives; \
             cross-registrable-domain consoles are dropped (relax -> popup)"
        );
    }

    // The same-site guard also applies on the CLI path (a CLI value wins over
    // the overlay but is still subject to the same-site drop).
    #[test]
    fn frame_ancestor_origins_cli_drops_non_same_site() {
        let cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--public-url",
            "https://auth.zeroship.ai",
            "--frame-ancestor-origins",
            "https://console.zeroship.ai,https://attacker.example.com",
        ]);
        assert_eq!(
            cfg.frame_ancestor_origins(),
            ["https://console.zeroship.ai".to_string()],
            "a non-same-site CLI origin is dropped by the issuer-same-site guard"
        );
    }

    // Dev loopback: issuer host `localhost`; a `localhost` console (any port) is
    // same-site, but a real registrable-domain console is not.
    #[test]
    fn frame_ancestor_origins_loopback_issuer_keeps_localhost_only() {
        // public_url defaults to http://localhost:9092.
        let cfg = resolve_with(
            &[],
            r#"
[auth]
frame_ancestor_origins = ["http://localhost:5173", "https://console.zeroship.ai"]
"#,
        );
        assert_eq!(
            cfg.frame_ancestor_origins(),
            ["http://localhost:5173".to_string()],
            "with a loopback issuer only localhost consoles are same-site"
        );
    }

    // The filter is the point of `AuthConfig` existing. This pins that the
    // accessor every consumer reads is the FILTERED list while the generated
    // declaration still carries the raw one, so a future refactor that wires a
    // consumer straight to `settings.frame_ancestor_origins` shows up here as a
    // visible difference rather than as a silently widened CSP header.
    #[test]
    fn frame_ancestor_origins_are_filtered_before_any_consumer_sees_them() {
        let cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--public-url",
            "https://auth.zeroship.ai",
            "--frame-ancestor-origins",
            "https://console.zeroship.ai,https://*.zeroship.ai,https://attacker.example.com",
        ]);
        assert_eq!(
            cfg.settings.frame_ancestor_origins.get().len(),
            3,
            "the declaration keeps the raw operator input"
        );
        assert_eq!(
            cfg.frame_ancestor_origins(),
            ["https://console.zeroship.ai".to_string()],
            "the accessor is fail-closed filtered"
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

    // ---- redaction ------------------------------------------------------

    // The derive is safe only because `Secret<T>` itself cannot format its
    // material, so this test drives the DERIVED impl over a config whose
    // secrets all carry a distinctive sentinel. A hand-written `Debug` with a
    // per-field `<redacted>` list is a list someone forgets to extend: the
    // field added in the next patch prints in full and nothing says so.
    //
    // "Redacted" is asserted three ways, because each is a different mistake a
    // future impl could make: printing the value, printing a PREFIX of it (the
    // "first 4 chars are fine" habit), and printing its LENGTH (which narrows a
    // brute force and, for a short key, is most of the secret).
    #[test]
    fn the_debug_of_a_resolved_config_leaks_no_secret_value_prefix_or_length() {
        const SENTINEL: &str = "quartzine-vellichor-sprocketful-lagniappe-widdershins";

        let mut cfg = test_config();
        cfg.settings.database_url = supplied(SENTINEL);
        cfg.settings.stash_signing_key = supplied(SENTINEL);
        cfg.settings.totp_enc_key = supplied(SENTINEL);
        cfg.settings.google_client_secret = supplied(SENTINEL);
        cfg.settings.smtp_password = supplied(SENTINEL);
        let rendered = format!("{cfg:?}");

        assert!(
            !rendered.contains(SENTINEL),
            "the secret value itself reached Debug output:\n{rendered}"
        );
        // Every prefix of three characters or more. Three, not one: a single
        // letter is in any English word the struct prints.
        for length in 3..=SENTINEL.len() {
            let prefix = &SENTINEL[..length];
            assert!(
                !rendered.contains(prefix),
                "a {length}-character prefix of the secret reached Debug output:\n{rendered}"
            );
        }
        // The length. 53 is not a value any other field in this fixture holds,
        // so a hit here is the secret's size and not a coincidence.
        assert_eq!(SENTINEL.len(), 53);
        assert!(
            !rendered.contains("53"),
            "the secret's length reached Debug output:\n{rendered}"
        );
        // The one-variable control: the SAME struct, the SAME formatting call,
        // and the secret's SOURCE tier does appear. Without it, a Debug impl
        // that printed nothing at all would satisfy every assertion above.
        assert!(
            rendered.contains("Secret(configured from Env)"),
            "the source tier must still be reported:\n{rendered}"
        );
        assert!(
            rendered.contains("Secret(<unset>)"),
            "an unsupplied secret must be visibly unset:\n{rendered}"
        );

        // Does NOT cover `--check-config` output, which is built by hand in
        // main.rs from `is_configured()` rather than from this impl, nor a
        // secret reaching a tracing field at a call site.
    }
}
