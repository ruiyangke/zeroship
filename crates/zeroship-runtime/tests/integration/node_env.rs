//! `process.env.NODE_ENV` defaults: a deployed isolate reports `production`,
//! the local dev tier reports `development`, and a creator's `NODE_ENV` app
//! variable overrides both.
//!
//! The dev tier is a process-wide stated input (`set_dev_mode`). Each case that
//! states it is ignored in the shared run and executed alone in a child copy of
//! this binary by its spawner, so no other case ever observes the mode it sets.

use crate::support::*;
use std::collections::BTreeMap;
use zeroship_runtime::{EnvSnapshot, FetchOutcome};

fn vars(items: &[(&str, &str)]) -> BTreeMap<String, String> {
    items
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// Build an isolate in `dev` mode with `app_vars` as the creator's env, and
/// return the JSON body reporting what `process.env.NODE_ENV` sees.
fn reported_node_env(app_vars: &[(&str, &str)], dev: bool) -> String {
    zeroship_runtime::set_dev_mode(dev);
    let modules = m(r#"
        export default {
            fetch() {
                return Response.json({ NODE_ENV: globalThis.process.env.NODE_ENV });
            }
        };
    "#);
    let env = EnvSnapshot::new(vars(app_vars), BTreeMap::new(), Vec::new());
    match dispatch_fetch_with_env(modules, TestRequest::get("http://localhost/"), env) {
        FetchOutcome::Response { status, body, .. } => {
            let body = body_to_string(&body);
            assert_eq!(status, 200, "body: {body}");
            body
        }
        _ => panic!("expected a synchronous Response"),
    }
}

/// Run one ignored case of this module alone in a child copy of this binary.
fn passes_alone(case: &str) {
    let name = format!("integration::node_env::{case}");
    let out = std::process::Command::new(std::env::current_exe().expect("current_exe"))
        .args(["--exact", &name, "--ignored", "--nocapture", "--test-threads=1"])
        .output()
        .expect("spawn a child copy of this test binary");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{name} failed alone:\n{text}");
    assert!(
        text.contains("1 passed"),
        "the child ran no test, so it proved nothing:\n{text}"
    );
}

#[test]
#[ignore = "states a process-wide cell; its spawner runs it alone in a child"]
fn deployed_isolate_defaults_to_production() {
    assert_eq!(reported_node_env(&[], false), r#"{"NODE_ENV":"production"}"#);
}

#[test]
fn deployed_isolate_defaults_to_production_in_an_isolated_process() {
    passes_alone("deployed_isolate_defaults_to_production");
}

#[test]
#[ignore = "states a process-wide cell; its spawner runs it alone in a child"]
fn dev_isolate_defaults_to_development() {
    assert_eq!(reported_node_env(&[], true), r#"{"NODE_ENV":"development"}"#);
}

#[test]
fn dev_isolate_defaults_to_development_in_an_isolated_process() {
    passes_alone("dev_isolate_defaults_to_development");
}

#[test]
#[ignore = "states a process-wide cell; its spawner runs it alone in a child"]
fn app_variable_overrides_the_default_in_both_tiers() {
    let staging = &[("NODE_ENV", "staging")];
    assert_eq!(reported_node_env(staging, false), r#"{"NODE_ENV":"staging"}"#);
    assert_eq!(reported_node_env(staging, true), r#"{"NODE_ENV":"staging"}"#);
}

#[test]
fn app_variable_overrides_the_default_in_both_tiers_in_an_isolated_process() {
    passes_alone("app_variable_overrides_the_default_in_both_tiers");
}
