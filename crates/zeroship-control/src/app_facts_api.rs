//! Control's app and plan facts for the workflow service.
//!
//! The workflow service used to read `zeroship.apps` and `zeroship.plans`
//! through its own database binding. It reads them here instead, so the policy
//! columns and the whole plan catalog need no grant on a service login.
//!
//! The answer carries a watermark, and the ONE thing this handler must get
//! right is where that watermark comes from: it is selected in the SAME
//! statement as the facts, so it is at least the write position of every change
//! visible in that statement's snapshot. Taken before the read it would
//! under-state and cause spurious refusals; taken in a separate statement it
//! would order nothing. The consumer's fence is only as good as that.

#![expect(
    clippy::future_not_send,
    reason = "ORM and HTTP stay on their compio thread"
)]

use crate::AppState;
use ntex::web::{
    self,
    types::{Json, State},
};
use std::{sync::Arc, time::Duration};
use zeroship_core::{
    app_id::AppId,
    service_assertion::presented_issuer,
    service_identity::{AuthError, endpoints},
    service_peers::{WORKFLOW_SERVICE_NAME, service_issuer},
    workflow_app_facts::{
        AppFactsRequest, AppFactsResponse, AppSourceFacts, MAX_APPS_PER_REQUEST, PlanSourceFacts,
        SourceWatermark,
    },
    workflow_coordination::{Failure, FailureCode},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// A request is a list of app ids and nothing else, and the list is bounded by
/// the wire contract. The body limit is the transport's backstop for a caller
/// that ignores it, not the bound itself.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

pub fn configure(config: &mut web::ServiceConfig) {
    config.service(
        web::resource(endpoints::CONTROL_APP_FACTS.path_template())
            .state(web::types::JsonConfig::default().limit(MAX_REQUEST_BYTES))
            .route(web::post().to(app_facts)),
    );
}

async fn app_facts(
    request: web::HttpRequest,
    state: State<Arc<AppState>>,
    body: web::types::Payload,
) -> web::HttpResponse {
    respond(handle(request, state, body).await)
}

async fn handle(
    request: web::HttpRequest,
    state: State<Arc<AppState>>,
    body: web::types::Payload,
) -> Result<AppFactsResponse, FailureCode> {
    compio::time::timeout(REQUEST_TIMEOUT, async {
        let authorization = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        let issuer = presented_issuer(authorization).ok_or(FailureCode::Unauthenticated)?;
        let role = service_issuer(WORKFLOW_SERVICE_NAME).map_err(|_| FailureCode::Unavailable)?;
        // Whole-issuer equality, not principal equality: this is a role-arity
        // credential, so an instance-arity `svc/workflow/<id>` is refused.
        if issuer != role {
            return Err(FailureCode::Unauthenticated);
        }
        crate::internal::verify_service_caller(&state, authorization, endpoints::CONTROL_APP_FACTS)
            .await
            .map_err(|error| match error {
                AuthError::StoreUnavailable => FailureCode::Unavailable,
                _ => FailureCode::Unauthenticated,
            })?;
        // Only the verified workflow role reaches body decoding.
        let mut body = body.into_inner();
        let query =
            <Json<AppFactsRequest> as web::FromRequest<web::error::DefaultError>>::from_request(
                &request, &mut body,
            )
            .await
            .map_err(|error| match error {
                web::error::JsonPayloadError::Overflow => FailureCode::RequestTooLarge,
                _ => FailureCode::Invalid,
            })?
            .into_inner();
        read(&state, &query.app_ids).await
    })
    .await
    .map_err(|_| FailureCode::Unavailable)?
}

async fn read(state: &AppState, apps: &[AppId]) -> Result<AppFactsResponse, FailureCode> {
    if apps.is_empty() || apps.len() > MAX_APPS_PER_REQUEST {
        return Err(FailureCode::Invalid);
    }
    let ids: Vec<&str> = apps.iter().map(AppId::as_str).collect();
    let connection = state
        .registry
        .conn()
        .await
        .map_err(|_| FailureCode::Unavailable)?;
    // ONE statement, so the watermark and the rows share a snapshot. Every
    // change visible to this SELECT committed before `pg_current_wal_lsn()`
    // was evaluated, which is what lets the caller order two answers. Splitting
    // this into two statements, or reading the position first, breaks that and
    // the caller cannot tell.
    //
    // INNER JOIN on the plan, matching what the policy ledger required when it
    // read these tables itself. It cannot drop a live app: `apps_plan_fk` is
    // `ON DELETE RESTRICT`, so an app always names a plan row that exists.
    // Absence in the answer therefore means the APP has no row, which is the
    // one meaning the consumers are built around.
    let rows = connection
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
        .map_err(|_| FailureCode::Unavailable)?;
    // An empty result still owes the caller a watermark, and the query above
    // produces no row to carry one. Read it separately in that case only: with
    // no facts to order, there is nothing for a separate statement to skew.
    let watermark = match rows.first() {
        Some(row) => row.try_get::<_, i64>("watermark").map_err(unavailable)?,
        None => connection
            .query_one(
                "SELECT (pg_current_wal_lsn() - '0/0'::pg_lsn)::bigint AS watermark",
                &[],
            )
            .await
            .map_err(|_| FailureCode::Unavailable)?
            .try_get::<_, i64>("watermark")
            .map_err(unavailable)?,
    };
    let watermark = SourceWatermark::new(watermark).ok_or(FailureCode::Unavailable)?;
    let mut facts = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: &str = row.try_get("id").map_err(unavailable)?;
        let policy: Option<serde_json::Value> =
            row.try_get("workflow_policy_json").map_err(unavailable)?;
        facts.push(AppSourceFacts {
            app_id: AppId::parse(id).map_err(|_| FailureCode::Unavailable)?,
            plan_id: row
                .try_get::<_, &str>("plan_id")
                .map_err(unavailable)?
                .to_owned(),
            workflows_enabled: row.try_get("workflows_enabled").map_err(unavailable)?,
            archived: row.try_get("archived").map_err(unavailable)?,
            deleted: row.try_get("deleted").map_err(unavailable)?,
            plan: PlanSourceFacts {
                workflows_allowed: row.try_get("workflows_allowed").map_err(unavailable)?,
                archived: row.try_get("plan_archived").map_err(unavailable)?,
                workflow_policy: policy,
            },
        });
    }
    Ok(AppFactsResponse {
        watermark,
        apps: facts,
    })
}

fn unavailable(_: compio_postgres::Error) -> FailureCode {
    FailureCode::Unavailable
}

fn respond(result: Result<AppFactsResponse, FailureCode>) -> web::HttpResponse {
    match result {
        Ok(response) => web::HttpResponse::Ok().json(&response),
        Err(code) => {
            let status = match code {
                FailureCode::Invalid => 400,
                FailureCode::Unauthenticated => 401,
                FailureCode::Denied => 403,
                FailureCode::Conflict => 409,
                FailureCode::RequestTooLarge => 413,
                FailureCode::Capacity => 429,
                FailureCode::Unavailable => 503,
            };
            web::HttpResponse::build(
                ntex::http::StatusCode::from_u16(status)
                    .unwrap_or(ntex::http::StatusCode::SERVICE_UNAVAILABLE),
            )
            .force_close()
            .json(&Failure { code })
        }
    }
}
