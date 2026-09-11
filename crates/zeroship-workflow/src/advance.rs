use std::collections::HashSet;

use chrono::{DateTime, Utc};
use compio_postgres::{Client, GenericClient, NoTls, Row};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::errors::WorkflowError;
use crate::store::pg::WorkflowTables;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowAdvanceNackKind {
    Deadlock,
    Invalid,
    ApplyFailed,
    Backpressure,
    ClaimLost,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunDispatchRequest {
    pub run_id: String,
    pub app_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAdvanceRegistration {
    pub run_id: String,
    pub app_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_wake_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub terminal: bool,
}

impl WorkflowAdvanceRegistration {
    #[must_use]
    pub fn next(run_id: impl Into<String>, app_id: Uuid, next_wake_at: DateTime<Utc>) -> Self {
        Self {
            run_id: run_id.into(),
            app_id,
            next_wake_at: Some(next_wake_at),
            terminal: false,
        }
    }

    #[must_use]
    pub fn terminal(run_id: impl Into<String>, app_id: Uuid) -> Self {
        Self {
            run_id: run_id.into(),
            app_id,
            next_wake_at: None,
            terminal: true,
        }
    }

    #[must_use]
    pub fn preserve(run_id: impl Into<String>, app_id: Uuid) -> Self {
        Self {
            run_id: run_id.into(),
            app_id,
            next_wake_at: None,
            terminal: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAdvanceResponse {
    #[serde(default, skip_serializing_if = "is_false")]
    pub ack: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub nack: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_wake_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registrations: Vec<WorkflowAdvanceRegistration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nack_kind: Option<WorkflowAdvanceNackKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl WorkflowAdvanceResponse {
    #[must_use]
    pub fn ack(run_id: impl Into<String>, registrations: Vec<WorkflowAdvanceRegistration>) -> Self {
        let run_id = run_id.into();
        let next_wake_at = registrations
            .iter()
            .find(|registration| registration.run_id == run_id)
            .and_then(|registration| registration.next_wake_at);
        Self {
            ack: true,
            nack: false,
            run_id: Some(run_id),
            next_wake_at,
            registrations,
            nack_kind: None,
            reason: None,
        }
    }

    #[must_use]
    pub fn nack(
        run_id: impl Into<String>,
        kind: WorkflowAdvanceNackKind,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            ack: false,
            nack: true,
            run_id: Some(run_id.into()),
            next_wake_at: None,
            registrations: Vec::new(),
            nack_kind: Some(kind),
            reason: Some(reason.into()),
        }
    }

    #[must_use]
    pub fn is_ack(&self) -> bool {
        self.ack && !self.nack
    }

    #[must_use]
    pub fn is_nack(&self) -> bool {
        self.nack && !self.ack
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[allow(clippy::future_not_send)]
pub async fn collect_post_apply_registrations(
    db_url: &str,
    app_id: Uuid,
    run_id: &str,
    family: bool,
) -> Result<Vec<WorkflowAdvanceRegistration>, WorkflowError> {
    let conn = open_conn(db_url).await?;
    collect_post_apply_registrations_on_conn(&conn, app_id, run_id, family).await
}

pub async fn collect_post_apply_registrations_on_conn<C>(
    conn: &C,
    app_id: Uuid,
    run_id: &str,
    family: bool,
) -> Result<Vec<WorkflowAdvanceRegistration>, WorkflowError>
where
    C: GenericClient + Sync,
{
    let tables = WorkflowTables::for_app_id(&app_id);
    if family {
        collect_family_registrations(conn, &tables, run_id).await
    } else {
        collect_single_registration(conn, &tables, run_id).await
    }
}

#[allow(clippy::future_not_send)]
async fn open_conn(url: &str) -> Result<Client, WorkflowError> {
    let (client, connection) = compio_postgres::connect(url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            tracing::error!(error = %e, "workflow advance pg connection error");
        }
    })
    .detach();
    Ok(client)
}

async fn collect_single_registration<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<Vec<WorkflowAdvanceRegistration>, WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            &format!(
                "SELECT id, app_id, state, wake_at, cancel_requested, claimed_by, dispatch_nonce, waiting_step_key \
                   FROM {} \
                  WHERE id = $1",
                tables.runs
            ),
            &[&run_id],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(vec![WorkflowAdvanceRegistration::terminal(run_id, tables.app_id)]);
    };
    Ok(vec![registration_for_row(conn, tables, row).await?])
}

async fn collect_family_registrations<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<Vec<WorkflowAdvanceRegistration>, WorkflowError>
where
    C: GenericClient + Sync,
{
    let sql = format!(
        "WITH RECURSIVE ancestors AS ( \
                 SELECT id, parent_run_id \
                   FROM {runs} \
                  WHERE id = $1 \
                 UNION ALL \
                 SELECT p.id, p.parent_run_id \
                   FROM {runs} p \
                   JOIN ancestors a ON a.parent_run_id = p.id \
             ), root AS ( \
                 SELECT id \
                   FROM ancestors \
                  WHERE parent_run_id IS NULL \
                  LIMIT 1 \
             ), family AS ( \
                 SELECT id, app_id, state, wake_at, cancel_requested, claimed_by, dispatch_nonce, waiting_step_key, tree_depth \
                   FROM {runs} \
                  WHERE id = (SELECT id FROM root) \
                 UNION ALL \
                 SELECT c.id, c.app_id, c.state, c.wake_at, c.cancel_requested, c.claimed_by, c.dispatch_nonce, c.waiting_step_key, c.tree_depth \
                   FROM {runs} c \
                   JOIN family f ON c.parent_run_id = f.id \
             ) \
             SELECT id, app_id, state, wake_at, cancel_requested, claimed_by, dispatch_nonce, waiting_step_key \
               FROM family \
              ORDER BY tree_depth, id",
        runs = tables.runs
    );
    let rows = conn.query(&sql, &[&run_id]).await?;
    if rows.is_empty() {
        return Ok(vec![WorkflowAdvanceRegistration::terminal(run_id, tables.app_id)]);
    }

    let mut seen = HashSet::new();
    let mut registrations = Vec::new();
    for row in &rows {
        let registration = registration_for_row(conn, tables, row).await?;
        seen.insert(registration.run_id.clone());
        registrations.push(registration);
    }
    collect_parent_after_child_apply(conn, tables, run_id, &mut seen, &mut registrations).await?;
    collect_continued_as_new_successor(conn, tables, run_id, &mut seen, &mut registrations)
        .await?;
    Ok(registrations)
}

async fn collect_parent_after_child_apply<C>(
    conn: &C,
    tables: &WorkflowTables,
    child_run_id: &str,
    seen: &mut HashSet<String>,
    registrations: &mut Vec<WorkflowAdvanceRegistration>,
) -> Result<(), WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            &format!(
                "SELECT parent_run_id \
                   FROM {} \
                  WHERE id = $1 \
                    AND parent_run_id IS NOT NULL",
                tables.runs
            ),
            &[&child_run_id],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let parent_run_id: String = row.get("parent_run_id");
    if seen.contains(&parent_run_id) {
        return Ok(());
    }

    let parent_rows = conn
        .query(
            &format!(
                "SELECT id, app_id, state, wake_at, cancel_requested, claimed_by, dispatch_nonce, waiting_step_key \
                   FROM {} \
                  WHERE id = $1",
                tables.runs
            ),
            &[&parent_run_id],
        )
        .await?;
    if let Some(row) = parent_rows.first() {
        let registration = registration_for_row(conn, tables, row).await?;
        seen.insert(registration.run_id.clone());
        registrations.push(registration);
    }
    Ok(())
}

async fn collect_continued_as_new_successor<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    seen: &mut HashSet<String>,
    registrations: &mut Vec<WorkflowAdvanceRegistration>,
) -> Result<(), WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            &format!(
                "SELECT continued_as_new_run_id \
                   FROM {} \
                  WHERE id = $1 \
                    AND continued_as_new_run_id IS NOT NULL",
                tables.runs
            ),
            &[&run_id],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let successor_run_id: String = row.get("continued_as_new_run_id");
    if seen.contains(&successor_run_id) {
        return Ok(());
    }

    let successor_rows = conn
        .query(
            &format!(
                "SELECT id, app_id, state, wake_at, cancel_requested, claimed_by, dispatch_nonce, waiting_step_key \
                   FROM {} \
                  WHERE id = $1",
                tables.runs
            ),
            &[&successor_run_id],
        )
        .await?;
    if let Some(row) = successor_rows.first() {
        let registration = registration_for_row(conn, tables, row).await?;
        seen.insert(registration.run_id.clone());
        registrations.push(registration);
    }
    Ok(())
}

