use std::fmt::Write as _;

use crate::{Condition, Effect, Policy, Resource, Statement};

/// Lowers a wrapper policy into Cedar source.
#[must_use]
pub fn lower(policy: &Policy) -> String {
    let mut source = String::new();
    writeln!(&mut source, "// Policy: {}", policy.name).expect("writing to String is infallible");

    for (index, statement) in policy.statements.iter().enumerate() {
        append_time_window_todos(&mut source, statement);

        let effect = match statement.effect {
            Effect::Allow => "permit",
            Effect::Deny => "forbid",
        };

        writeln!(&mut source, "{effect} (").expect("writing to String is infallible");
        source.push_str("  principal,\n");
        let action_scope = lower_actions(&statement.actions);
        writeln!(&mut source, "  {action_scope},").expect("writing to String is infallible");
        let resource_lowering = lower_resources(&statement.resources);
        writeln!(&mut source, "  {}", resource_lowering.scope)
            .expect("writing to String is infallible");
        source.push(')');

        let when_clause = lower_when_clause(statement, resource_lowering.condition);
        if let Some(when_clause) = when_clause {
            source.push_str("\nwhen {\n  ");
            source.push_str(&when_clause);
            source.push_str("\n}");
        }

        source.push_str(";\n");
        if index + 1 < policy.statements.len() {
            source.push('\n');
        }
    }

    source
}

fn lower_actions(actions: &[crate::Action]) -> String {
    let actions = actions
        .iter()
        .map(|action| format!("Action::{}", cedar_string(action.cedar_id())))
        .collect::<Vec<_>>()
        .join(", ");

    format!("action in [{actions}]")
}

struct ResourceLowering {
    scope: String,
    condition: Option<String>,
}

fn lower_resources(resources: &[Resource]) -> ResourceLowering {
    if resources.iter().any(|resource| matches!(resource, Resource::Any)) {
        return ResourceLowering {
            scope: "resource".to_owned(),
            condition: None,
        };
    }

    match resources {
        [] => ResourceLowering {
            scope: "resource".to_owned(),
            condition: Some("false".to_owned()),
        },
        [resource] => ResourceLowering {
            scope: format!("resource == {}", resource.cedar_uid()),
            condition: None,
        },
        resources => {
            let condition = resources
                .iter()
                .map(|resource| format!("resource == {}", resource.cedar_uid()))
                .collect::<Vec<_>>()
                .join(" || ");

            ResourceLowering {
                scope: "resource".to_owned(),
                condition: Some(format!("({condition})")),
            }
        }
    }
}

fn lower_when_clause(statement: &Statement, resource_condition: Option<String>) -> Option<String> {
    let mut parts = Vec::new();

    if let Some(resource_condition) = resource_condition {
        parts.push(resource_condition);
    }

    if !statement.conditions.is_empty() {
        parts.push(lower_conditions(&statement.conditions));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" && "))
    }
}

fn lower_conditions(conditions: &[Condition]) -> String {
    conditions
        .iter()
        .map(lower_condition)
        .collect::<Vec<_>>()
        .join(" && ")
}

fn lower_condition(condition: &Condition) -> String {
    match condition {
        Condition::IpRange { cidrs } => {
            if cidrs.is_empty() {
                return "false".to_owned();
            }

            cidrs
                .iter()
                .map(|cidr| {
                    format!(
                        "context.request_ip.isInRange(ip({}))",
                        cedar_string(cidr)
                    )
                })
                .collect::<Vec<_>>()
                .join(" || ")
        }
        Condition::TimeWindow { .. } => "true".to_owned(),
        Condition::RequireMfa => "context.mfa_verified == true".to_owned(),
        Condition::MfaWithin { seconds } => format!("context.mfa_age_seconds <= {seconds}"),
    }
}

fn append_time_window_todos(source: &mut String, statement: &Statement) {
    for condition in &statement.conditions {
        if let Condition::TimeWindow { start, end, tz } = condition {
            writeln!(
                source,
                "// TODO(authz): TimeWindow(start={start}, end={end}, tz={tz}) lowers to true in P9-U2; wire context.now/local-time comparison before enforcing it."
            )
            .expect("writing to String is infallible");
        }
    }
}

fn cedar_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    escaped.push('"');
    escaped
}
