use std::collections::HashMap;
use std::net::IpAddr;
use std::str::FromStr;

use cedar_policy::{Context, Decision, PolicySet, Request, Response, RestrictedExpression};
use compio_postgres::Client;
use uuid::Uuid;

use crate::authority::{self, Authority};
use crate::entities::{assemble_entities, cedar_string, resource_entity_uid, uid};
use crate::{lower, Action, AuthzError, Policy, Resource};

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
#[derive(Debug)]
pub struct AuthzContext<'a> {
    pub principal_id: Uuid,
    pub token_policy: Option<Policy>,
    pub action: Action,
    pub resource: Resource,
    pub now: i64,
    pub request_ip: Option<IpAddr>,
    pub mfa_verified: bool,
    pub mfa_age_seconds: Option<u32>,
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
/// [`AuthzError::Validation`] when the principal has no user row, and the Cedar
/// error variants when a policy, request or entity is rejected. A resolve
/// failure is never degraded into a Deny: an unreadable membership must not be
/// audited as "no seat".
pub async fn enforce(
    pg: &Client,
    static_policies: &PolicySet,
    ctx: &AuthzContext<'_>,
) -> Result<AuthzDecision, AuthzError> {
    let authority = authority::resolve(pg, ctx.principal_id, &ctx.resource).await?;
    let entities = assemble_entities(ctx.principal_id, &authority, &ctx.resource)?;

    if ctx.token_policy.is_some() {
        let principal_request = build_request(ctx, &authority)?;
        let principal_decision = cedar_policy::Authorizer::new().is_authorized(
            &principal_request,
            static_policies,
            &entities,
        );
        if principal_decision.decision() != Decision::Allow {
            let matched_policies = matched_policy_ids(&principal_decision);
            audit_decision(pg, ctx, AuthzDecision::Deny, &matched_policies).await;
            return Ok(AuthzDecision::Deny);
        }
    }

    let final_policies = if let Some(token_policy) = &ctx.token_policy {
        policy_set_from_policy(token_policy)?
    } else {
        static_policies.clone()
    };

    let req = build_request(ctx, &authority)?;
    let decision = cedar_policy::Authorizer::new().is_authorized(&req, &final_policies, &entities);
    let matched_policies = matched_policy_ids(&decision);

    let decision = if decision.decision() == Decision::Allow {
        AuthzDecision::Allow
    } else {
        AuthzDecision::Deny
    };
    audit_decision(pg, ctx, decision, &matched_policies).await;
    Ok(decision)
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
/// # Errors
///
/// Propagates whatever [`enforce`] returns for any probed resource.
pub async fn is_authorized_anywhere(
    pg: &Client,
    static_policies: &PolicySet,
    ctx: &AuthzContext<'_>,
) -> Result<bool, AuthzError> {
    let mut resources = vec![Resource::Any];
    resources.extend(authority::organization_resources(pg, ctx.principal_id).await?);
    resources.extend(authority::project_probe_resources(pg, ctx.principal_id).await?);

    for resource in resources {
        let probe = AuthzContext {
            principal_id: ctx.principal_id,
            token_policy: None,
            action: ctx.action,
            resource,
            now: ctx.now,
            request_ip: ctx.request_ip,
            mfa_verified: ctx.mfa_verified,
            mfa_age_seconds: ctx.mfa_age_seconds,
            request_id: ctx.request_id,
        };
        if enforce(pg, static_policies, &probe).await? == AuthzDecision::Allow {
            return Ok(true);
        }
    }

    Ok(false)
}

fn policy_set_from_policy(policy: &Policy) -> Result<PolicySet, AuthzError> {
    PolicySet::from_str(&lower(policy)).map_err(|err| AuthzError::CedarParse(err.to_string()))
}

fn build_request(ctx: &AuthzContext<'_>, authority: &Authority) -> Result<Request, AuthzError> {
    let principal = uid("User", &ctx.principal_id.to_string())?;
    let action = uid("Action", ctx.action.cedar_id())?;
    let resource = resource_entity_uid(&ctx.resource)?;
    let context = build_context(ctx, authority)?;
    Request::new(principal, action, resource, context, None)
        .map_err(|err| AuthzError::CedarRequest(err.to_string()))
}

/// Build the request context.
///
/// EVERY key declared here is supplied on EVERY request, unconditionally. That
/// is what makes the rank comparisons fail closed: a principal with no seat
/// arrives with `effective_rank: 0` and each band denies at its own `>=`, with
/// the band recorded as a non-match rather than as an evaluation error nothing
/// writes down.
fn build_context(ctx: &AuthzContext<'_>, authority: &Authority) -> Result<Context, AuthzError> {
    let request_ip = ctx
        .request_ip
        .map_or_else(|| "0.0.0.0".to_owned(), |ip| ip.to_string());
    let mfa_age_seconds = ctx.mfa_age_seconds.unwrap_or(u32::MAX);
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
        (
            "mfa_verified".to_owned(),
            restricted(if ctx.mfa_verified { "true" } else { "false" })?,
        ),
        (
            "mfa_age_seconds".to_owned(),
            restricted(&mfa_age_seconds.to_string())?,
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
                &ctx.principal_id,
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
    use super::audit_resource;
    use crate::Resource;

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
}