async fn registration_for_row<C>(
    conn: &C,
    tables: &WorkflowTables,
    row: &Row,
) -> Result<WorkflowAdvanceRegistration, WorkflowError>
where
    C: GenericClient + Sync,
{
    let run_id: String = row.get("id");
    let app_id: Uuid = row.get("app_id");
    let state: String = row.get("state");
    let mut wake_at: Option<DateTime<Utc>> = row.get("wake_at");
    let cancel_requested: bool = row.get("cancel_requested");
    let waiting_step_key: Option<String> = row.get("waiting_step_key");

    if is_schedulable_state(&state) {
        if cancel_requested {
            return Ok(WorkflowAdvanceRegistration::next(run_id, app_id, Utc::now()));
        }
        if state == "waiting" && wake_at.is_none() {
            wake_at =
                rearm_waiting_run_if_pending_signal(conn, tables, &run_id, waiting_step_key.as_deref())
                    .await?;
        }
        if let Some(wake_at) = wake_at {
            return Ok(WorkflowAdvanceRegistration::next(run_id, app_id, wake_at));
        }
        if state == "waiting"
            && waiting_run_has_live_resume_source(conn, tables, &run_id, waiting_step_key.as_deref()).await?
        {
            return Ok(WorkflowAdvanceRegistration::preserve(run_id, app_id));
        }
    }

    Ok(WorkflowAdvanceRegistration::terminal(run_id, app_id))
}

