use std::collections::HashMap;
use std::net::IpAddr;
use std::str::FromStr;

use cedar_policy::{Context, Decision, PolicySet, Request, Response, RestrictedExpression, Schema};
use compio_postgres::Client;
use zeroship_core::UserId;

use crate::authority::{self, Authority};
use crate::entities::{assemble_entities, cedar_string, resource_entity_uid, uid};
use crate::{lower, Action, AuthzError, PlatformPolicies, Policy, Resource};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthzDecision {
    Allow,
    Deny,
}

/// Everything an authorization request carries FROM THE CALLER.
///
/// It deliberately has no rank field. Authority is resolved server-side by
/// [`crate::authority::resolve`] inside [`enforce`], from the requested
/// resource, on every call. A caller-supplied rank would be a claim, and a
/// claim is not an authority.
///
/// It also carries no MFA fields, and their deletion is worth a sentence. Every
/// producer of a live `zeroship_authn::VerifiedPrincipal` set `mfa_verified:
/// false` and `mfa_age_seconds: None` unconditionally - the platform has no
/// second-factor signal to report - so `Condition::RequireMfa` and
/// `Condition::MfaWithin` compared against a value that was not merely unknown
/// but WRONG. A condition that can only ever fail is worse than an absent one:
/// it reads to a reviewer as a fence. Both variants and both fields went in the
/// same sweep. `now` stayed, because it is an honest unused input rather than a
/// falsified one.
#[derive(Debug)]
pub struct AuthzContext<'a> {
    pub principal_id: UserId,
    pub token_policy: Option<Policy>,
    pub action: Action,
    pub resource: Resource,
    pub now: i64,
    pub request_ip: Option<IpAddr>,
    pub request_id: Option<&'a str>,
}

/// Enforce the static policy bands and the caller's optional wrapper policy.
///
/// When `token_policy` is present this intentionally calls Cedar twice:
/// principal-without-token first, then the wrapper. This preserves TOKEN
/// SUBSET USER after a membership or role changes.
///
/// The wrapper always comes from the bearer's own scopes, which lower to
/// `Resource::Any` ([`crate::scopes_to_policy`]) - a bare `resource` matching
/// every resource type. The wrapper therefore narrows the ACTION and nothing
/// else, and the rank comparison in the static bands is the whole fence.
///
/// # Errors
///
/// Returns [`AuthzError::Db`] when the authority resolve fails,
/// [`AuthzError::Validation`] when the principal has no user row,
/// [`AuthzError::CedarEval`] when Cedar records a per-policy evaluation error,
/// and the other Cedar error variants when a policy, request or entity is
/// rejected. A resolve failure is never degraded into a Deny: an unreadable
/// membership must not be audited as "no seat".
pub async fn enforce(
    pg: &Client,
    platform: &PlatformPolicies,
    ctx: &AuthzContext<'_>,
) -> Result<AuthzDecision, AuthzError> {
    let authority = authority::resolve(pg, &ctx.principal_id, &ctx.resource).await?;
    let entities = assemble_entities(&ctx.principal_id, &authority, &ctx.resource)?;

    if ctx.token_policy.is_some() {
        let principal_request = build_request(ctx, &authority, platform.schema())?;
        let principal_decision = cedar_policy::Authorizer::new().is_authorized(
            &principal_request,
            platform.policies(),
            &entities,
        );
        refuse_on_evaluation_error(&principal_decision, "principal pass")?;
        if principal_decision.decision() != Decision::Allow {
            let matched_policies = matched_policy_ids(&principal_decision);
            audit_decision(pg, ctx, AuthzDecision::Deny, &matched_policies).await;
            return Ok(AuthzDecision::Deny);
        }
    }

    let final_policies = if let Some(token_policy) = &ctx.token_policy {
        policy_set_from_policy(token_policy)?
    } else {
        platform.policies().clone()
    };

    let req = build_request(ctx, &authority, platform.schema())?;
    let decision = cedar_policy::Authorizer::new().is_authorized(&req, &final_policies, &entities);
    refuse_on_evaluation_error(&decision, "wrapper pass")?;
    let matched_policies = matched_policy_ids(&decision);

    let decision = if decision.decision() == Decision::Allow {
        AuthzDecision::Allow
    } else {
        AuthzDecision::Deny
    };
    audit_decision(pg, ctx, decision, &matched_policies).await;
    Ok(decision)
}

