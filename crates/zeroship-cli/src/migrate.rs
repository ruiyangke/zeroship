//! `zeroship migrate` - apply an app's committed migrations to its DEPLOYED
//! database.
//!
//! Shape:
//!   zeroship migrate [path-to-migrations.ir.json] [--app=<id>] [--app-name=<name>]
//!                    [--control=URL] [--token=TOKEN] [--config=PATH] [--env=NAME] [--yes]
//!
//! The path comes from `zeroship.jsonc`'s `migrations.out` unless a positional
//! overrides it. There is NO compiled default: the build decides where it
//! writes the recorded migration set, and a Rust constant guessing the same
//! string is how the two came to disagree in the first place.
//!
//! The file is the request body verbatim - the CLI
//! does not build, parse or rewrite it, because recording a `.ts` migration
//! means EVALUATING it, which needs Node, esbuild and the installed
//! `zero-migrate` engine (`sdks/vite-plugin/src/gen-types/recorder.ts`). That
//! work belongs to the build; shipping the result belongs here. Same division
//! as `zeroship deploy`, which uploads a `.zship` it did not build.
//!
//! WHY THIS COMMAND EXISTS. Applying migrations is what creates the app's
//! schema, its migrator role, and the `app_<id>_role` the runtime does
//! `SET LOCAL ROLE` to on every `env.db` call
//! (`tests/fixtures/data/roles.rs`). Nothing else in the platform
//! creates that role - `migrated`'s apply path is its only producer
//! (`crates/zeroship-migrate-server/src/apply.rs`). Deploy an app that uses `env.db` without
//! applying its migrations and the FIRST database call fails with
//! `role "app_..._role" does not exist`, which reaches the end user as
//! `{"message":"internal error"}`. Before this command there was no supported
//! way for a creator to run the step at all.
//!
//! The request uses the configured control URL, but the edge sends the
//! migration-service path directly to `zeroship-migrate-server`. Control is not
//! in the request path. Reusing one creator-facing URL keeps project config from
//! needing a second endpoint for the same deployment.

use std::path::PathBuf;

use zeroship_core::AppId;

use crate::project_config::{self, ProjectConfig, Resolved};
use crate::{
    app_id_or_refuse, flag_str, parse_app_id, parse_flag, resolve_bearer_token, run_curl,
    AppTarget, ControlResponse,
};

/// The migration set to post: a positional path, else `<migrations.out>/<IR_FILENAME>`.
///
/// There is deliberately no third arm. Before this change the fallback was a
/// hardcoded `generated/zeroship/migrations.ir.json` while the build wrote
/// wherever `genTypesOut` said - two spellings of one fact, and the one the CLI
/// held could not see the one the build used.
fn resolve_ir_path(args: &[String], cfg: Option<&Resolved>) -> Result<PathBuf, String> {
    if let Some(p) = positional_path(args) {
        return Ok(PathBuf::from(p));
    }
    let Some(cfg) = cfg else {
        return Err(format!(
            "no migration set to apply. Pass the path written by the build \
             (`<migrations.out>/{IR_FILENAME}`), or add a {} declaring `migrations.out`.",
            project_config::CONFIG_FILENAME
        ));
    };
    Ok(cfg.require_path("migrations.out")?.join(IR_FILENAME))
}

/// The filename the build writes inside `migrations.out`.
///
/// This is a fact about the EMITTER's layout, not a path: `generated/zeroship/`
/// already holds the other two artifacts the migration fold produces
/// (`env.db.ts`, `schema.runtime.json`), all three are written by the same emit
/// step, and the directory that holds them is read from `zeroship.jsonc`.
pub const IR_FILENAME: &str = "migrations.ir.json";

/// Known flags accepted by `zeroship migrate` (bare names, no `=`).
///
/// Read off the call sites, not off the usage string - same discipline as
/// `DEPLOY_KNOWN_FLAGS`. A typo'd `--control` here is worse than a confusing
/// error: it would silently target `http://localhost:9090` instead of the
/// control plane named on the command line, and applying a migration set to
/// the wrong database is not something an error message afterwards can undo.
const MIGRATE_KNOWN_FLAGS: &[&str] = &[
    "--app", "--app-name", "--control", "--token", "--config", "--env", "--yes",
];