fn is_schedulable_state(state: &str) -> bool {
    matches!(
        state,
        "queued" | "running" | "sleeping" | "waiting" | "compensating"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WaitingStep {
    Sleep,
    WaitSignal {
        ordinal: i32,
        name: String,
        signal_type: String,
    },
    Child,
}

fn parse_waiting_step_key(key: &str) -> Result<WaitingStep, WorkflowError> {
    let parts: Vec<&str> = key.split(':').collect();
    match parts.as_slice() {
        ["sleep", ordinal, _name] => {
            let _ = ordinal.parse::<i32>().map_err(|_| {
                WorkflowError::Invalid(format!("invalid sleep waiting_step_key ordinal: {key}"))
            })?;
            Ok(WaitingStep::Sleep)
        }
        ["wait", ordinal, name, signal_type] | ["wait", ordinal, name, signal_type, _] => {
            let ordinal = ordinal.parse::<i32>().map_err(|_| {
                WorkflowError::Invalid(format!("invalid wait waiting_step_key ordinal: {key}"))
            })?;
            Ok(WaitingStep::WaitSignal {
                ordinal,
                name: (*name).to_string(),
                signal_type: (*signal_type).to_string(),
            })
        }
        ["child", ordinal, _name] => {
            let _ = ordinal.parse::<i32>().map_err(|_| {
                WorkflowError::Invalid(format!("invalid child waiting_step_key ordinal: {key}"))
            })?;
            Ok(WaitingStep::Child)
        }
        _ => Err(WorkflowError::Invalid(format!(
            "unrecognized waiting_step_key: {key}"
        ))),
    }
}

async fn waiting_run_has_live_resume_source<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    waiting_step_key: Option<&str>,
) -> Result<bool, WorkflowError>
where
    C: GenericClient + Sync,
{
    match waiting_step_key {
        Some(key) => match parse_waiting_step_key(key)? {
            WaitingStep::Sleep => Ok(false),
            WaitingStep::Child => waiting_run_has_running_step(conn, tables, run_id, "child").await,
            WaitingStep::WaitSignal { ordinal, name, .. } => {
                let rows = conn
                    .query(
                        &format!(
                            "SELECT 1 \
                               FROM {} \
                              WHERE run_id = $1 \
                                AND ordinal = $2 \
                                AND name = $3 \
                                AND kind = 'wait_signal' \
                                AND state = 'running' \
                              LIMIT 1",
                            tables.steps
                        ),
                        &[&run_id, &ordinal, &name],
                    )
                    .await?;
                if !rows.is_empty() {
                    return Ok(true);
                }
                waiting_run_has_subscription(conn, tables, run_id, Some(ordinal)).await
            }
        },
        None => waiting_run_has_running_resume_step_or_subscription(conn, tables, run_id).await,
    }
}

async fn waiting_run_has_running_resume_step_or_subscription<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<bool, WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            &format!(
                "SELECT 1 \
                   FROM {} \
                  WHERE run_id = $1 \
                    AND kind IN ('child', 'wait_signal') \
                    AND state = 'running' \
                  LIMIT 1",
                tables.steps
            ),
            &[&run_id],
        )
        .await?;
    if !rows.is_empty() {
        return Ok(true);
    }
    waiting_run_has_subscription(conn, tables, run_id, None).await
}

