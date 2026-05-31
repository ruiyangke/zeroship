//! Audit event insert into `zeroship.audit_events`.

use compio_postgres::GenericClient;
use serde_json::Value;

use crate::error::{AuthError, Result};

/// Insert one row into `zeroship.audit_events`.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
#[allow(clippy::too_many_arguments)]
pub async fn insert(
    conn: &(impl GenericClient + Sync),
    event_type: &str,
    outcome: &str,
    user_id: Option<&uuid::Uuid>,
    client_id: Option<&str>,
    request_id: Option<&str>,
    ip: Option<std::net::IpAddr>,
    user_agent: Option<&str>,
    auth_method: Option<&str>,
    detail: &Value,
) -> Result<()> {
    conn.execute(
        "INSERT INTO zeroship.audit_events \
            (event_type, outcome, user_id, client_id, request_id, ip, user_agent, auth_method, detail) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        &[
            &event_type,
            &outcome,
            &user_id,
            &client_id,
            &request_id,
            &ip,
            &user_agent,
            &auth_method,
            &detail,
        ],
    )
    .await
    .map_err(|e| AuthError::Db(format!("audit insert: {e}")))?;
    Ok(())
}
