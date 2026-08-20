//! Live-PG tests for boot-time OAuth client reconciliation from config.
//!
//! These replace the `/admin/oauth-clients` route tests. What they pin:
//!
//! - a configured registration lands in `zeroship.oauth_clients` with only the
//!   HASH of its secret, and re-running is idempotent and picks up edits;
//! - `skip_consent` is DERIVED from the trusted set, never from the
//!   registration - the property the deleted route also held;
//! - an absent key leaves the table alone, while a present one prunes
//!   first-party rows it no longer names;
//! - pruning never touches a per-app (`oac_…`) client or the platform CLI
//!   client, because this is not their registrar.

#![allow(clippy::future_not_send)]

use std::collections::HashSet;

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;
use zeroship_control::oauth_clients::reconcile_oauth_clients;
use zeroship_core::auth::hash_api_key;
use zeroship_core::config::OauthClientRegistration;
use zeroship_core::device_grant::PLATFORM_CLI_CLIENT_ID;

use crate::common;

fn db_url() -> String {
    common::require_control_db()
}

async fn pg() -> Client {
    let (client, conn) = connect(&db_url(), NoTls).await.expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn registration(client_id: &str) -> OauthClientRegistration {
    OauthClientRegistration {
        client_id: client_id.to_string(),
        client_name: "ACME CI".to_string(),
        client_uri: Some("https://acme.example".to_string()),
        logo_uri: Some("https://acme.example/logo.png".to_string()),
        redirect_uris: vec!["https://ci.acme.example/oidc/callback".to_string()],
        scopes: vec![
            "apps:read".to_string(),
            "apps:deploy".to_string(),
            "env:read".to_string(),
        ],
        token_endpoint_auth_method: "client_secret_basic".to_string(),
        client_secret: Some("s3cr3t-from-the-mounted-overlay".to_string()),
        refresh_allowed: true,
    }
}

async fn count_client(pg: &Client, client_id: &str) -> i64 {
    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .expect("count oauth client");
    rows[0].get("n")
}

async fn drop_client(pg: &Client, client_id: &str) {
    let _ = pg
        .execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await;
}

#[compio::test]
async fn a_configured_registration_lands_hashed_and_reconciles_idempotently() {
    let pg = pg().await;
    let client_id = format!("cfg-client-{}", Uuid::new_v4().simple());
    let mut reg = registration(&client_id);
    drop_client(&pg, &client_id).await;

    let report = reconcile_oauth_clients(&pg, Some(std::slice::from_ref(&reg)), &HashSet::new())
        .await
        .expect("first reconcile");
    assert_eq!(report.registered, 1);

    let rows = pg
        .query(
            "SELECT client_name, redirect_uris, scopes, skip_consent, client_secret_hash, \
                    refresh_allowed, token_endpoint_auth_method \
             FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .expect("select client");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.get::<_, String>("client_name"), "ACME CI");
    assert_eq!(
        row.get::<_, Vec<String>>("redirect_uris"),
        vec!["https://ci.acme.example/oidc/callback".to_string()]
    );
    assert_eq!(row.get::<_, Vec<String>>("scopes").len(), 3);
    assert!(row.get::<_, bool>("refresh_allowed"));
    assert_eq!(
        row.get::<_, String>("token_endpoint_auth_method"),
        "client_secret_basic"
    );
    // The overlay's plaintext never reaches a column; only its hash does.
    let stored: Option<String> = row.get("client_secret_hash");
    let stored = stored.expect("a confidential client stores a hash");
    assert_eq!(stored, hash_api_key("s3cr3t-from-the-mounted-overlay"));
    assert_ne!(stored, "s3cr3t-from-the-mounted-overlay");

    // Config is the source of truth, so a boot after an EDIT carries the edit
    // rather than conflicting on the primary key.
    reg.client_name = "ACME CI (renamed)".to_string();
    reconcile_oauth_clients(&pg, Some(std::slice::from_ref(&reg)), &HashSet::new())
        .await
        .expect("second reconcile");
    let rows = pg
        .query(
            "SELECT client_name FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .expect("select renamed");
    assert_eq!(rows.len(), 1, "reconcile must upsert, not duplicate");
    assert_eq!(rows[0].get::<_, String>("client_name"), "ACME CI (renamed)");

    drop_client(&pg, &client_id).await;
    common::drain_pg().await;
}

/// `skip_consent` follows the TRUSTED set, and nothing in the registration can
/// move it. Both halves run the identical registration so the only variable is
/// the trusted set - asserting the `true` case alone would pass even if the
/// value were being read off the registration.
#[compio::test]
async fn skip_consent_is_derived_from_the_trusted_set_not_the_registration() {
    let pg = pg().await;
    let client_id = format!("cfg-trusted-{}", Uuid::new_v4().simple());
    let reg = registration(&client_id);

    for trusted in [false, true] {
        drop_client(&pg, &client_id).await;
        let set: HashSet<String> = if trusted {
            std::iter::once(client_id.clone()).collect()
        } else {
            HashSet::new()
        };
        reconcile_oauth_clients(&pg, Some(std::slice::from_ref(&reg)), &set)
            .await
            .expect("reconcile");
        let rows = pg
            .query(
                "SELECT skip_consent FROM zeroship.oauth_clients WHERE client_id = $1",
                &[&client_id],
            )
            .await
            .expect("select skip_consent");
        assert_eq!(
            rows[0].get::<_, bool>("skip_consent"),
            trusted,
            "skip_consent must track the trusted set (trusted = {trusted})"
        );
    }

    drop_client(&pg, &client_id).await;
    common::drain_pg().await;
}

/// An absent key is not an empty set. `None` manages nothing; `Some` is
/// authoritative and de-registers a first-party client it no longer names.
#[compio::test]
async fn absent_config_manages_nothing_and_present_config_prunes() {
    let pg = pg().await;
    let stale = format!("cfg-stale-{}", Uuid::new_v4().simple());
    let kept = format!("cfg-kept-{}", Uuid::new_v4().simple());
    drop_client(&pg, &stale).await;
    drop_client(&pg, &kept).await;

    reconcile_oauth_clients(
        &pg,
        Some(&[registration(&stale), registration(&kept)]),
        &HashSet::new(),
    )
    .await
    .expect("seed both");
    assert_eq!(count_client(&pg, &stale).await, 1);
    assert_eq!(count_client(&pg, &kept).await, 1);

    // Absent: the table is not this pass's business.
    let report = reconcile_oauth_clients(&pg, None, &HashSet::new())
        .await
        .expect("absent config");
    assert_eq!(report, Default::default(), "an absent key changes nothing");
    assert_eq!(count_client(&pg, &stale).await, 1);

    // Present: the named set is the whole first-party set.
    let report = reconcile_oauth_clients(&pg, Some(&[registration(&kept)]), &HashSet::new())
        .await
        .expect("prune pass");
    assert_eq!(report.registered, 1);
    assert!(report.pruned >= 1, "the unnamed first-party row is pruned");
    assert_eq!(count_client(&pg, &stale).await, 0);
    assert_eq!(count_client(&pg, &kept).await, 1);

    drop_client(&pg, &kept).await;
    common::drain_pg().await;
}

/// The two client families this reconciler does not register, it also must not
/// delete: per-app end-user clients come from the deploy path, and the platform
/// CLI client is reconciled by the auth service at its own boot. Pruning either
/// would make two services fight over the same rows.
#[compio::test]
async fn pruning_spares_per_app_clients_and_the_platform_cli_client() {
    let pg = pg().await;
    let app_client = zeroship_core::typed_id::app_oauth_client_id(&Uuid::now_v7());
    let named = format!("cfg-named-{}", Uuid::new_v4().simple());

    // Register both foreigners through the same path, then reconcile a config
    // that names NEITHER.
    let mut app_reg = registration(&app_client);
    app_reg.client_id.clone_from(&app_client);
    let mut cli_reg = registration(PLATFORM_CLI_CLIENT_ID);
    cli_reg.client_id = PLATFORM_CLI_CLIENT_ID.to_string();
    let cli_existed = count_client(&pg, PLATFORM_CLI_CLIENT_ID).await == 1;
    reconcile_oauth_clients(&pg, Some(&[app_reg, cli_reg]), &HashSet::new())
        .await
        .expect("seed foreigners");

    reconcile_oauth_clients(&pg, Some(&[registration(&named)]), &HashSet::new())
        .await
        .expect("prune pass");

    assert_eq!(
        count_client(&pg, &app_client).await,
        1,
        "a per-app oac_ client must survive a prune that does not name it"
    );
    assert_eq!(
        count_client(&pg, PLATFORM_CLI_CLIENT_ID).await,
        1,
        "the platform CLI client must survive a prune that does not name it"
    );

    drop_client(&pg, &app_client).await;
    drop_client(&pg, &named).await;
    if !cli_existed {
        drop_client(&pg, PLATFORM_CLI_CLIENT_ID).await;
    }
    common::drain_pg().await;
}

/// A rejected registration fails the whole pass. Boot treats this as fatal, so
/// the alternative would be a deployment that came up with a login surface
/// silently missing.
#[compio::test]
async fn an_invalid_registration_fails_the_pass() {
    let pg = pg().await;
    let client_id = format!("cfg-bad-{}", Uuid::new_v4().simple());
    let mut reg = registration(&client_id);
    reg.scopes = vec!["not:a:scope".to_string()];

    let err = reconcile_oauth_clients(&pg, Some(&[reg]), &HashSet::new())
        .await
        .expect_err("an unknown scope must fail the pass");
    assert!(err.contains("unknown scope"), "{err}");
    assert_eq!(
        count_client(&pg, &client_id).await,
        0,
        "a rejected registration writes no row"
    );
    common::drain_pg().await;
}
