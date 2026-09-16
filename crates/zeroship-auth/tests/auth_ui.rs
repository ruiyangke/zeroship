//! The auth UI browser suite, scoped to the crate that owns the surface.
//!
//! `tests/web/` holds the Playwright specs; this module gives them a live
//! server and a real migrated database through the SAME `common::database`
//! fixture the HTTP cases use, so there is one provisioning path rather than a
//! shell script's second copy of it.
//!
//! IGNORED BY DEFAULT, deliberately. A browser must not become a prerequisite of
//! `cargo test -p zeroship-auth`, and the tree does not put required cases
//! behind opt-in cargo features. Run it inside `nix develop`, which supplies the
//! version-matched Chromium and the `playwright` CLI:
//!
//! ```text
//! cargo test -p zeroship-auth --test main -- --ignored auth_ui
//! ```
//!
//! WHAT THIS DOES NOT COVER: the `zeroship-auth` binary's own startup - argument
//! parsing, secret-file loading, the readiness gate. Those are process-level
//! contracts owned by `config_env_tier` and `check_config_smtp_test`. Spawning
//! the binary from here would mean a nested `cargo build` for the CLI that
//! produces its secrets, which is the anti-pattern the fixture review names.

use crate::common;
use common::{auth_server::AuthServer, database::Database};

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use zeroship_mailer::{Email, Mailer, MailerError, MessageId};

/// The registered client the specs authorize as.
///
/// Shared with `tests/fixtures/auth_ui_ids.json` only in spirit: the ids are
/// stable strings so a spec failure names a client an operator can find.
const APP_ID: &str = "app_0000000000000000000000001";
const CLIENT_ID: &str = "oac_0000000000000000000000001";
const REDIRECT_URI: &str = "http://127.0.0.1:9999/native-cb";
const SECTOR: &str = "https://auth-ui.zeroship.test";

/// A `Mailer` that appends the same `=== MAIL ===` block `StdoutMailer` prints,
/// to a file the specs can read.
///
/// THE BLOCK IS THE CONTRACT, not a debugging convenience:
/// `tests/web/helpers.ts` greps it for `RCPT TO (envelope): <recipient>`, the
/// verification subject line, and a `/verify` URL whose token is base64url. That
/// is how a browser obtains a real mailed link. `StdoutMailer` writes to this
/// process's stderr, which a browser cannot read, so the same fields go to the
/// path handed over as `ZEROSHIP_AUTH_UI_AUTH_LOG`.
#[derive(Debug)]
struct EnvelopeFileMailer {
    path: PathBuf,
}

#[async_trait::async_trait]
impl Mailer for EnvelopeFileMailer {
    async fn send(
        &self,
        _db: &compio_postgres::Client,
        msg: Email,
    ) -> Result<MessageId, MailerError> {
        use std::io::Write as _;

        let header_to = msg.header_to.as_ref().unwrap_or(&msg.to);
        let block = format!(
            "\n=== MAIL ===\nTo: {} <{}>\nRCPT TO (envelope): {}\nFrom: {} <{}>\nReply-To: {}\nReturn-Path: {}\nSubject: {}\nHeaders: {:?}\n\n{}\n=== END ===\n",
            header_to.name.as_deref().unwrap_or(""),
            header_to.email,
            msg.to.email,
            msg.from.name.as_deref().unwrap_or(""),
            msg.from.email,
            msg.reply_to.as_ref().map_or("", |a| a.email.as_str()),
            msg.envelope_from.as_deref().unwrap_or(""),
            msg.subject,
            msg.headers,
            msg.text,
        );
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|why| MailerError::Transport(format!("open the auth log: {why}")))?;
        file.write_all(block.as_bytes())
            .map_err(|why| MailerError::Transport(format!("append the auth log: {why}")))?;
        Ok(MessageId(format!(
            "envelope-file-{}",
            uuid::Uuid::new_v4().simple()
        )))
    }
}

/// Register the per-app client the consent spec authorizes as.
///
/// The specs sign up their own users; what they cannot create is the app
/// registration, which is platform data. Mirrors the rows the Rust consent cases
/// seed, minus the user.
async fn seed_browser_client(pg: &compio_postgres::Client) {
    let project_id = common::unowned_project(pg).await;
    pg.execute(
        "INSERT INTO zeroship.plans \
            (id, name, runtime_limits_json, assignable_by_creator) \
         VALUES ('free', 'Free', '{}'::jsonb, TRUE) \
         ON CONFLICT (id) DO NOTHING",
        &[],
    )
    .await
    .expect("seed free plan");
    pg.execute(
        "INSERT INTO zeroship.apps (id, name, project_id, organization_id) \
         SELECT $1, 'auth UI consent fixture', p.id, p.organization_id \
           FROM zeroship.projects p WHERE p.id = $2",
        &[&APP_ID, &project_id],
    )
    .await
    .expect("seed browser app");
    pg.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes, skip_consent) \
         VALUES ($1, 'Auth UI consent fixture', $2, $3, FALSE)",
        &[
            &CLIENT_ID,
            &vec![REDIRECT_URI.to_string()],
            &vec![
                "openid".to_string(),
                "profile".to_string(),
                "email".to_string(),
                "read:notes".to_string(),
            ],
        ],
    )
    .await
    .expect("seed browser oauth client");
    pg.execute(
        "INSERT INTO zeroship.app_oauth_clients (app_id, client_id, sector_identifier) \
         VALUES ($1, $2, $3)",
        &[&APP_ID, &CLIENT_ID, &SECTOR],
    )
    .await
    .expect("seed browser app client");
    pg.execute(
        "INSERT INTO zeroship.app_scope_defs (app_id, scope_id, label, description) \
         VALUES ($1, 'read:notes', 'Read notes', 'Read your notes')",
        &[&APP_ID],
    )
    .await
    .expect("seed browser scope definition");
}