/// Refuse a response that carries per-policy evaluation errors.
///
/// Cedar does not fail an authorization when one policy blows up. It SKIPS that
/// policy, records the error in `diagnostics().errors()`, and returns a
/// perfectly ordinary response - so a permit that should have fired becomes a
/// Deny whose `matched_policy_ids` is empty. In `zeroship.authz_decisions` that
/// row is byte-identical to the denial of a principal who holds no seat at all,
/// which makes the one durable record of the decision actively misleading about
/// why it went that way.
///
/// A silent deny is also the WRONG answer for the failure mode: an evaluation
/// error means the platform could not decide, and a caller told "forbidden"
/// will not retry while a caller told "error" will.
fn refuse_on_evaluation_error(response: &Response, pass: &str) -> Result<(), AuthzError> {
    let errors: Vec<String> = response
        .diagnostics()
        .errors()
        .map(ToString::to_string)
        .collect();
    if errors.is_empty() {
        return Ok(());
    }
    tracing::error!(
        pass,
        errors = errors.join("; "),
        "authz: Cedar recorded a policy evaluation error; refusing rather than \
         returning the deny it would otherwise produce"
    );
    Err(AuthzError::CedarEval(format!(
        "{pass}: {}",
        errors.join("; ")
    )))
}

/// Return true when the principal can perform `ctx.action` on SOME resource
/// they currently hold authority over.
///
/// This is the consent/grant question - the user is delegating an action
/// vocabulary, not authorizing one concrete request - so the answer must be the
/// true one or the screen over-claims.
///
/// The probe order is `Resource::Any`, then each organization the principal is
/// a member of, then the representative project set
/// ([`authority::project_probe_resources`]). Apps are NOT probed: every
/// app-scoped band also has a `Project` statement, so a project probe answers
/// for the apps inside it, and enumerating apps would add cost without adding
/// an answer.
///
/// Because every band discriminates the resource TYPE in its scope, an
/// App-typed or Project-typed probe can no longer satisfy an organization
/// action. That is what stopped `organization:members:write` from reading as
/// grantable to any creator with one app.
///
/// **THIS FUNCTION IS WHY THE SCHEMA'S `appliesTo` IS WIDE.** It legitimately
/// asks every action against every resource type, and expects "no" for most of
/// them. A schema whose `appliesTo` was derived from the pairs the bands write
/// would turn those honest questions into `AuthzError::CedarRequest` at
/// `build_request` - raised before `audit_decision`, so a 500 with no record
/// rather than a denial.
///
/// # Errors
///
/// Propagates whatever [`enforce`] returns for any probed resource.
pub async fn is_authorized_anywhere(
    pg: &Client,
    platform: &PlatformPolicies,
    ctx: &AuthzContext<'_>,
) -> Result<bool, AuthzError> {
    let mut resources = vec![Resource::Any];
    resources.extend(authority::organization_resources(pg, &ctx.principal_id).await?);
    resources.extend(authority::project_probe_resources(pg, &ctx.principal_id).await?);

    for resource in resources {
        let probe = AuthzContext {
            principal_id: ctx.principal_id.clone(),
            token_policy: None,
            action: ctx.action,
            resource,
            now: ctx.now,
            request_ip: ctx.request_ip,
            request_id: ctx.request_id,
        };
        if enforce(pg, platform, &probe).await? == AuthzDecision::Allow {
            return Ok(true);
        }
    }

    Ok(false)
}

