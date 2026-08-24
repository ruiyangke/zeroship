//! Connection-string parameters that only take effect in the STARTUP PACKET.
//!
//! `libpq_parameter_parity.rs` rules on which keys are ACCEPTED, and its header
//! names the failure it exists to prevent: "accepting a parameter and silently
//! ignoring it, which looks identical to support from the caller's side and
//! misconfigures them quietly". A parse-level table cannot see that failure,
//! because parsing is not the part that would be missing.
//!
//! These are the end-to-end halves for the parameters whose whole purpose is to
//! change the session the server opens. Each asks the SERVER what it received,
//! and each has a control showing the value is not simply the default.
//!
//! libpq reference, both forms measured against psql 16.14:
//!
//! ```text
//! $ psql "host=... options=-csearch_path=pg_catalog" -c 'show search_path'
//! pg_catalog
//! $ psql "postgres://...?options=-c%20search_path%3Dpg_catalog" -c 'show search_path'
//! pg_catalog
//! ```

use compio_postgres::{Client, Config, NoTls};
use std::str::FromStr;
use std::time::Duration;

#[allow(dead_code)]
mod common;

const WATCHDOG: Duration = Duration::from_secs(30);

fn base_url() -> String {
    common::test_url()
}

async fn connect_with(config: Config) -> Client {
    let (client, connection) = config
        .connect(common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&base_url(), &error));
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();
    client
}

async fn setting(client: &Client, name: &str) -> String {
    client
        .query_one(&format!("SHOW {name}"), &[])
        .await
        .unwrap_or_else(|error| panic!("SHOW {name}: {error}"))
        .get(0)
}

#[compio::test]
async fn options_reaches_the_backend_in_both_connection_string_forms() {
    compio::time::timeout(WATCHDOG, async {
        // The CONTROL first: without `options`, search_path is not pg_catalog.
        // Without this the assertions below would pass on a server that
        // happened to be configured that way already.
        let plain = connect_with(Config::from_str(&base_url()).expect("parse the base url")).await;
        let default = setting(&plain, "search_path").await;
        assert_ne!(
            default, "pg_catalog",
            "the control value already equals the one under test; this test \
             could not tell a working `options` from an ignored one"
        );

        // Keyword form.
        let mut keyword = Config::from_str(&base_url()).expect("parse the base url");
        keyword.options("-csearch_path=pg_catalog");
        let client = connect_with(keyword).await;
        assert_eq!(
            setting(&client, "search_path").await,
            "pg_catalog",
            "`options` was accepted and then not sent"
        );

        // URL query form, percent-encoded exactly as libpq requires.
        let url = format!(
            "{}{}options=-c%20search_path%3Dpg_catalog",
            base_url(),
            if base_url().contains('?') { "&" } else { "?" }
        );
        let client = connect_with(Config::from_str(&url).expect("parse the url form")).await;
        assert_eq!(
            setting(&client, "search_path").await,
            "pg_catalog",
            "`options` survived parsing from a URL but did not reach the server"
        );
    })
    .await
    .expect("startup options test exceeded its watchdog");
}

#[compio::test]
async fn application_name_from_the_connection_string_names_the_session() {
    compio::time::timeout(WATCHDOG, async {
        let chosen = common::test_object_name("startup-app");
        let mut config = Config::from_str(&base_url()).expect("parse the base url");
        config.application_name(&chosen);
        let client = connect_with(config).await;

        // Asked of the SERVER, not of the Config, so this fails if the value
        // is parsed and then left out of the startup packet.
        assert_eq!(setting(&client, "application_name").await, chosen);
    })
    .await
    .expect("application_name test exceeded its watchdog");
}