async fn waiting_run_has_running_step<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    kind: &str,
) -> Result<bool, WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            &format!(
                "SELECT 1 \
                   FROM {} \
                  WHERE run_id = $1 \
                    AND kind = $2 \
                    AND state = 'running' \
                  LIMIT 1",
                tables.steps
            ),
            &[&run_id, &kind],
        )
        .await?;
    Ok(!rows.is_empty())
}

async fn waiting_run_has_subscription<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    ordinal: Option<i32>,
) -> Result<bool, WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            &format!(
                "SELECT 1 \
                   FROM {} \
                  WHERE run_id = $1 \
                    AND ($2::integer IS NULL OR ordinal = $2) \
                  LIMIT 1",
                tables.subscriptions
            ),
            &[&run_id, &ordinal],
        )
        .await?;
    Ok(!rows.is_empty())
}

async fn rearm_waiting_run_if_pending_signal<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    waiting_step_key: Option<&str>,
) -> Result<Option<DateTime<Utc>>, WorkflowError>
where
    C: GenericClient + Sync,
{
    let Some(key) = waiting_step_key else {
        return Ok(None);
    };
    let pending = match parse_waiting_step_key(key)? {
        WaitingStep::Sleep => false,
        WaitingStep::WaitSignal { signal_type, .. } => {
            let rows = conn
                .query(
                    &format!(
                        "SELECT id \
                           FROM {} \
                          WHERE run_id = $1 \
                            AND type = $2 \
                            AND consumed_by IS NULL \
                          LIMIT 1",
                        tables.signals
                    ),
                    &[&run_id, &signal_type],
                )
                .await?;
            !rows.is_empty()
        }
        WaitingStep::Child => {
            let rows = conn
                .query(
                    &format!(
                        "SELECT sig.id \
                           FROM {} s \
                           JOIN {} sig \
                             ON sig.run_id = s.run_id \
                            AND sig.type = s.signal_type \
                            AND sig.consumed_by IS NULL \
                          WHERE s.run_id = $1 \
                            AND s.kind = 'child' \
                            AND s.state = 'running' \
                          LIMIT 1",
                        tables.steps, tables.signals
                    ),
                    &[&run_id],
                )
                .await?;
            !rows.is_empty()
        }
    };
    if !pending {
        return Ok(None);
    }

    let wake_at = Utc::now();
    let changed = conn
        .execute(
            &format!(
                "UPDATE {} \
                    SET wake_at = $2 \
                  WHERE id = $1 \
                    AND state = 'waiting' \
                    AND wake_at IS NULL",
                tables.runs
            ),
            &[&run_id, &wake_at],
        )
        .await?;
    if changed > 0 {
        Ok(Some(wake_at))
    } else {
        Ok(None)
    }
}