/// Run the Playwright specs under `tests/web/`, capturing its output.
///
/// CAPTURED RATHER THAN INHERITED so a failing run names the spec that failed
/// and why. A child that inherits stdio is silenced by the test harness, which
/// turns every failure into "the specs failed" and sends the next reader to the
/// source to find out which one.
fn run_playwright(
    base_url: &str,
    auth_log: &str,
    results_json: &str,
) -> std::process::Output {
    let web = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/web");
    assert!(
        web.join("playwright.config.ts").is_file(),
        "the browser specs are expected at {}",
        web.display()
    );
    Command::new("playwright")
        .arg("test")
        .current_dir(&web)
        .env("ZEROSHIP_AUTH_UI_BASE_URL", base_url)
        .env("ZEROSHIP_AUTH_UI_AUTH_LOG", auth_log)
        .env("ZEROSHIP_AUTH_UI_RESULTS_JSON", results_json)
        .env("ZEROSHIP_AUTH_UI_OIDC_CLIENT_ID", CLIENT_ID)
        .env("ZEROSHIP_AUTH_UI_OIDC_REDIRECT_URI", REDIRECT_URI)
        .output()
        .expect("spawn `playwright test`; run this inside `nix develop`")
}

/// Wait until `/readyz` answers 200, bounded.
///
/// THE FIRST REQUEST PAYS THE CONNECT, and the readiness gate bounds its own
/// probe (`readiness::PROBE_TIMEOUT`) and caches the outcome. A probe that
/// arrives before that connect completes therefore reports NOT ready and is
/// cached that way - which is a correct answer to "can I serve traffic yet",
/// and the wrong thing to hand to `global-setup.ts`, whose first act is to
/// require a ready server. The retired launcher waited here for the same reason.
async fn wait_until_ready(server: &AuthServer) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let response = server
            .http
            .request(http::Method::GET, format!("{}/readyz", server.auth_base))
            .expect("build the readiness probe")
            .send()
            .await;
        if let Ok(response) = response {
            if response.status().as_u16() == 200 {
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the auth server never became ready"
        );
        compio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// The browser tier's only entry point.
///
/// A failure here must NOT be read as "the measurement could not run": the
/// fixture panics on provisioning trouble, so reaching the assertion means the
/// server and the database were live and Playwright genuinely disagreed.
#[ntex::test]
#[allow(clippy::future_not_send)]
#[ignore = "needs a browser and the flake's playwright; run with `cargo test -p zeroship-auth --test main -- --ignored auth_ui`"]
async fn the_auth_ui_browser_specs_pass() {
    Database::run(async |database| {
        let scratch = tempfile::tempdir().expect("create the browser suite scratch directory");
        let auth_log = scratch.path().join("auth.log");
        // The specs' `global-setup.ts` requires this path to EXIST before the
        // run, and the mailer only creates it on the first send - which a spec
        // performs long after setup. The retired launcher got this for free from
        // its `>"$AUTH_LOG"` redirection.
        std::fs::File::create(&auth_log).expect("create the auth log");
        let results_json = scratch.path().join("playwright-results.json");

        // SEEDING GOES THROUGH THE ADMIN CONNECTION. `server.pg` is the migrated
        // auth role, which is deliberately denied INSERT on
        // `zeroship.organizations` - the refusal is the scoping working, not a
        // fixture defect.
        let db = database.connect().await;
        seed_browser_client(&db).await;

        let server = AuthServer::with_mailer(
            database,
            Arc::new(EnvelopeFileMailer {
                path: auth_log.clone(),
            }),
        )
        .await;
        wait_until_ready(&server).await;

        let run = run_playwright(
            &server.auth_base,
            auth_log.to_str().expect("UTF-8 auth log path"),
            results_json.to_str().expect("UTF-8 results path"),
        );
        assert!(
            run.status.success(),
            "the auth UI browser specs failed ({})\nresults at {}\n--- playwright stdout ---\n{}\n--- playwright stderr ---\n{}",
            run.status,
            results_json.display(),
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr),
        );
    })
    .await;
}