pub fn cmd_migrate(args: &[String]) -> Result<(), String> {
    check_unknown_migrate_flags(args)?;

    let cwd = std::env::current_dir().map_err(|e| format!("cannot read the working directory: {e}"))?;
    let file = project_config::locate(args, &cwd)?;
    let loaded = file.as_deref().map(ProjectConfig::load).transpose()?;
    let resolved = match (&loaded, flag_str(args, "--env=")) {
        (Some(cfg), env) => Some(cfg.resolve(env.as_deref())?),
        // NO IMPLICIT ENVIRONMENT: `--env=` without a file is a
        // typo, not a request, and silently ignoring it would run against the
        // wrong target with the creator believing otherwise.
        (None, Some(env)) => {
            return Err(format!(
                "--env={env} needs a {} in this directory to read the environment from",
                project_config::CONFIG_FILENAME
            ))
        }
        (None, None) => None,
    };

    let (app, target) = resolve_migrate_app(args, resolved.as_ref())?;
    let control_url = project_config::resolve_control(args, resolved.as_ref())?;
    let token = resolve_bearer_token(args)?;

    let input = resolve_ir_path(args, resolved.as_ref())?;

    // BEFORE the POST, always. Applying a migration set to the wrong database
    // "is not something an error message afterwards can undo", and with a
    // config file the target is no longer visible in the command itself.
    project_config::print_provenance(
        "migrate",
        &[("app", &app), ("control", &control_url)],
    );
    eprintln!("zeroship migrate: migrations = {}", input.display());

    // A CORRECT config run at the wrong moment is the one failure the
    // provenance line cannot stop. `"protected": true` on an environment is the
    // creator saying so; `--yes` is them saying they meant it.
    if resolved.as_ref().is_some_and(project_config::Resolved::is_protected)
        && !args.iter().any(|a| a == "--yes")
    {
        return Err(format!(
            "the selected environment is marked \"protected\": true and this would apply \
             migrations to {}. Re-run with --yes if that is what you meant.",
            control_url.value
        ));
    }

    let (app, control_url) = (app.value, control_url.value);
    let body = std::fs::read_to_string(&input).map_err(|e| {
        format!(
            "failed to read {}: {e}\n\
             This file is written by the build (`pnpm build` / `vite build` with \
             @zeroship/vite-plugin) from the app's `migrations/*.ts`. An app with no \
             migrations has none to apply.",
            input.display()
        )
    })?;

    let mut client = CurlMigrateClient;
    let outcome = apply_migrations(&mut client, &control_url, &target, &token, &body)?;

    eprintln!(
        "Applied {} migration op(s) to app {} ({} skipped).",
        outcome.applied, app, outcome.skipped
    );
    if let Some(id) = outcome.migration_id {
        eprintln!("  migration_id: {id}");
    }
    Ok(())
}

/// The first non-flag argument after the subcommand, if any.
///
/// `zeroship migrate --app=x` must NOT read `--app=x` as the path; the default
/// applies instead.
fn positional_path(args: &[String]) -> Option<String> {
    args.get(2)
        .filter(|arg| !arg.starts_with("--"))
        .cloned()
}

pub(crate) fn check_unknown_migrate_flags(args: &[String]) -> Result<(), String> {
    // args[0] = binary, args[1] = "migrate", args[2] = optional path.
    for arg in args.iter().skip(2) {
        if !arg.starts_with("--") {
            continue;
        }
        let flag_name = match arg.find('=') {
            Some(idx) => &arg[..idx],
            None => arg.as_str(),
        };
        if !MIGRATE_KNOWN_FLAGS.contains(&flag_name) {
            return Err(format!(
                "unknown flag `{flag_name}`; a typo here is silent - `--control` \
                 falling back to its default would apply migrations to \
                 http://localhost:9090 instead of the control plane you named. \
                 Usage: zeroship migrate [path-to-migrations.ir.json] \
                 [--app=<id>] [--app-name=<name>] [--control=<url>] [--token=<token>] \
                 [--config=<path>] [--env=<name>] [--yes]"
            ));
        }
    }
    Ok(())
}