fn policy_set_from_policy(policy: &Policy) -> Result<PolicySet, AuthzError> {
    PolicySet::from_str(&lower(policy)).map_err(|err| AuthzError::CedarParse(err.to_string()))
}

/// Build the Cedar request, BOUND TO THE SCHEMA.
///
/// The `Some(schema)` is not decoration. It makes Cedar check the action id,
/// the principal type, the resource type and the whole context SHAPE against
/// `deploy/policies/zeroship.cedarschema` before any policy runs, so a request
/// outside the declared vocabulary is an `AuthzError::CedarRequest` rather than
/// a deny that looks exactly like an honest non-match.
fn build_request(
    ctx: &AuthzContext<'_>,
    authority: &Authority,
    schema: &Schema,
) -> Result<Request, AuthzError> {
    let principal = uid("User", ctx.principal_id.as_str())?;
    let action = uid("Action", ctx.action.cedar_id())?;
    let resource = resource_entity_uid(&ctx.resource)?;
    let context = build_context(ctx, authority)?;
    Request::new(principal, action, resource, context, Some(schema))
        .map_err(|err| AuthzError::CedarRequest(err.to_string()))
}

/// Build the request context.
///
/// EVERY key declared here is supplied on EVERY request, unconditionally. That
/// is what makes the rank comparisons fail closed: a principal with no seat
/// arrives with `effective_rank: 0` and each band denies at its own `>=`, with
/// the band recorded as a non-match rather than as an evaluation error nothing
/// writes down.
///
/// This map and the `RequestContext` type in
/// `deploy/policies/zeroship.cedarschema` must agree EXACTLY, in both
/// directions - a schema-bound request refuses a missing key and an extra one
/// alike. `every_action_builds_a_schema_bound_request_at_every_resource_type`
/// exercises this contract through the real request builder.
fn build_context(ctx: &AuthzContext<'_>, authority: &Authority) -> Result<Context, AuthzError> {
    let request_ip = ctx
        .request_ip
        .map_or_else(|| "0.0.0.0".to_owned(), |ip| ip.to_string());
    let now_minute_utc = utc_minute_of_day(ctx.now);

    let pairs = HashMap::from([
        ("now".to_owned(), restricted(&ctx.now.to_string())?),
        (
            "now_minute_utc".to_owned(),
            restricted(&now_minute_utc.to_string())?,
        ),
        (
            "request_ip".to_owned(),
            restricted(&format!("ip({})", cedar_string(&request_ip)))?,
        ),
        // The authority, resolved server-side from the requested resource.
        // `effective_rank` is already narrowed for a project- or app-scoped
        // request; `billing_rank` is organization-level always.
        (
            "effective_rank".to_owned(),
            restricted(&authority.effective_rank.to_string())?,
        ),
        (
            "billing_rank".to_owned(),
            restricted(&authority.billing_rank.to_string())?,
        ),
    ]);
    Context::from_pairs(pairs).map_err(|err| AuthzError::CedarRequest(err.to_string()))
}

fn restricted(source: &str) -> Result<RestrictedExpression, AuthzError> {
    RestrictedExpression::from_str(source).map_err(|err| AuthzError::CedarRequest(err.to_string()))
}

const fn utc_minute_of_day(now: i64) -> i64 {
    now.rem_euclid(86_400) / 60
}

fn matched_policy_ids(response: &Response) -> Vec<String> {
    response
        .diagnostics()
        .reason()
        .map(ToString::to_string)
        .collect()
}

async fn audit_decision(
    pg: &Client,
    ctx: &AuthzContext<'_>,
    decision: AuthzDecision,
    matched_policies: &[String],
) {
    let (resource_type, resource_id) = audit_resource(&ctx.resource);
    let decision = match decision {
        AuthzDecision::Allow => "allow",
        AuthzDecision::Deny => "deny",
    };
    let request_ip = ctx.request_ip;
    if let Err(err) = pg
        .execute(
            "INSERT INTO zeroship.authz_decisions \
                (actor_user_id, action, resource_type, resource_id, decision, matched_policies, request_ip, request_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                &ctx.principal_id.as_str(),
                &ctx.action.cedar_id(),
                &resource_type,
                &resource_id,
                &decision,
                &matched_policies,
                &request_ip,
                &ctx.request_id,
            ],
        )
        .await
    {
        tracing::error!(error = %err, "authz decision audit insert failed");
    }
}

