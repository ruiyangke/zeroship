//! Account deletion through real stores, HTTP routes and reaper transactions.
//!
//! Each case owns a PostgreSQL container restored from the platform migrations.
//! Reaper scans and deliberately broken constraints cannot affect another case.

use compio_postgres::Client;

use zeroship_auth::cron::account_reaper::ControlAccess;

use crate::common::mock_control::{Answer, MockControl};

/// A control plane that always says "erasure may proceed", for the tests whose
/// subject is something else.
#[allow(clippy::future_not_send)]
async fn clear_control() -> (MockControl, ControlAccess) {
    let mock = MockControl::start(Answer::Clear).await;
    let access = ControlAccess {
        control_url: mock.base.clone(),
        keyring: mock.keyring(),
    };
    (mock, access)
}

/// Force a user's scheduled erasure into the past so the reaper's due-scan
/// selects it without waiting through the grace window.
#[allow(clippy::future_not_send)]
async fn backdate_schedule(db: &Client, user_id: &zeroship_core::UserId) {
    db.execute(
        "UPDATE zeroship.users \
         SET deletion_scheduled_for = NOW() - INTERVAL '1 minute' \
         WHERE id = $1",
        &[&user_id.as_str()],
    )
    .await
    .expect("backdate schedule");
}

/// Rows in `zeroship.audit_events` of one type for one user.
#[allow(clippy::future_not_send)]
async fn audit_detail(
    db: &Client,
    user_id: &zeroship_core::UserId,
    event_type: &str,
) -> Vec<serde_json::Value> {
    db.query(
        "SELECT detail FROM zeroship.audit_events \
         WHERE actor_user_id = $1 AND event_type = $2 ORDER BY id",
        &[&user_id.as_str(), &event_type],
    )
    .await
    .expect("read audit events")
    .iter()
    .map(|row| row.get::<_, serde_json::Value>("detail"))
    .collect()
}

#[path = "account_deletion/concurrency.rs"]
mod concurrency;
#[path = "account_deletion/http.rs"]
mod http;
#[path = "account_deletion/reaper.rs"]
mod reaper;
#[path = "account_deletion/requests.rs"]
mod requests;
