//! Facts sources for tests that drive the CONSUMERS of Control's app-facts
//! endpoint.
//!
//! Neither of these is a second implementation of that endpoint, and neither
//! stands in for it: the endpoint itself is exercised against the real handler
//! in `zeroship-control`. What they supply here is the seam, so a test can
//! drive the policy ledger and the closing lane against real Control rows
//! ([`DatabaseAppFacts`]) or against an answer it dictates outright
//! ([`ScriptedAppFacts`]).

#![allow(
    dead_code,
    reason = "each including target uses the source its own cases need"
)]
#![allow(
    clippy::future_not_send,
    reason = "fixture clients stay on their compio runtime"
)]

use std::{cell::RefCell, rc::Rc};
use zeroship_core::{
    AppId,
    workflow_app_facts::{AppFactsResponse, AppSourceFacts, PlanSourceFacts, SourceWatermark},
};
use zeroship_workflow_manager::{
    Error,
    app_facts::{AppFactsFuture, AppFactsSource},
};

/// Control's rows, read straight from the fixture's database.
///
/// The watermark comes from the SAME statement as the rows, which is the one
/// property the consumer's fence depends on. A fixture that read it separately
/// would still order correctly by accident most of the time, and would stop
/// binding the thing the fence is about.
pub struct DatabaseAppFacts {
    client: compio_postgres::Client,
}

impl std::fmt::Debug for DatabaseAppFacts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("DatabaseAppFacts").finish()
    }
}

impl DatabaseAppFacts {
    pub async fn connect(url: &str) -> Rc<Self> {
        Rc::new(Self {
            client: super::platform::connect(url).await,
        })
    }
}

impl AppFactsSource for DatabaseAppFacts {
    fn observe<'a>(&'a self, apps: &'a [AppId]) -> AppFactsFuture<'a> {
        Box::pin(async move {
            if apps.is_empty() {
                return Err(Error::Invalid);
            }
            let ids: Vec<&str> = apps.iter().map(AppId::as_str).collect();
            let rows = self
                .client
                .query(
                    "SELECT a.id, a.plan_id, a.workflows_enabled, \
                            a.archived_at IS NOT NULL AS archived, \
                            a.deleted_at IS NOT NULL AS deleted, \
                            p.workflows_allowed, p.archived AS plan_archived, \
                            p.workflow_policy_json, \
                            (pg_current_wal_lsn() - '0/0'::pg_lsn)::bigint AS watermark \
                       FROM zeroship.apps a \
                       JOIN zeroship.plans p ON p.id = a.plan_id \
                      WHERE a.id = ANY($1)",
                    &[&ids],
                )
                .await
                .map_err(|_| Error::Unavailable)?;
            let watermark = match rows.first() {
                Some(row) => row.get::<_, i64>("watermark"),
                None => self
                    .client
                    .query_one(
                        "SELECT (pg_current_wal_lsn() - '0/0'::pg_lsn)::bigint AS watermark",
                        &[],
                    )
                    .await
                    .map_err(|_| Error::Unavailable)?
                    .get::<_, i64>("watermark"),
            };
            let facts = rows
                .iter()
                .map(|row| AppSourceFacts {
                    app_id: AppId::parse(row.get::<_, &str>("id")).unwrap(),
                    plan_id: row.get::<_, &str>("plan_id").to_owned(),
                    workflows_enabled: row.get("workflows_enabled"),
                    archived: row.get("archived"),
                    deleted: row.get("deleted"),
                    plan: PlanSourceFacts {
                        workflows_allowed: row.get("workflows_allowed"),
                        archived: row.get("plan_archived"),
                        workflow_policy: row.get("workflow_policy_json"),
                    },
                })
                .collect();
            Ok(AppFactsResponse {
                watermark: SourceWatermark::new(watermark).ok_or(Error::Unavailable)?,
                apps: facts,
            })
        })
    }
}

/// An answer a test dictates, including its watermark.
///
/// This is what lets a case put a STALE answer in front of the publication
/// fence. A lagging replica or a cache in front of the route is what would do
/// that in production; neither can be arranged in a fixture, and the fence is
/// what the deployment relies on when one appears.
#[derive(Debug, Default)]
pub struct ScriptedAppFacts {
    answer: RefCell<Option<AppFactsResponse>>,
}

impl ScriptedAppFacts {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    /// The answer the next observation returns. An unset answer is unavailable
    /// rather than empty, so a case that forgets to script one fails loudly.
    pub fn set(&self, response: AppFactsResponse) {
        *self.answer.borrow_mut() = Some(response);
    }
}

impl AppFactsSource for ScriptedAppFacts {
    fn observe<'a>(&'a self, apps: &'a [AppId]) -> AppFactsFuture<'a> {
        let scripted = self.answer.borrow().clone();
        Box::pin(async move {
            if apps.is_empty() {
                return Err(Error::Invalid);
            }
            scripted.ok_or(Error::Unavailable)
        })
    }
}