/// The migration-service reply, reduced to what the command prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrateOutcome {
    pub applied: usize,
    pub skipped: usize,
    pub migration_id: Option<String>,
}

pub(crate) trait MigrateClient {
    fn apply(
        &mut self,
        control_url: &str,
        app_id: &AppId,
        token: &str,
        body: &str,
    ) -> Result<ControlResponse, String>;

    fn list_apps(&mut self, control_url: &str, token: &str) -> Result<ControlResponse, String>;
}

struct CurlMigrateClient;

fn migration_apply_url(control_url: &str, app_id: &AppId) -> String {
    format!("{control_url}/v1/apps/{}/migrations/apply", app_id.as_str())
}

impl MigrateClient for CurlMigrateClient {
    fn apply(
        &mut self,
        control_url: &str,
        app_id: &AppId,
        token: &str,
        body: &str,
    ) -> Result<ControlResponse, String> {
        let url = migration_apply_url(control_url, app_id);
        let auth = format!("Authorization: Bearer {token}");
        let mut command = std::process::Command::new("curl");
        command.args([
            "-s",
            "-w",
            "\n%{http_code}",
            // An apply runs DDL on the deployed database. curl's default has no
            // total timeout, which is right here: cutting the client off
            // mid-apply would leave the creator guessing whether the schema
            // changed. The connect timeout still fails fast on a wrong --control.
            "--connect-timeout",
            "10",
            "-X",
            "POST",
            &url,
            "-H",
            &auth,
            "-H",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
        ]);
        run_curl(&mut command, Some(body.as_bytes()))
    }

    fn list_apps(&mut self, control_url: &str, token: &str) -> Result<ControlResponse, String> {
        let url = format!("{control_url}/api/apps");
        let auth = format!("Authorization: Bearer {token}");
        let mut command = std::process::Command::new("curl");
        command.args(["-s", "-w", "\n%{http_code}", "-H", &auth, &url]);
        run_curl(&mut command, None)
    }
}

/// Decide WHAT `migrate` was pointed at, from which input carried the value.
///
/// The same rule as `deploy`'s `resolve_deploy_app` and one deliberate
/// difference: there is NO `name` fallback. `migrate`'s `app` key is required,
/// because the fallback's purpose - a brand-new project that has no id yet -
/// is a deploy affordance, and letting it apply here is the mint-and-migrate
/// failure the command's own docs describe.
fn resolve_migrate_app(
    args: &[String],
    resolved: Option<&Resolved>,
) -> Result<(project_config::Sourced, AppTarget), String> {
    if let Some(name) = parse_flag(args, "--app-name") {
        if parse_flag(args, "--app").is_some() {
            return Err(
                "--app and --app-name both name a target; pass one. --app takes the \
                 app's ID (the identity), --app-name its routing label."
                    .to_string(),
            );
        }
        return Ok((
            project_config::Sourced {
                value: name.clone(),
                source: project_config::Source::Flag("--app-name"),
            },
            AppTarget::Name(name),
        ));
    }

    let sourced = project_config::resolve_value(args, "--app", None, None, resolved, "app", None)?;
    let id = app_id_or_refuse(&sourced.value)?;
    Ok((sourced, AppTarget::Id(id)))
}

/// POST the body to the app `app` names, resolving a name through the app list.
///
/// Deliberately NOT auto-creating a missing app, unlike `deploy`: creating an
/// app is the right answer to "push this code somewhere new", and the wrong
/// answer to "apply my schema" - a typo'd name would mint an empty app and
/// migrate it while the real app stayed unmigrated, which is the exact silent
/// failure this command exists to end.
pub(crate) fn apply_migrations<C: MigrateClient>(
    client: &mut C,
    control_url: &str,
    app: &AppTarget,
    token: &str,
    body: &str,
) -> Result<MigrateOutcome, String> {
    let app_id = match app {
        AppTarget::Id(id) => id.clone(),
        AppTarget::Name(name) => resolve_app_id_by_name(client, control_url, token, name)?,
    };

    let response = client.apply(control_url, &app_id, token, body)?;
    if response.status != 200 {
        return Err(format!(
            "Migration apply failed (HTTP {}): {}",
            response.status, response.body
        ));
    }
    parse_outcome(&response.body)
}

