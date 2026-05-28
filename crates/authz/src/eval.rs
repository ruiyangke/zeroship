use std::collections::HashMap;
use std::net::IpAddr;
use std::str::FromStr;

use cedar_policy::{Context, Decision, EntityUid, PolicySet, Request, RestrictedExpression};
use compio_postgres::Client;
use serde_json::Value;
use uuid::Uuid;

use crate::entities::{cedar_string, uid};
use crate::{assemble_entities, lower, Action, AuthzError, Policy, Resource};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthzDecision {
    Allow,
    Deny,
}

#[derive(Debug)]
pub struct AuthzContext<'a> {
    pub principal_id: Uuid,
    pub token_id: Option<Uuid>,
    pub token_policy: Option<Policy>,
    pub action: Action,
    pub resource: Resource,
    pub request_ip: Option<IpAddr>,
    pub mfa_verified: bool,
    pub mfa_age_seconds: Option<u32>,
    pub request_id: Option<&'a str>,
}

/// Enforce static owner permissions and optional token policies.
///
/// When `token_id` or `token_policy` is present this intentionally calls Cedar
/// twice: owner-without-token first, then token policies. This preserves
/// TOKEN ⊂ USER after app memberships or platform roles change.
pub async fn enforce(
    pg: &Client,
    static_policies: &PolicySet,
    ctx: &AuthzContext<'_>,
) -> Result<AuthzDecision, AuthzError> {
    let entities = assemble_entities(pg, ctx.principal_id, ctx.action, ctx.resource.clone()).await?;

    if ctx.token_id.is_some() || ctx.token_policy.is_some() {
        let owner_req = build_request(ctx)?;
        let owner_decision =
            cedar_policy::Authorizer::new().is_authorized(&owner_req, static_policies, &entities);
        if owner_decision.decision() != Decision::Allow {
            audit_decision(pg, ctx, AuthzDecision::Deny).await;
            return Ok(AuthzDecision::Deny);
        }
    }

    let final_policies = if let Some(token_policy) = &ctx.token_policy {
        policy_set_from_policy(token_policy)?
    } else if let Some(token_id) = ctx.token_id {
        load_token_policies(pg, token_id).await?
    } else {
        static_policies.clone()
    };

    let req = build_request(ctx)?;
    let decision = cedar_policy::Authorizer::new().is_authorized(&req, &final_policies, &entities);

    let decision = if decision.decision() == Decision::Allow {
        AuthzDecision::Allow
    } else {
        AuthzDecision::Deny
    };
    audit_decision(pg, ctx, decision).await;
    Ok(decision)
}

/// Return true when the principal can perform `ctx.action` on any resource
/// they currently control: platform-wide `Resource::Any` first, then each app
/// membership resource. This is used for grant/consent checks where the user is
/// delegating an action vocabulary, not authorizing one concrete app request.
pub async fn is_authorized_anywhere(
    pg: &Client,
    static_policies: &PolicySet,
    ctx: &AuthzContext<'_>,
) -> Result<bool, AuthzError> {
    let mut resources = vec![Resource::Any];
    resources.extend(load_principal_app_resources(pg, ctx.principal_id).await?);

    for resource in resources {
        let probe = AuthzContext {
            principal_id: ctx.principal_id,
            token_id: None,
            token_policy: None,
            action: ctx.action,
            resource,
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

async fn load_principal_app_resources(
    pg: &Client,
    principal_id: Uuid,
) -> Result<Vec<Resource>, AuthzError> {
    let rows = pg
        .query(
            "SELECT DISTINCT app_id FROM control.app_members WHERE user_id = $1",
            &[&principal_id],
        )
        .await
        .map_err(|err| AuthzError::Db(format!("load app grant resources: {err}")))?;

    rows.into_iter()
        .map(|row| {
            let resource = Resource::App {
                id: row.get("app_id"),
            };
            resource
                .validate_ids()
                .map_err(|message| AuthzError::Validation(message.to_owned()))?;
            Ok(resource)
        })
        .collect()
}

fn policy_set_from_policy(policy: &Policy) -> Result<PolicySet, AuthzError> {
    PolicySet::from_str(&lower(policy)).map_err(|err| AuthzError::CedarParse(err.to_string()))
}

async fn load_token_policies(pg: &Client, token_id: Uuid) -> Result<PolicySet, AuthzError> {
    let rows = pg
        .query(
            "SELECT policies FROM control.permission_tokens \
             WHERE id = $1 \
               AND revoked_at IS NULL \
               AND (expires_at IS NULL OR expires_at > NOW())",
            &[&token_id],
        )
        .await
        .map_err(|err| AuthzError::Db(format!("load token policies: {err}")))?;
    let row = rows
        .first()
        .ok_or_else(|| AuthzError::Validation(format!("permission token not active: {token_id}")))?;
    let wrapper_json: Value = row.get("policies");
    let wrapper = Policy::from_json_value(&wrapper_json)?;
    policy_set_from_policy(&wrapper)
}

fn build_request(ctx: &AuthzContext<'_>) -> Result<Request, AuthzError> {
    let principal = uid("User", &ctx.principal_id.to_string())?;
    let action = uid("Action", ctx.action.cedar_id())?;
    let resource = resource_uid(&ctx.resource)?;
    let context = build_context(ctx)?;
    Request::new(principal, action, resource, context, None)
        .map_err(|err| AuthzError::CedarRequest(err.to_string()))
}

fn build_context(ctx: &AuthzContext<'_>) -> Result<Context, AuthzError> {
    let request_ip = ctx
        .request_ip
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "0.0.0.0".to_owned());
    let mfa_age_seconds = ctx.mfa_age_seconds.unwrap_or(u32::MAX);

    let pairs = HashMap::from([
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
    ]);
    Context::from_pairs(pairs).map_err(|err| AuthzError::CedarRequest(err.to_string()))
}

fn restricted(source: &str) -> Result<RestrictedExpression, AuthzError> {
    RestrictedExpression::from_str(source)
        .map_err(|err| AuthzError::CedarRequest(err.to_string()))
}

fn resource_uid(resource: &Resource) -> Result<EntityUid, AuthzError> {
    match resource {
        Resource::App { id } => uid("App", id),
        Resource::Org { id } => uid("Org", id),
        Resource::Any => uid("Resource", "*"),
    }
}

async fn audit_decision(pg: &Client, ctx: &AuthzContext<'_>, decision: AuthzDecision) {
    let (resource_type, resource_id) = audit_resource(&ctx.resource);
    let decision = match decision {
        AuthzDecision::Allow => "allow",
        AuthzDecision::Deny => "deny",
    };
    let request_ip = ctx.request_ip;
    if let Err(err) = pg
        .execute(
            "INSERT INTO control.authz_decisions \
                (user_id, token_id, action, resource_type, resource_id, decision, request_ip, request_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                &ctx.principal_id,
                &ctx.token_id,
                &ctx.action.cedar_id(),
                &resource_type,
                &resource_id,
                &decision,
                &request_ip,
                &ctx.request_id,
            ],
        )
        .await
    {
        tracing::error!(error = %err, "authz decision audit insert failed");
    }
}

fn audit_resource(resource: &Resource) -> (&'static str, Option<String>) {
    match resource {
        Resource::App { id } => ("app", Some(id.clone())),
        Resource::Org { id } => ("org", Some(id.clone())),
        Resource::Any => ("any", None),
    }
}
