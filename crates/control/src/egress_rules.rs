//! Creator self-service for an app's egress rules.
//!
//! They cover every RAW BYTE STREAM the app can open - `node:net`, `node:tls`
//! and outbound `WebSocket` - through one rule set and one evaluator. `fetch`
//! is the exception and reaches any public host with no rule at all.
//!
//! `zeroship.app_egress_rules` is the authoritative rule set the registry
//! projects into every runtime (`Registry::get_versions`). This module is the
//! only writer of that table, and the creator who owns the app is the author:
//! `GET`/`POST`/`DELETE /api/apps/{app_id}/egress-rules`, authorized as
//! `env:read`/`env:write` on `Resource::App` through the same `app_members`
//! path as vars and secrets.
//!
//! A rule is a **verdict**, a **destination** and a **port**. The destination
//! is an exact DNS name or an address range, and which one it is comes from the
//! value's own grammar rather than from a field the caller sets
//! ([`zeroship_core::net_policy::Destination::parse`]). Rules are an unordered
//! SET: any matching reject refuses, otherwise any matching accept admits,
//! otherwise the connect is refused. Nothing here stores a position and nothing
//! may start to.
//!
//! Three things bound what a creator can write, and none of them is a human
//! reviewer:
//!
//! 1. **Deny by default.** An app with no accept rule gets `NetPolicy::Denied`:
//!    it cannot resolve `node:net` at all and every outbound `WebSocket` is
//!    refused. Nothing here changes that.
//! 2. **Shape.** Every rule goes through [`EgressRule::parse`], which refuses a
//!    wildcard outright (they are no longer representable), refuses a bare IP
//!    literal in favour of the `/32` or `/128` that says which check decides it,
//!    canonicalises both forms before storage, and holds accept ranges to the
//!    IPv4 `/16` and IPv6 `/32` prefix floors. A reject range has no floor:
//!    `0.0.0.0/0` is the strictest rule in the grammar.
//! 3. **Plan caps.** `max_grants` bounds the number of ACCEPT rules;
//!    `max_sockets` and `egress_ceiling_bytes` bound concurrency and volume.
//!    All three come from the plan catalog and a creator can never raise them.
//!    Reject rules are capped separately by [`MAX_REJECT_RULES`], which is a
//!    resource bound and not a security one.
//!
//! What this deliberately does NOT claim: it is not a boundary against a
//! malicious creator. `fetch` reaches any public host with no rule at all, so
//! raw-stream narrowness is a compromised-dependency blast-radius control
//! (`zeroship_core::net_policy`), and the malicious-creator controls are
//! attribution, spend enforcement, and egress ceilings.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use compio_postgres::Client;
use ntex::web::{
    self,
    types::{Json, Path, State},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use zeroship_authz::{Action as AuthzAction, Resource};
use zeroship_core::net_policy::{normalize_name, Destination, EgressRule, Verdict};
use zeroship_core::types::{AppNetPolicyLimits, FREE_TIER_NET_POLICY_LIMITS};

use crate::audit::{self, Action as AuditAction, AuditEntry};
use crate::authz_guard::AuthzGuard;
use crate::env_handlers::admin_rate_limit;
use crate::http_util;
use crate::AppState;

/// An egress-rule body is a verdict, a destination, a port and a short note.
/// 8 KiB is far more than that and far less than anything worth streaming.
pub const EGRESS_RULE_PAYLOAD_BYTES: usize = 8 * 1024;

const MAX_NOTE_CHARS: usize = 200;

/// How many REJECT rules one app may hold.
///
/// **This is a resource bound, not a security one, and it must not be
/// "hardened" downward on a security argument it does not have.** A reject rule
/// can only ever narrow what an app reaches, so capping rejects caps safety. It
/// is capped only because `Registry::get_versions` reads every rule row into
/// `AppNetPolicy`, which rides `RouteEntry` on the gateway's poll: an unbounded
/// reject list is a denial of service against that projection.
///
/// The number is derived from a measurement rather than picked, and
/// `reject_rule_projection_cost_bounds_the_cap` is the measurement: it
/// serializes one `NetEgressEntry` holding the longest range the grammar admits
/// and asserts both the per-rule cost and that this cap stays inside a stated
/// budget. The budget is **64 KiB of reject rules per app**; the per-rule cost
/// the test pins is what turns that budget into this number. The budget itself
/// is a judgement and is stated as one - what was missing before was the
/// measurement, not the opinion.
pub const MAX_REJECT_RULES: u32 = 500;

/// The serialized-bytes budget [`MAX_REJECT_RULES`] is derived from. Only the
/// derivation reads it, so it lives with the test that performs it.
#[cfg(test)]
const REJECT_RULE_PROJECTION_BUDGET_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct EgressRuleBody {
    /// Required. A body without it is a 400 rather than an implied accept: a
    /// defaulted verdict is a rule whose meaning depends on which side of a
    /// client upgrade you are on, and accept is the wrong thing to guess.
    pub verdict: Verdict,
    /// An exact DNS name, or an address range in CIDR form. The kind is
    /// inferred from the grammar; a wildcard is refused.
    pub destination: String,
    pub port: u16,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteEgressRuleBody {
    pub destination: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct EgressRuleRecord {
    pub app_id: Uuid,
    pub verdict: Verdict,
    /// `"name"` or `"cidr"`, derived from the destination rather than stored
    /// twice as a fact the caller could contradict.
    pub kind: &'static str,
    pub destination: String,
    pub port: u16,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub note: Option<String>,
    /// The verdict that applies once the whole SET is read, which is not always
    /// the one on this row: an accept range wholly inside a reject range at the
    /// same port is dead, and this is where that becomes visible.
    ///
    /// Only STATICALLY decidable domination is reflected. Whether a name lands
    /// inside a rejected range depends on what it resolves to at connect time,
    /// which this endpoint does not look up, so a name rule always reports its
    /// own verdict.
    pub effective_verdict: Verdict,
}

#[derive(Clone, Debug, Serialize)]
pub struct EgressRequest {
    pub host: String,
    pub port: u16,
    pub reason: String,
}

/// The plan ceiling on egress, echoed on every list so a creator can see what
/// they are working against without reading the plan catalog.
#[derive(Debug, Serialize)]
pub struct EgressRuleLimits {
    /// Bounds ACCEPT rules only. A reject can only narrow, so it is never
    /// charged here.
    pub max_accept_rules: u32,
    pub used_accept_rules: u32,
    pub max_reject_rules: u32,
    pub used_reject_rules: u32,
    pub max_sockets: u32,
    pub egress_ceiling_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct EgressRuleList {
    pub app_id: Uuid,
    pub rules: Vec<EgressRuleRecord>,
    /// The manifest's `net.requests` hints, inert until a rule exists.
    pub requests: Vec<EgressRequest>,
    /// Hints with no matching accept rule - what the creator has yet to allow.
    pub pending_requests: Vec<EgressRequest>,
    pub limits: EgressRuleLimits,
}

#[derive(Debug, Serialize)]
pub struct SetEgressRuleResult {
    pub rule: EgressRuleRecord,
    /// Non-null exactly once per app: on its FIRST accept rule for an address
    /// range. See [`first_range_accept_notice`].
    pub notice: Option<String>,
}

/// The words a creator gets when their first address-range accept rule moves
/// their app out of the class that cannot leak a name and into the class that
/// can.
///
/// This is the ONLY place a creator meets that trade. A name rule is decided
/// before any lookup, so an app whose rules are all names never resolves a
/// destination it is about to refuse. A range rule can only be decided against
/// a resolved address, so a connect to a name no rule admits, at a port
/// carrying an accept range, is resolved FIRST and refused afterwards - and
/// that query reaches the nameserver of whoever owns the name, including a name
/// a compromised dependency chose. Nothing in the product surfaces this
/// otherwise, which is why it is a required response field and not a doc line.
fn first_range_accept_notice(port: u16) -> String {
    format!(
        "This is the first address-range accept rule on this app, and it changes how the app \
         refuses destinations it does not allow. Until now every rule named a host exactly, so \
         a connect to a host you had not allowed was refused without looking it up and the name \
         never left the machine. An address range can only be checked against a resolved \
         address. From now on, a connect on port {port} to a name that matches none of your name \
         rules is resolved first and refused afterwards, so the lookup for that name reaches the \
         nameserver of whoever owns it - including a name your dependencies chose rather than \
         you. Rules that name hosts exactly do not do this, and neither do accept ranges on other \
         ports. Deleting every address-range accept rule on port {port} restores the earlier \
         behaviour for that port."
    )
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum EgressRuleError {
    AppNotFound,
    /// The rule failed shape validation; carries the validator's message.
    Invalid(String),
    /// The app already holds its cap of rules of this verdict.
    CapExceeded {
        verdict: Verdict,
        max: u32,
        current: u32,
    },
    /// Delete targeted a row that does not exist.
    RuleNotFound,
    Db,
}

impl EgressRuleError {
    pub fn into_response(self) -> web::HttpResponse {
        match self {
            Self::AppNotFound => {
                web::HttpResponse::NotFound().json(&json!({"error": "app not found"}))
            }
            Self::Invalid(detail) => web::HttpResponse::BadRequest()
                .json(&json!({"error": "invalid egress rule", "detail": detail})),
            Self::CapExceeded {
                verdict,
                max,
                current,
            } => web::HttpResponse::Conflict().json(&json!({
                "error": "egress rule limit reached",
                "detail": format!(
                    "this app may hold {max} {} rules and already holds {current}",
                    verdict.as_str()
                ),
                "verdict": verdict,
                "max_rules": max,
                "used_rules": current,
            })),
            Self::RuleNotFound => {
                web::HttpResponse::NotFound().json(&json!({"error": "egress rule not found"}))
            }
            Self::Db => {
                web::HttpResponse::InternalServerError().json(&json!({"error": "database error"}))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// The app's plan-derived net caps. A missing app is `AppNotFound`; a missing
/// or corrupt plan row falls back to the free tier, never to "unbounded".
async fn plan_net_limits(pg: &Client, app_id: Uuid) -> Result<AppNetPolicyLimits, EgressRuleError> {
    let rows = pg
        .query(
            "SELECT p.net_policy_limits_json \
             FROM zeroship.apps a \
             LEFT JOIN zeroship.plans p ON p.id = a.plan_id \
             WHERE a.id = $1",
            &[&app_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, app_id = %app_id, "control: app plan lookup failed");
            EgressRuleError::Db
        })?;
    let Some(row) = rows.first() else {
        return Err(EgressRuleError::AppNotFound);
    };
    let json: Option<serde_json::Value> = row.get("net_policy_limits_json");
    Ok(json
        .and_then(|j| {
            serde_json::from_value::<AppNetPolicyLimits>(j)
                .map_err(|err| {
                    tracing::warn!(
                        app_id = %app_id,
                        error = %err,
                        "control: plan net_policy_limits_json parse failure - using free-tier caps"
                    );
                })
                .ok()
        })
        .unwrap_or(FREE_TIER_NET_POLICY_LIMITS))
}

/// How many rules of one verdict the app currently holds.
async fn count_rules(
    pg: &Client,
    app_id: Uuid,
    verdict: Verdict,
) -> Result<u32, EgressRuleError> {
    let verdict_text = verdict.as_str();
    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.app_egress_rules \
             WHERE app_id = $1 AND verdict = $2",
            &[&app_id, &verdict_text],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, app_id = %app_id, "control: egress rule count failed");
            EgressRuleError::Db
        })?;
    let n: i64 = rows.first().map_or(0, |row| row.get("n"));
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

/// List an app's rules, its manifest hints, and the plan ceiling.
pub async fn list_rules(pg: &Client, app_id: Uuid) -> Result<EgressRuleList, EgressRuleError> {
    let manifest_rows = pg
        .query(
            "SELECT manifest_json FROM zeroship.apps WHERE id = $1",
            &[&app_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, app_id = %app_id, "control: app manifest lookup failed");
            EgressRuleError::Db
        })?;
    let Some(manifest_row) = manifest_rows.first() else {
        return Err(EgressRuleError::AppNotFound);
    };
    let manifest_json: Option<String> = manifest_row.get("manifest_json");

    // ORDER BY is a determinism device and nothing more: the verdict is a pure
    // function of the SET, so this ordering must never become meaningful.
    let rows = pg
        .query(
            "SELECT app_id, verdict, kind, destination, port, created_by, created_at, note \
             FROM zeroship.app_egress_rules \
             WHERE app_id = $1 \
             ORDER BY kind ASC, destination ASC, port ASC",
            &[&app_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, app_id = %app_id, "control: app egress rule list failed");
            EgressRuleError::Db
        })?;
    let rules = rows_to_records(&rows);

    let requests = manifest_net_requests(manifest_json.as_deref(), app_id);
    let accepted_keys = rules
        .iter()
        .filter(|r| r.verdict == Verdict::Accept)
        .map(|r| (r.destination.clone(), r.port))
        .collect::<std::collections::HashSet<_>>();
    let pending_requests = requests
        .iter()
        .filter(|r| !accepted_keys.contains(&(hint_destination_key(&r.host), r.port)))
        .cloned()
        .collect::<Vec<_>>();

    let caps = plan_net_limits(pg, app_id).await?;
    let used_accept_rules = count_of(&rules, Verdict::Accept);
    let used_reject_rules = count_of(&rules, Verdict::Reject);
    Ok(EgressRuleList {
        app_id,
        rules,
        requests,
        pending_requests,
        limits: EgressRuleLimits {
            max_accept_rules: caps.max_grants,
            used_accept_rules,
            max_reject_rules: MAX_REJECT_RULES,
            used_reject_rules,
            max_sockets: caps.max_sockets,
            egress_ceiling_bytes: caps.egress_ceiling_bytes,
        },
    })
}

/// Validate and write one rule, replacing an existing
/// `(kind, destination, port)` in place. The single writer of
/// `zeroship.app_egress_rules`.
///
/// Replacing rather than coexisting is the schema's doing and is deliberate:
/// `verdict` is not in the primary key, so flipping a destination from accept
/// to reject is an UPDATE and cannot leave the two opposing rows behind.
pub async fn upsert_rule(
    pg: &Client,
    app_id: Uuid,
    body: &EgressRuleBody,
    created_by: &str,
) -> Result<SetEgressRuleResult, EgressRuleError> {
    let caps = plan_net_limits(pg, app_id).await?;

    // ONE authoring boundary, shared with the runtime. Everything a rule may
    // not be - a wildcard, a bare IP literal, a single-label name, an accept
    // range broader than the floor - is refused here and nowhere else.
    let rule = EgressRule::parse(body.verdict, &body.destination, body.port)
        .map_err(EgressRuleError::Invalid)?;
    let kind = destination_kind(rule.destination());
    let destination = rule.destination().to_text();
    let port = i32::from(rule.port());
    let verdict_text = rule.verdict().as_str();

    let note = match body.note.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(note) if note.chars().count() > MAX_NOTE_CHARS => {
            return Err(EgressRuleError::Invalid(format!(
                "note must be at most {MAX_NOTE_CHARS} characters"
            )));
        }
        Some(note) => Some(note.to_string()),
        None => None,
    };

    // Whether this write is the app's FIRST range accept must be read BEFORE
    // the write, so re-POSTing the same rule does not re-announce it.
    let is_first_range_accept = rule.verdict() == Verdict::Accept
        && kind == "cidr"
        && count_range_accepts(pg, app_id).await? == 0;

    let cap = i64::from(match rule.verdict() {
        Verdict::Accept => caps.max_grants,
        Verdict::Reject => MAX_REJECT_RULES,
    });

    // The cap is a WHERE on the insert rather than a read-then-write, so a
    // second call cannot slip between the check and the row. It is not a hard
    // serialization barrier: two concurrent inserts under READ COMMITTED can
    // each see a pre-insert count, so the ceiling can overshoot by at most the
    // number of in-flight calls. Bounding a creator's own rule list does not
    // warrant a table lock on the registry's read path.
    //
    // The count is per-VERDICT and excludes the row being written, so
    // re-noting a rule the app already holds never trips its own cap, and an
    // accept rule is never charged against the reject bound or the reverse.
    let rows = pg
        .query(
            "INSERT INTO zeroship.app_egress_rules \
                (app_id, verdict, kind, destination, port, created_by, note) \
             SELECT $1, $2, $3, $4, $5, $6, $7 \
             WHERE (SELECT COUNT(*) FROM zeroship.app_egress_rules \
                    WHERE app_id = $1 AND verdict = $2 \
                      AND NOT (kind = $3 AND destination = $4 AND port = $5)) < $8 \
             ON CONFLICT (app_id, kind, destination, port) DO UPDATE SET \
                verdict = EXCLUDED.verdict, \
                created_by = EXCLUDED.created_by, \
                created_at = NOW(), \
                note = EXCLUDED.note \
             RETURNING app_id, verdict, kind, destination, port, created_by, created_at, note",
            &[
                &app_id,
                &verdict_text,
                &kind,
                &destination,
                &port,
                &created_by,
                &note,
                &cap,
            ],
        )
        .await
        .map_err(|err| {
            tracing::error!(
                error = %err, app_id = %app_id, destination = %destination, port,
                "control: app egress rule upsert failed"
            );
            EgressRuleError::Db
        })?;

    let Some(row) = rows.first() else {
        // No row means the WHERE refused it: the app is already at its cap.
        return Err(EgressRuleError::CapExceeded {
            verdict: rule.verdict(),
            max: u32::try_from(cap).unwrap_or(u32::MAX),
            current: count_rules(pg, app_id, rule.verdict()).await?,
        });
    };

    // The effective verdict needs the whole set, so it is read back rather than
    // derived from this row alone.
    let all = pg
        .query(
            "SELECT app_id, verdict, kind, destination, port, created_by, created_at, note \
             FROM zeroship.app_egress_rules WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, app_id = %app_id, "control: egress rule reread failed");
            EgressRuleError::Db
        })?;
    let written_key = (kind, destination.clone(), rule.port());
    let record = rows_to_records(&all)
        .into_iter()
        .find(|r| (r.kind, r.destination.clone(), r.port) == written_key)
        .unwrap_or_else(|| {
            written_record(&rule, app_id, row.get("created_by"), row.get("created_at"), note.clone())
        });

    Ok(SetEgressRuleResult {
        rule: record,
        notice: is_first_range_accept.then(|| first_range_accept_notice(rule.port())),
    })
}

/// How many ACCEPT rules with a range destination the app holds, at ANY port.
///
/// The input to the one-time notice, and NOT the runtime DNS gate predicate,
/// which this comment used to claim it was. The gate asks whether a range
/// ACCEPT exists AT THE PORT being connected to
/// (`EgressRules::holds_range_accept_at`); this asks whether the app has ever
/// written one. They cannot be one function: the notice is about the first time
/// an app enters the leaking class at all, and a per-port count would announce
/// it again on every new port. Keeping the two honestly separate is the point -
/// a shared name over two different questions is how a test of one comes to
/// read as coverage of the other.
async fn count_range_accepts(pg: &Client, app_id: Uuid) -> Result<u32, EgressRuleError> {
    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.app_egress_rules \
             WHERE app_id = $1 AND verdict = 'accept' AND kind = 'cidr'",
            &[&app_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, app_id = %app_id, "control: range accept count failed");
            EgressRuleError::Db
        })?;
    let n: i64 = rows.first().map_or(0, |row| row.get("n"));
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

/// Delete one rule. The destination is only a key into rows this app already
/// holds, so it is not shape-validated - but it IS canonicalised the same way,
/// so `API.Example.COM.` deletes the row `api.example.com` and `10.0.0.1/24`
/// deletes the row `10.0.0.0/24`.
///
/// The verdict is not part of the key because it is not part of the primary
/// key: one destination and port carries at most one rule, so naming it is
/// enough to name the rule.
pub async fn delete_rule(
    pg: &Client,
    app_id: Uuid,
    destination: &str,
    port: u16,
) -> Result<(), EgressRuleError> {
    if port == 0 {
        return Err(EgressRuleError::Invalid(
            "port must be between 1 and 65535".to_string(),
        ));
    }
    let parsed = Destination::parse(destination).map_err(EgressRuleError::Invalid)?;
    let kind = destination_kind(&parsed);
    let destination = parsed.to_text();
    let port = i32::from(port);
    let n = pg
        .execute(
            "DELETE FROM zeroship.app_egress_rules \
             WHERE app_id = $1 AND kind = $2 AND destination = $3 AND port = $4",
            &[&app_id, &kind, &destination, &port],
        )
        .await
        .map_err(|err| {
            tracing::error!(
                error = %err, app_id = %app_id, destination = %destination, port,
                "control: app egress rule delete failed"
            );
            EgressRuleError::Db
        })?;
    if n == 0 {
        return Err(EgressRuleError::RuleNotFound);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The stored `kind` discriminator, derived from the parsed destination so the
/// column can never disagree with the value it describes.
pub(crate) fn destination_kind(destination: &Destination) -> &'static str {
    match destination {
        Destination::Name(_) => "name",
        Destination::Range(_) => "cidr",
    }
}

fn count_of(rules: &[EgressRuleRecord], verdict: Verdict) -> u32 {
    u32::try_from(rules.iter().filter(|r| r.verdict == verdict).count()).unwrap_or(u32::MAX)
}

/// Turn a whole app's rows into records, stamping each with the verdict that
/// applies once the SET is read.
///
/// Deny-overrides on an unordered set means an accept can be dominated by a
/// reject, and the one case decidable without a lookup is range-inside-range at
/// the same port. That is the case this reflects. Name rules and
/// name-versus-range overlaps are settled at connect time against a resolved
/// address, so reporting a guess for them here would be reporting a lookup this
/// endpoint did not do.
fn rows_to_records(rows: &[compio_postgres::Row]) -> Vec<EgressRuleRecord> {
    let parsed: Vec<(EgressRuleRecord, Option<Destination>)> = rows
        .iter()
        .map(|row| {
            let raw: String = row.get("verdict");
            let verdict = parse_verdict(&raw);
            let record = row_to_record(row, verdict);
            let destination = Destination::parse(&record.destination).ok();
            (record, destination)
        })
        .collect();

    let reject_ranges: Vec<(&Destination, u16)> = parsed
        .iter()
        .filter(|(r, _)| r.verdict == Verdict::Reject)
        .filter_map(|(r, d)| d.as_ref().map(|d| (d, r.port)))
        .filter(|(d, _)| matches!(d, Destination::Range(_)))
        .collect();

    parsed
        .iter()
        .map(|(record, destination)| {
            let mut record = record.clone();
            if record.verdict == Verdict::Accept {
                if let Some(Destination::Range(net)) = destination.as_ref() {
                    let dominated = reject_ranges.iter().any(|(reject, port)| {
                        *port == record.port
                            && matches!(
                                reject,
                                Destination::Range(reject_net) if reject_net.contains(net)
                            )
                    });
                    if dominated {
                        record.effective_verdict = Verdict::Reject;
                    }
                }
            }
            record
        })
        .collect()
}

/// A verdict read back from the database, for EVERY reader of the table.
///
/// The column carries a CHECK constraint, so anything else is a hand-edited
/// row; treating it as REJECT is the fail-closed reading, and the control plane
/// is the only writer that is supposed to produce one.
///
/// One function because the two readers - this endpoint and the registry
/// projection the worker consumes - must not be able to disagree about a row.
/// If they did, a creator would be shown one verdict and the runtime would
/// enforce another, and only one of those two answers is visible to them.
pub(crate) fn parse_verdict(raw: &str) -> Verdict {
    match raw {
        "accept" => Verdict::Accept,
        other => {
            if other != "reject" {
                tracing::error!(
                    verdict = %other,
                    "control: egress rule row carries an unknown verdict; reading it as reject"
                );
            }
            Verdict::Reject
        }
    }
}

/// The record for a rule this request just wrote, built from the rule itself.
///
/// Only reached if the re-read cannot see the row the write returned, which the
/// primary key makes unreachable through the API - so it is exactly the kind of
/// arm that rots unwatched. It used to hardcode `Verdict::Accept`, the
/// fail-OPEN direction: a creator who wrote a REJECT would be told they had
/// written an accept. It shapes what the creator is SHOWN and not what the
/// runtime enforces - the worker reads the registry projection, not this
/// response - so it was a display defect, and still the wrong default to leave
/// in a file whose subject is verdicts.
fn written_record(
    rule: &EgressRule,
    app_id: Uuid,
    created_by: String,
    created_at: DateTime<Utc>,
    note: Option<String>,
) -> EgressRuleRecord {
    EgressRuleRecord {
        app_id,
        verdict: rule.verdict(),
        kind: destination_kind(rule.destination()),
        destination: rule.destination().to_text(),
        port: rule.port(),
        created_by,
        created_at,
        note,
        // No other row was read, so no domination can be known. Reporting this
        // rule's own verdict is the only answer this path has evidence for.
        effective_verdict: rule.verdict(),
    }
}

fn row_to_record(row: &compio_postgres::Row, verdict: Verdict) -> EgressRuleRecord {
    let port_i32: i32 = row.get("port");
    let kind: String = row.get("kind");
    EgressRuleRecord {
        app_id: row.get("app_id"),
        verdict,
        kind: if kind == "cidr" { "cidr" } else { "name" },
        destination: row.get("destination"),
        port: u16::try_from(port_i32).unwrap_or(0),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        note: row.get("note"),
        effective_verdict: verdict,
    }
}

fn manifest_net_requests(manifest_json: Option<&str>, app_id: Uuid) -> Vec<EgressRequest> {
    let Some(raw) = manifest_json else {
        return Vec::new();
    };
    let manifest = match serde_json::from_str::<zeroship_bundle::Manifest>(raw) {
        Ok(manifest) => manifest,
        Err(err) => {
            tracing::warn!(
                app_id = %app_id,
                error = %err,
                "control: app egress rule list could not parse manifest requests"
            );
            return Vec::new();
        }
    };
    manifest
        .net
        .requests
        .into_iter()
        .map(|r| EgressRequest {
            host: normalize_name(&r.host),
            port: r.port,
            reason: r.reason,
        })
        .collect()
}

/// The key a manifest hint is matched on. A hint is creator-authored text that
/// has never been through the rule grammar, so it is canonicalised the same way
/// a rule would be when it can be, and compared as-is when it cannot - a hint
/// that is not a legal destination simply never matches a rule, which is the
/// right answer.
fn hint_destination_key(host: &str) -> String {
    Destination::parse(host).map_or_else(|_| normalize_name(host), |d| d.to_text())
}

fn bad_uuid() -> web::HttpResponse {
    web::HttpResponse::BadRequest().json(&json!({"error": "bad app_id"}))
}

// ---------------------------------------------------------------------------
// Creator HTTP surface
// ---------------------------------------------------------------------------

pub async fn list(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let Ok(app_id) = Uuid::parse_str(&path) else {
        return bad_uuid();
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::EnvRead,
            Resource::App {
                id: app_id.to_string(),
            },
            &state,
        )
        .await
    {
        return resp;
    }
    match list_rules(state.control_pg.as_ref(), app_id).await {
        Ok(list) => web::HttpResponse::Ok().json(&list),
        Err(e) => e.into_response(),
    }
}

pub async fn create(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<EgressRuleBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let Ok(app_id) = Uuid::parse_str(&path) else {
        return bad_uuid();
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::EnvWrite,
            Resource::App {
                id: app_id.to_string(),
            },
            &state,
        )
        .await
    {
        return resp;
    }
    match upsert_rule(
        state.control_pg.as_ref(),
        app_id,
        &body,
        &authz.principal_id.to_string(),
    )
    .await
    {
        Ok(result) => {
            let resource = format!(
                "{}:{}:{}",
                result.rule.verdict.as_str(),
                result.rule.destination,
                result.rule.port
            );
            log_rule_audit(
                &req,
                &state,
                &authz,
                app_id,
                AuditAction::SetAppEgressRule,
                &resource,
            )
            .await;
            web::HttpResponse::Ok().json(&result)
        }
        Err(e) => e.into_response(),
    }
}

pub async fn delete(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<DeleteEgressRuleBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await {
        return r;
    }
    let Ok(app_id) = Uuid::parse_str(&path) else {
        return bad_uuid();
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::EnvWrite,
            Resource::App {
                id: app_id.to_string(),
            },
            &state,
        )
        .await
    {
        return resp;
    }
    match delete_rule(
        state.control_pg.as_ref(),
        app_id,
        &body.destination,
        body.port,
    )
    .await
    {
        Ok(()) => {
            let resource = format!("{}:{}", body.destination, body.port);
            log_rule_audit(
                &req,
                &state,
                &authz,
                app_id,
                AuditAction::DeleteAppEgressRule,
                &resource,
            )
            .await;
            web::HttpResponse::NoContent().finish()
        }
        Err(e) => e.into_response(),
    }
}

async fn log_rule_audit(
    req: &web::HttpRequest,
    state: &AppState,
    authz: &AuthzGuard,
    app_id: Uuid,
    action: AuditAction,
    resource: &str,
) {
    let ip = http_util::source_ip(req, state.trust_proxy);
    audit::log(
        &state.registry,
        AuditEntry {
            app_id: Some(app_id),
            creator_id: None,
            actor_user_id: Some(authz.principal_id),
            action,
            resource: Some(resource),
            source_ip: ip.as_deref(),
        },
    )
    .await;
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/api/apps/{id}/egress-rules")
            .state(web::types::PayloadConfig::new(EGRESS_RULE_PAYLOAD_BYTES))
            .route(web::get().to(list))
            .route(web::post().to(create))
            .route(web::delete().to(delete)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_core::types::NetEgressEntry;

    /// The measurement [`MAX_REJECT_RULES`] is derived from, kept as a test so
    /// the constant's stated basis fails loudly if the wire shape grows.
    ///
    /// This measures the PROJECTION cost of a reject rule - the thing the cap
    /// exists to bound - not anything about safety. A reject can only narrow.
    #[test]
    fn reject_rule_projection_cost_bounds_the_cap() {
        // The longest destination the grammar admits on the range side: a full
        // IPv6 address plus a prefix. A name may be longer, but a name rule is
        // not what an unbounded reject list would be written with.
        let entry = NetEgressEntry {
            verdict: Verdict::Reject,
            destination: "2001:0db8:85a3:0000:0000:8a2e:0370:7334/128".to_string(),
            port: 65535,
        };
        let per_rule = serde_json::to_vec(&entry).expect("entry serializes").len();
        // Pinned so the constant's stated basis is a measurement someone can
        // re-run, not a number in a doc comment nothing checks.
        assert_eq!(
            per_rule, 93,
            "the per-rule projection cost moved; re-derive MAX_REJECT_RULES"
        );
        let worst_case = per_rule * MAX_REJECT_RULES as usize;
        assert!(
            worst_case <= REJECT_RULE_PROJECTION_BUDGET_BYTES,
            "{MAX_REJECT_RULES} reject rules at {per_rule} bytes each is {worst_case} bytes, \
             over the {REJECT_RULE_PROJECTION_BUDGET_BYTES}-byte per-app projection budget"
        );
        // The other direction: the cap is not so small that the budget is
        // wasted. Without this the assertion above passes for a cap of 1.
        assert!(
            per_rule * (MAX_REJECT_RULES as usize * 2) > REJECT_RULE_PROJECTION_BUDGET_BYTES,
            "the cap is far below the budget it is derived from; re-derive it"
        );
    }

    /// The validator must discriminate, not merely refuse. Each pair below
    /// differs in ONE thing, so a green result says the rule under test is what
    /// decided it.
    #[test]
    fn wildcards_are_refused_and_the_exact_name_beside_them_is_accepted() {
        let refused = EgressRule::parse(Verdict::Accept, "*.example.com", 443)
            .expect_err("a wildcard destination must be refused");
        assert!(
            refused.contains("wildcard"),
            "the refusal must say wildcards are the problem, got {refused:?}"
        );
        // The control: same port, same verdict, same registrable domain, no `*`.
        let accepted = EgressRule::parse(Verdict::Accept, "api.example.com", 443)
            .expect("an exact name must be accepted");
        assert_eq!(accepted.destination().to_text(), "api.example.com");
    }

    #[test]
    fn an_accept_range_below_the_floor_is_refused_and_one_at_it_is_accepted() {
        let refused = EgressRule::parse(Verdict::Accept, "10.0.0.0/8", 443)
            .expect_err("an accept range broader than the /16 floor must be refused");
        assert!(
            refused.contains("/16"),
            "the refusal must name the floor, got {refused:?}"
        );
        // The control: one bit narrower, everything else identical.
        let accepted = EgressRule::parse(Verdict::Accept, "10.0.0.0/16", 443)
            .expect("a range at the floor must be accepted");
        assert_eq!(accepted.destination().to_text(), "10.0.0.0/16");
        assert_eq!(destination_kind(accepted.destination()), "cidr");
        // And the same range as a REJECT has no floor at all, which is the
        // asymmetry the floor is only meaningful because of.
        EgressRule::parse(Verdict::Reject, "0.0.0.0/0", 443)
            .expect("a reject range has no floor");
    }

    /// A row whose verdict is not one the CHECK constraint admits is a
    /// hand-edited row, and must read as REJECT: a rule the control plane did
    /// not write must never be able to WIDEN what an app reaches.
    ///
    /// Unreachable through the API, and unpinned until now - flipping the
    /// fallback to `Accept` in BOTH readers left this crate green. It is
    /// tested at the function rather than the endpoint because the endpoint
    /// cannot produce the input.
    #[test]
    fn an_unknown_verdict_reads_as_reject_in_both_readers() {
        assert_eq!(parse_verdict("reject"), Verdict::Reject);
        assert_eq!(parse_verdict("REJECT"), Verdict::Reject);
        assert_eq!(parse_verdict(""), Verdict::Reject);
        assert_eq!(parse_verdict("allow"), Verdict::Reject);
        // The control, differing in ONE thing - the exact stored token. Without
        // it a reader that answered REJECT for everything would pass.
        assert_eq!(parse_verdict("accept"), Verdict::Accept);
    }

    /// The record reported for a rule the re-read could not find must carry the
    /// verdict that was WRITTEN. The old fallback hardcoded `Accept`, so a
    /// creator writing a reject would have been shown an accept.
    #[test]
    fn the_written_record_reports_the_verdict_that_was_written() {
        let app_id = Uuid::new_v4();
        let now = Utc::now();
        let reject = EgressRule::parse(Verdict::Reject, "93.184.216.7/32", 443).unwrap();
        let record = written_record(&reject, app_id, "usr_x".to_string(), now, None);
        assert_eq!(record.verdict, Verdict::Reject);
        assert_eq!(record.effective_verdict, Verdict::Reject);
        assert_eq!(record.kind, "cidr");
        assert_eq!(record.destination, "93.184.216.7/32");

        // The control, differing in ONE thing - the verdict written.
        let accept = EgressRule::parse(Verdict::Accept, "93.184.216.7/32", 443).unwrap();
        let record = written_record(&accept, app_id, "usr_x".to_string(), now, None);
        assert_eq!(record.verdict, Verdict::Accept);
        assert_eq!(record.effective_verdict, Verdict::Accept);
    }

    #[test]
    fn the_first_range_accept_notice_says_what_changes() {
        let notice = first_range_accept_notice(443);
        assert!(
            notice.contains("resolved first and refused afterwards"),
            "the notice must state the new order of operations: {notice}"
        );
        assert!(
            notice.contains("nameserver"),
            "the notice must say where the lookup goes: {notice}"
        );
        assert!(notice.contains("443"), "the notice must name the port");
    }
}