fn resolve_app_id_by_name<C: MigrateClient>(
    client: &mut C,
    control_url: &str,
    token: &str,
    name: &str,
) -> Result<AppId, String> {
    let list = client.list_apps(control_url, token)?;
    if list.status != 200 {
        return Err(format!(
            "app lookup failed (HTTP {}): {}",
            list.status, list.body
        ));
    }
    let json = serde_json::from_str::<serde_json::Value>(&list.body)
        .map_err(|e| format!("parse app list response: {e}"))?;
    let apps = json
        .as_array()
        .ok_or_else(|| "parse app list response: expected array".to_string())?;
    for app in apps {
        if app.get("name").and_then(|n| n.as_str()) == Some(name) {
            return parse_app_id(&app.to_string(), "app list response");
        }
    }
    Err(format!(
        "app `{name}` not found; `zeroship migrate` never creates an app - \
         deploy it first, or pass its id with --app="
    ))
}

fn parse_outcome(body: &str) -> Result<MigrateOutcome, String> {
    let json = serde_json::from_str::<serde_json::Value>(body)
        .map_err(|e| format!("parse migration apply response: {e}"))?;
    let count = |key: &str| {
        json.get(key)
            .and_then(|v| v.as_array())
            .map(Vec::len)
            .unwrap_or(0)
    };
    Ok(MigrateOutcome {
        applied: count("applied"),
        skipped: count("skipped"),
        migration_id: json
            .get("migration_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    const APP_ID: &str = "app_034klb07lrb9jgma6imvmx000";

    fn app_id(raw: &str) -> AppId {
        AppId::parse(raw).expect("test app id must be canonical")
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum FakeCall {
        Apply(String),
        List,
    }

    #[derive(Default)]
    struct FakeMigrateClient {
        calls: Vec<FakeCall>,
        applies: VecDeque<ControlResponse>,
        lists: VecDeque<ControlResponse>,
    }

    impl FakeMigrateClient {
        fn with_apply(mut self, status: u16, body: &str) -> Self {
            self.applies.push_back(ControlResponse {
                status,
                body: body.to_string(),
            });
            self
        }

        fn with_list(mut self, status: u16, body: &str) -> Self {
            self.lists.push_back(ControlResponse {
                status,
                body: body.to_string(),
            });
            self
        }
    }

    impl MigrateClient for FakeMigrateClient {
        fn apply(
            &mut self,
            _control_url: &str,
            app_id: &AppId,
            _token: &str,
            _body: &str,
        ) -> Result<ControlResponse, String> {
            self.calls
                .push(FakeCall::Apply(app_id.as_str().to_string()));
            self.applies
                .pop_front()
                .ok_or_else(|| "unexpected apply call".to_string())
        }

        fn list_apps(
            &mut self,
            _control_url: &str,
            _token: &str,
        ) -> Result<ControlResponse, String> {
            self.calls.push(FakeCall::List);
            self.lists
                .pop_front()
                .ok_or_else(|| "unexpected list call".to_string())
        }
    }

    /// An id goes straight to the apply endpoint - no lookup, so a creator
    /// whose token cannot list apps can still migrate the one they own.
    #[test]
    fn typed_app_applies_without_a_lookup() {
        let app = app_id(APP_ID);
        let mut client = FakeMigrateClient::default().with_apply(
            200,
            r#"{"migration_id":"0197f8a1-2b3c-7d4e-8f90-1a2b3c4d5e6f","applied":["20260101000000_create_todos"],"skipped":[],"pending_contract":[]}"#,
        );

        let outcome = apply_migrations(
            &mut client,
            "http://control.test",
            &AppTarget::Id(app),
            "tok",
            "{}",
        )
        .expect("apply by id");

        assert_eq!(outcome.applied, 1);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(client.calls, vec![FakeCall::Apply(APP_ID.to_string())]);
    }

    #[test]
    fn apply_url_uses_the_migration_service_route_on_the_control_origin() {
        assert_eq!(
            migration_apply_url("https://control.zeroship.ai", &app_id(APP_ID)),
            "https://control.zeroship.ai/v1/apps/app_034klb07lrb9jgma6imvmx000/migrations/apply",
        );
    }

    /// A name is resolved through the app list before the apply.
    #[test]
    fn name_app_resolves_then_applies_by_id() {
        let mut client = FakeMigrateClient::default()
            .with_list(
                200,
                r#"[{"id":"app_034klb07lrb9jgma6imvmx000","name":"todos"}]"#,
            )
            .with_apply(200, r#"{"applied":[],"skipped":["a","b"]}"#);

        let outcome = apply_migrations(
            &mut client,
            "http://control.test",
            &AppTarget::Name("todos".to_string()),
            "tok",
            "{}",
        )
        .expect("apply by name");

        assert_eq!(outcome.applied, 0);
        assert_eq!(outcome.skipped, 2);
        assert_eq!(
            client.calls,
            vec![FakeCall::List, FakeCall::Apply(APP_ID.to_string()),]
        );
    }

    /// A name that matches nothing must NOT create an app, and must say so.
    /// `deploy` creates on first push; migrate must not, or a typo silently
    /// migrates a brand-new empty app while the real one stays broken.
    #[test]
    fn unknown_name_fails_without_creating_anything() {
        let mut client = FakeMigrateClient::default().with_list(200, "[]");

        let err = apply_migrations(
            &mut client,
            "http://control.test",
            &AppTarget::Name("typo".to_string()),
            "tok",
            "{}",
        )
        .expect_err("unknown app must fail");

        assert!(err.contains("never creates an app"), "{err}");
        assert_eq!(client.calls, vec![FakeCall::List]);
    }

    /// The migration service's own status and body must survive to the user.
    /// A 422 naming the bad document is the whole diagnostic; replacing it with
    /// a generic message reproduces the failure this command exists to end.
    #[test]
    fn service_error_body_reaches_the_user() {
        let app = app_id(APP_ID);
        let mut client = FakeMigrateClient::default().with_apply(
            422,
            r#"{"error":"migration_malformed","detail":"malformed IR document (20260101000000_create_todos.ir.json): unknown op"}"#,
        );

        let err = apply_migrations(
            &mut client,
            "http://control.test",
            &AppTarget::Id(app),
            "tok",
            "{}",
        )
        .expect_err("422 must fail");

        assert!(err.contains("HTTP 422"), "{err}");
        assert!(err.contains("20260101000000_create_todos.ir.json"), "{err}");
    }

    /// A typo'd flag is rejected before any request is built.
    #[test]
    fn unknown_migrate_flag_is_rejected() {
        let args = s(&["zeroship", "migrate", "--app=todos", "--contrl=http://x"]);
        let err = check_unknown_migrate_flags(&args).expect_err("typo must be rejected");
        assert!(err.contains("--contrl"), "{err}");

        let args = s(&[
            "zeroship",
            "migrate",
            "generated/zeroship/migrations.ir.json",
            "--app=todos",
            "--control=http://localhost:9090",
            "--token=pat",
        ]);
        assert!(check_unknown_migrate_flags(&args).is_ok());
    }

    /// The optional path must not swallow a flag, and must default when absent.
    #[test]
    fn positional_path_defaults_and_ignores_flags() {
        assert_eq!(
            positional_path(&s(&["zeroship", "migrate", "--app=todos"])),
            None
        );
        assert_eq!(
            positional_path(&s(&["zeroship", "migrate", "custom.ir.json", "--app=todos"])),
            Some("custom.ir.json".to_string())
        );
        assert_eq!(positional_path(&s(&["zeroship", "migrate"])), None);
    }
}