/// The audit tag for a resource.
///
/// `zeroship.authz_decisions.resource_type` is unconstrained `text`, so nothing
/// at the database layer notices a mis-tagged row. The match is exhaustive so
/// that a new [`Resource`] variant is a compile error here rather than a row
/// silently filed under the wrong type.
fn audit_resource(resource: &Resource) -> (&'static str, Option<String>) {
    match resource {
        Resource::App { id } => ("app", Some(id.clone())),
        Resource::Project { id } => ("project", Some(id.clone())),
        Resource::Organization { id } => ("organization", Some(id.clone())),
        Resource::Any => ("any", None),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use cedar_policy::Decision;

    use super::{
        audit_resource, build_request, matched_policy_ids, refuse_on_evaluation_error, AuthzContext,
    };
    use crate::entities::{resource_entity_uid, uid};
    use crate::{load_platform_policies, Action, Authority, AuthzError, Resource};
    use zeroship_core::UserId;

    fn principal() -> UserId {
        UserId::parse("usr_0000000000000000000001").expect("valid user id fixture")
    }

    /// Every id-bearing variant records its own id, and each type tag is
    /// distinct. A shared tag would make two different resources
    /// indistinguishable in the one durable record of a decision.
    #[test]
    fn audit_tags_are_distinct_and_carry_the_id() {
        let cases = [
            (Resource::App { id: "a".to_owned() }, "app"),
            (Resource::Project { id: "p".to_owned() }, "project"),
            (
                Resource::Organization { id: "o".to_owned() },
                "organization",
            ),
        ];
        let mut tags = std::collections::HashSet::new();
        for (resource, expected_tag) in cases {
            let (tag, id) = audit_resource(&resource);
            assert_eq!(tag, expected_tag);
            assert!(id.is_some(), "{resource:?} must record its id");
            assert!(tags.insert(tag), "duplicate audit tag {tag}");
        }
        let (tag, id) = audit_resource(&Resource::Any);
        assert_eq!(tag, "any");
        assert_eq!(id, None);
        assert!(tags.insert(tag));
    }

    /// EVERY action must build a schema-bound request at EVERY resource type,
    /// and the whole context must be accepted with it.
    ///
    /// This is the binding for the width of `appliesTo` in
    /// `deploy/policies/zeroship.cedarschema`, and for the exact agreement
    /// between [`build_context`](super::build_context) and that file's
    /// `RequestContext`. Nothing else checks either: a narrowed `appliesTo`, a
    /// context key added on one side only, or an action dropped from the schema
    /// all leave the policy set validating cleanly and turn live requests into
    /// `AuthzError::CedarRequest` - raised BEFORE `audit_decision`, so a 500
    /// with no row in `zeroship.authz_decisions` to say it happened.
    ///
    /// The cross product is the surface `is_authorized_anywhere` really asks
    /// for. It probes `Resource::Any`, then each organization, then a
    /// representative project, for whatever action the consent screen names -
    /// so "organization:members:write at a Project" is an honest question that
    /// must return `Deny`, never an error.
    #[test]
    fn every_action_builds_a_schema_bound_request_at_every_resource_type() {
        let platform = load_platform_policies().expect("policies and schema load");
        let authority = Authority {
            email_verified: true,
            account_locked: false,
            effective_rank: 40,
            billing_rank: 20,
        };
        let resources = [
            Resource::Any,
            Resource::App {
                id: uuid::Uuid::nil().to_string(),
            },
            Resource::Project {
                id: "prj_0123456789abcdefghijkl".to_owned(),
            },
            Resource::Organization {
                id: "org_0123456789abcdefghijkl".to_owned(),
            },
        ];

        let mut ruled_on = 0usize;
        for action in Action::all() {
            for resource in &resources {
                ruled_on += 1;
                let ctx = AuthzContext {
                    principal_id: principal(),
                    token_policy: None,
                    action: *action,
                    resource: resource.clone(),
                    now: 1_760_000_000,
                    request_ip: None,
                    request_id: None,
                };
                build_request(&ctx, &authority, platform.schema()).unwrap_or_else(|err| {
                    panic!(
                        "{} at {} is refused by the schema: {err}",
                        action.cedar_id(),
                        resource.cedar_type()
                    )
                });
            }
        }

        assert_eq!(
            ruled_on,
            Action::all().len() * resources.len(),
            "the enumeration collapsed, so the clean result above means nothing"
        );
        assert!(ruled_on >= 40, "ruled on only {ruled_on} pair(s)");
    }

    /// A Cedar evaluation error must be refused, and the reason it must is in
    /// the second half of this test: the response Cedar hands back for one is
    /// an ordinary `Deny` with an EMPTY reason set.
    ///
    /// Cedar does not fail an authorization when a policy blows up. It skips
    /// that policy and records the error out of band. So a permit that should
    /// have fired becomes a denial whose `matched_policies` column is `{}` -
    /// byte-identical, in `zeroship.authz_decisions`, to the denial of a
    /// principal who holds no seat at all. Returning that Deny would file a
    /// platform malfunction as a routine refusal, and tell a caller who should
    /// retry that they are forbidden.
    ///
    /// The defect is provoked the way it actually happens: a policy
    /// dereferencing an entity attribute the store does not carry. That is
    /// exactly why authority rides in the request CONTEXT rather than on the
    /// `User` entity - see `crate::entities`.
    #[test]
    fn a_policy_evaluation_error_is_refused_and_would_otherwise_be_a_bare_deny() {
        let policies = cedar_policy::PolicySet::from_str(
            "permit (principal, action, resource) when { principal.rank >= 1 };",
        )
        .expect("the policy parses - the attribute is missing at RUNTIME, not at parse time");
        let authority = Authority {
            email_verified: true,
            account_locked: false,
            effective_rank: 40,
            billing_rank: 20,
        };
        let principal = principal();
        let entities = crate::assemble_entities(&principal, &authority, &Resource::Any)
            .expect("entities assemble");
        let request = cedar_policy::Request::new(
            uid("User", principal.as_str()).expect("principal uid"),
            uid("Action", Action::AppsRead.cedar_id()).expect("action uid"),
            resource_entity_uid(&Resource::Any).expect("resource uid"),
            cedar_policy::Context::empty(),
            None,
        )
        .expect("request builds");

        let response =
            cedar_policy::Authorizer::new().is_authorized(&request, &policies, &entities);

        // What the caller would have been told without the refusal.
        assert_eq!(response.decision(), Decision::Deny);
        assert!(
            matched_policy_ids(&response).is_empty(),
            "the audit row for an evaluation error carries no policy id, which is \
             what makes it indistinguishable from an honest non-match"
        );

        let refused = refuse_on_evaluation_error(&response, "test pass");
        match refused {
            Err(AuthzError::CedarEval(message)) => {
                assert!(
                    message.contains("test pass"),
                    "the refusal must name the pass it came from: {message}"
                );
            }
            other => panic!("an evaluation error must be refused, got {other:?}"),
        }

        // The control: the same helper on a response with no evaluation error
        // must pass it through, or the arm above proves nothing about errors.
        let clean = load_platform_policies().expect("policies load");
        let clean_response =
            cedar_policy::Authorizer::new().is_authorized(&request, clean.policies(), &entities);
        assert!(refuse_on_evaluation_error(&clean_response, "test pass").is_ok());
    }
}
