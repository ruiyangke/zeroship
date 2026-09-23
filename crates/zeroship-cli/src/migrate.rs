//! `zeroship migrate` - apply a database's committed migrations to the DEPLOYED
//! database that app is bound to.
//!
//! Shape:
//!   zeroship migrate [path-to-migrations-dir] [--app=<id>] [--app-name=<name>]
//!                    [--database=<label>] [--control=URL] [--token=TOKEN]
//!                    [--config=PATH] [--env=NAME] [--yes]
//!
//! The directory comes from that database's `migrations` in `zeroship.jsonc`
//! unless a positional overrides it. There is NO compiled default: the workspace
//! decides where its migrations live, and a Rust constant guessing the same
//! string is how the two came to disagree in the first place.
//!
//! # The IR exists on the wire and nowhere else
//!
//! The command reads the creator's `.ts` migrations and RECORDS them, here, at
//! apply time. There is no pre-built file to read and nothing produces one: a
//! creator's migrations exist once, as the `.ts` they wrote, and a second
//! encoding of them on disk would be hand-editable, independently stale, and
//! one more thing to regenerate before every apply.
//!
//! **The recording runs in Node, not in this process**, and that is forced
//! twice over. The migration service is Rust and holds the privileged
//! credential, so it must never evaluate creator TypeScript; and nothing in
//! `crates/` transpiles TypeScript at all - a creator's `.ts` is stripped by
//! esbuild in the JavaScript toolchain, and the recorder drains a module-level
//! singleton inside `@zeroship/migrate`, so a Rust recorder would be a SECOND
//! implementation of the authoring DSL. So this spawns the one recorder the
//! build already uses ([`record_apply_request`]) and posts what comes back off
//! its pipe, in memory. Node is already required to produce the `.zship` this
//! app deploys, so nothing new is required of the machine.
//!
//! **The target is the DATABASE, and it is in the URL rather than the body**,
//! precisely because the body is posted as the recorder produced it. `--database`
//! names one of the labels this app declares, and the CLI dereferences it to a
//! `dbs_` id here, before the request: a label is local to one `zeroship.jsonc`
//! and must never travel as an identifier.
//!
//! WHY THIS COMMAND EXISTS. Applying migrations is what puts a creator's tables
//! in their database. Deploy an app that uses `env.db` without applying its
//! migrations and the FIRST database call fails with
//! `42703 undefined_column` or a missing relation, which reaches the end user as
//! `{"message":"internal error"}`. Before this command there was no supported
//! way for a creator to run the step at all.
//!
//! The request uses the configured control URL, but the edge sends the
//! migration-service path directly to `zeroship-migrate-server`. Control is not
//! in the request path. Reusing one creator-facing URL keeps project config from
//! needing a second endpoint for the same deployment.

use std::path::{Path, PathBuf};

use zeroship_core::{AppId, DatabaseId};

use crate::project_config::{self, ProjectConfig, Resolved};
use crate::{
    app_id_or_refuse, flag_str, parse_app_id, parse_flag, resolve_bearer_token, run_curl,
    AppTarget, ControlResponse,
};

/// The migrations to record: a positional directory, else that database's own
/// `migrations` from `zeroship.jsonc`.
///
/// There is deliberately no third arm, and no arm for a pre-built IR file. The
/// recorded set is never written to disk, so a path to one names a shape nothing
/// in the toolchain produces; a positional that IS a file is refused by name
/// rather than read, because its documents are whatever was put in it rather
/// than what this app's `.ts` declare.
///
/// **A relative positional is resolved against `cwd` HERE**, before it can be
/// handed to a child process whose own working directory is the recording
/// target: re-resolving `migrations` inside `.../migrations` would name a
/// directory that does not exist and report an app with no migrations.
fn resolve_migrations_dir(
    args: &[String],
    cwd: &Path,
    cfg: Option<&Resolved>,
    database_label: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(p) = positional_path(args) {
        // `join` with an absolute right-hand side yields that side, so this is
        // "make relative absolute" and not "prefix everything".
        return Ok(cwd.join(p));
    }
    let (Some(cfg), Some(label)) = (cfg, database_label) else {
        return Err(format!(
            "no migrations to apply. Pass the directory holding this database's \
             `migrations/*.ts`, or add a {} declaring the database under `databases`.",
            project_config::CONFIG_FILENAME
        ));
    };
    // THE MIGRATIONS COME FROM THE DATABASE'S OWN `migrations`, so what is
    // recorded and the schema it lands in are two readings of ONE label. A
    // directory taken from any other database's entry would be a different set
    // of tables.
    cfg.database_path(label, "migrations")
}

/// The ESM one-liner `node` evaluates to hand back the apply-request body.
///
/// The recorder is the build's own (`@zeroship/vite-plugin`), reached by its
/// published subpath rather than by a path into the package, so the resolution
/// is the package's `exports` contract and not a guess about its layout.
const RECORD_APPLY_REQUEST_EVAL: &str = "process.stdout.write(await (await \
     import(\"@zeroship/vite-plugin/migrations-ir\")).recordApplyRequest(process.argv[1]));";

/// Record `migrations_dir` into the migration service's apply-request body.
///
/// # The pipe is the contract
///
/// `node` runs with its working directory set to `migrations_dir`, so the bare
/// specifier above resolves by walking UP from the migrations themselves: the
/// recorder that runs is the one the app whose migrations these are has
/// installed, never a different workspace's. On success the body is the whole of
/// stdout and nothing else - the recorder writes exactly one string there, and
/// Node's own warnings go to stderr. On failure the exit status is non-zero and
/// stderr carries the creator-facing message, which is forwarded whole: a
/// migration that fails to record names the file and the DSL guard that refused
/// it, and replacing that with a generic message would cost the creator the
/// diagnostic.
fn record_apply_request(migrations_dir: &Path) -> Result<String, String> {
    if migrations_dir.is_file() {
        return Err(format!(
            "{} is a file. `zeroship migrate` takes the DIRECTORY holding your \
             `migrations/*.ts`: it records them itself and posts the result, so there is no \
             recorded-migration file for it to read.",
            migrations_dir.display()
        ));
    }
    if !migrations_dir.is_dir() {
        return Err(format!(
            "no migrations directory at {}. An app with no migrations has none to apply.",
            migrations_dir.display()
        ));
    }
    // ABSOLUTE BEFORE IT CROSSES THE PROCESS BOUNDARY. `current_dir` moves the
    // child into this directory, so a relative path handed on as an argument
    // would be re-resolved against the directory it already names.
    let dir = migrations_dir.canonicalize().map_err(|e| {
        format!(
            "cannot resolve the migrations directory {}: {e}",
            migrations_dir.display()
        )
    })?;
    let output = std::process::Command::new("node")
        .arg("--input-type=module")
        .arg("--eval")
        .arg(RECORD_APPLY_REQUEST_EVAL)
        .arg(&dir)
        .current_dir(&dir)
        .output()
        .map_err(|e| {
            format!(
                "cannot run `node` to record {}: {e}\n\
                 `zeroship migrate` records your `migrations/*.ts` the same way the build \
                 does, which needs Node and the app's installed @zeroship/vite-plugin. \
                 Node already builds the .zship this app deploys.",
                migrations_dir.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "recording the migrations under {} failed:\n{}",
            migrations_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim_end()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|e| format!("the recorder produced a non-UTF-8 apply request: {e}"))
}

/// The database this apply targets, as the `dbs_` id that will be sent.
///
/// **The label is dereferenced HERE, before any request.** With a config file
/// present, `--database` names one of the labels `apps.<app>.databases`
/// declares - the file is the namespace the CLI resolves in, so a name two
/// workspaces could both choose never reaches a wire. With NO file there are no
/// labels, so `--database` is a `dbs_` id. The two cases are told apart by
/// whether there is a file, never by inspecting the value.
fn resolve_migrate_database(
    args: &[String],
    cfg: Option<&Resolved>,
    app_label: Option<&str>,
) -> Result<(Option<String>, DatabaseId), String> {
    let Some(cfg) = cfg else {
        let raw = parse_flag(args, "--database").ok_or_else(|| {
            format!(
                "which database? There is no {} in this directory to read a label from, so \
                 pass the id: --database=dbs_... . `zeroship db list --project=prj_...` \
                 shows them.",
                project_config::CONFIG_FILENAME
            )
        })?;
        let id = DatabaseId::parse(&raw).map_err(|_| {
            format!(
                "--database={raw:?} is not a database id (it looks like `dbs_<22 chars>`). \
                 There is no {} in this directory, so labels do not exist here.",
                project_config::CONFIG_FILENAME
            )
        })?;
        return Ok((None, id));
    };
    let app_label = app_label.ok_or_else(|| {
        format!(
            "{} declares no app for this command to read databases from.",
            cfg.path.display()
        )
    })?;
    let label = project_config::select_database(args, cfg, app_label)?;
    let raw_id = cfg.database_id(&label)?;
    let id = DatabaseId::parse(raw_id).map_err(|_| {
        format!(
            "{} declares `databases.{label}.id` as {raw_id:?}, which is not a database id \
             (it looks like `dbs_<22 chars>`)",
            cfg.path.display()
        )
    })?;
    Ok((Some(label), id))
}

/// Known flags accepted by `zeroship migrate` (bare names, no `=`).
///
/// Read off the call sites, not off the usage string - same discipline as
/// `DEPLOY_KNOWN_FLAGS`. A typo'd `--control` here is worse than a confusing
/// error: it would silently target `http://localhost:9090` instead of the
/// control plane named on the command line, and applying a migration set to
/// the wrong database is not something an error message afterwards can undo.
const MIGRATE_KNOWN_FLAGS: &[&str] = &[
    "--app", "--app-name", "--database", "--control", "--token", "--config", "--env", "--yes",
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

    let (label, app, target) = resolve_migrate_app(args, resolved.as_ref())?;
    let control_url = project_config::resolve_control(args, resolved.as_ref())?;
    let token = resolve_bearer_token(args)?;

    let (database_label, database) =
        resolve_migrate_database(args, resolved.as_ref(), label.as_deref())?;
    let migrations =
        resolve_migrations_dir(args, &cwd, resolved.as_ref(), database_label.as_deref())?;

    // BEFORE the POST, always. Applying a migration set to the wrong database
    // "is not something an error message afterwards can undo", and with a
    // config file the target is no longer visible in the command itself.
    project_config::print_provenance(
        "migrate",
        &[("app", &app), ("control", &control_url)],
    );
    eprintln!("zeroship migrate: migrations = {}", migrations.display());
    // The LABEL and the id it dereferenced to, side by side. The label is local
    // to this file and never travels; the id is what a server sees, and seeing
    // both is what tells a creator the dereference landed where they meant.
    match database_label.as_deref() {
        Some(label) => eprintln!(
            "zeroship migrate: database = {label} ({})",
            database.as_str()
        ),
        None => eprintln!("zeroship migrate: database = {}", database.as_str()),
    }

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
    // RECORDED AFTER the protected-environment refusal, so a run that is about
    // to be refused does not first evaluate the creator's migrations.
    let body = record_apply_request(&migrations)?;

    let mut client = CurlMigrateClient;
    let outcome = apply_migrations(&mut client, &control_url, &target, &database, &token, &body)?;

    eprintln!(
        "Applied {} migration op(s) to database {} for app {} ({} skipped).",
        outcome.applied,
        database.as_str(),
        app,
        outcome.skipped
    );
    if let Some(id) = outcome.migration_id {
        eprintln!("  migration_id: {id}");
    }
    Ok(())
}

/// The first non-flag argument after the subcommand, if any.
///
/// `zeroship migrate --app=x` must NOT read `--app=x` as the directory; the
/// default applies instead.
fn positional_path(args: &[String]) -> Option<String> {
    args.get(2)
        .filter(|arg| !arg.starts_with("--"))
        .cloned()
}

pub(crate) fn check_unknown_migrate_flags(args: &[String]) -> Result<(), String> {
    // args[0] = binary, args[1] = "migrate", args[2] = optional migrations dir.
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
                 Usage: zeroship migrate [path-to-migrations-dir] \
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
        database_id: &DatabaseId,
        token: &str,
        body: &str,
    ) -> Result<ControlResponse, String>;

    fn list_apps(&mut self, control_url: &str, token: &str) -> Result<ControlResponse, String>;
}

struct CurlMigrateClient;

/// The apply route, composed from two PARSED typed ids.
///
/// Both segments go through their id types before they reach this function, so
/// neither can be a label, a name, or anything else a path segment must not
/// carry. The app authorizes the call; the database is the target schema.
fn migration_apply_url(control_url: &str, app_id: &AppId, database_id: &DatabaseId) -> String {
    format!(
        "{control_url}/v1/apps/{}/databases/{}/migrations/apply",
        app_id.as_str(),
        database_id.as_str()
    )
}

impl MigrateClient for CurlMigrateClient {
    fn apply(
        &mut self,
        control_url: &str,
        app_id: &AppId,
        database_id: &DatabaseId,
        token: &str,
        body: &str,
    ) -> Result<ControlResponse, String> {
        let url = migration_apply_url(control_url, app_id, database_id);
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
) -> Result<(Option<String>, project_config::Sourced, AppTarget), String> {
    if let Some(name) = parse_flag(args, "--app-name") {
        if parse_flag(args, "--app").is_some() {
            return Err(
                "--app and --app-name both name a target; pass one. --app names an app \
                 the config file declares (an app ID when there is no file), --app-name \
                 its routing label."
                    .to_string(),
            );
        }
        return Ok((
            None,
            project_config::Sourced {
                value: name.clone(),
                source: project_config::Source::Flag("--app-name"),
            },
            AppTarget::Name(name),
        ));
    }

    let selection = project_config::select_app(args, resolved)?;
    let sourced = selection.id.ok_or_else(|| {
        format!(
            "`apps.{}` carries no `app` id yet. `zeroship migrate` never creates an app; \
             run `zeroship deploy` first.",
            selection.label.as_deref().unwrap_or("<none>")
        )
    })?;
    let id = app_id_or_refuse(&sourced.value)?;
    Ok((selection.label, sourced, AppTarget::Id(id)))
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
    database: &DatabaseId,
    token: &str,
    body: &str,
) -> Result<MigrateOutcome, String> {
    let app_id = match app {
        AppTarget::Id(id) => id.clone(),
        AppTarget::Name(name) => resolve_app_id_by_name(client, control_url, token, name)?,
    };

    let response = client.apply(control_url, &app_id, database, token, body)?;
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
    const DATABASE_ID: &str = "dbs_03cgepu94hyemwpcipafo7264";

    fn app_id(raw: &str) -> AppId {
        AppId::parse(raw).expect("test app id must be canonical")
    }

    fn database_id(raw: &str) -> DatabaseId {
        DatabaseId::parse(raw).expect("test database id must be canonical")
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum FakeCall {
        Apply(String, String),
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
            database_id: &DatabaseId,
            _token: &str,
            _body: &str,
        ) -> Result<ControlResponse, String> {
            self.calls.push(FakeCall::Apply(
                app_id.as_str().to_string(),
                database_id.as_str().to_string(),
            ));
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
            &database_id(DATABASE_ID),
            "tok",
            "{}",
        )
        .expect("apply by id");

        assert_eq!(outcome.applied, 1);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(
            client.calls,
            vec![FakeCall::Apply(APP_ID.to_string(), DATABASE_ID.to_string())]
        );
    }

    #[test]
    fn apply_url_names_the_app_and_the_database_it_targets() {
        assert_eq!(
            migration_apply_url(
                "https://control.zeroship.ai",
                &app_id(APP_ID),
                &database_id(DATABASE_ID),
            ),
            "https://control.zeroship.ai/v1/apps/app_034klb07lrb9jgma6imvmx000/databases/\
             dbs_03cgepu94hyemwpcipafo7264/migrations/apply",
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
            &database_id(DATABASE_ID),
            "tok",
            "{}",
        )
        .expect("apply by name");

        assert_eq!(outcome.applied, 0);
        assert_eq!(outcome.skipped, 2);
        assert_eq!(
            client.calls,
            vec![
                FakeCall::List,
                FakeCall::Apply(APP_ID.to_string(), DATABASE_ID.to_string()),
            ]
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
            &database_id(DATABASE_ID),
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
            &database_id(DATABASE_ID),
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
            "migrations",
            "--app=todos",
            "--control=http://localhost:9090",
            "--token=pat",
        ]);
        assert!(check_unknown_migrate_flags(&args).is_ok());
    }

    /// The shared cross-tool fixture: two databases, two apps, and
    /// `storefront` declaring both with `main` as its primary.
    fn fixture() -> Resolved {
        let text = include_str!("../../../tests/fixtures/project-config/zeroship.jsonc");
        ProjectConfig::parse(std::path::PathBuf::from("zeroship.jsonc"), text.to_string())
            .expect("the committed fixture must parse")
            .resolve(None)
            .expect("the committed fixture must resolve")
    }

    /// A NON-PRIMARY label is a legal target, and it resolves to ITS OWN id and
    /// ITS OWN `migrations`.
    ///
    /// The apply names the database, so `primary` decides only which handle is
    /// `env.db`; it does not decide which schema a migration set can reach. The
    /// primary is asserted alongside as the control, because a resolution that
    /// silently fell back to it would produce a legal-looking id and land the
    /// analytics tables in the main database.
    #[test]
    fn a_non_primary_database_label_resolves_to_its_own_id_and_migrations_path() {
        let cfg = fixture();
        let args = s(&[
            "zeroship",
            "migrate",
            "--app=storefront",
            "--database=analytics",
        ]);

        let (label, id) = resolve_migrate_database(&args, Some(&cfg), Some("storefront"))
            .expect("a non-primary label is a legal apply target");
        assert_eq!(label.as_deref(), Some("analytics"));
        assert_eq!(id.as_str(), "dbs_03evr3oqx1200qyvgmdnjrsla");
        assert_eq!(cfg.app_primary("storefront"), Some("main"));
        assert_ne!(
            id.as_str(),
            cfg.database_id("main").expect("the fixture declares main"),
            "the control: the primary is a different database from the one named"
        );

        let path = resolve_migrations_dir(&args, Path::new("/workspace"), Some(&cfg), label.as_deref())
            .expect("the migrations dir");
        assert!(
            path.ends_with("migrations/analytics"),
            "the migrations must come from the named database's own entry: {}",
            path.display()
        );
    }

    /// The control, differing in one variable: no `--database` at all resolves
    /// the primary, and its `migrations` is a different directory.
    #[test]
    fn no_database_flag_resolves_the_primary() {
        let cfg = fixture();
        let args = s(&["zeroship", "migrate", "--app=storefront"]);

        let (label, id) = resolve_migrate_database(&args, Some(&cfg), Some("storefront"))
            .expect("the primary is the default target");
        assert_eq!(label.as_deref(), Some("main"));
        assert_eq!(id.as_str(), "dbs_03evr3oqx1200yyd6zj2cebfw");

        let path = resolve_migrations_dir(&args, Path::new("/workspace"), Some(&cfg), label.as_deref())
            .expect("the migrations dir");
        assert!(path.ends_with("migrations"), "{}", path.display());
        assert!(
            !path.ends_with("migrations/analytics"),
            "the control: the primary's migrations are not the non-primary's: {}",
            path.display()
        );
    }

    /// A label the APP does not declare is refused before any request, naming
    /// the labels that exist. `admin` declares only `main`.
    #[test]
    fn a_database_the_app_does_not_use_is_refused() {
        let cfg = fixture();
        let args = s(&["zeroship", "migrate", "--app=admin", "--database=analytics"]);
        let error = resolve_migrate_database(&args, Some(&cfg), Some("admin"))
            .expect_err("an undeclared database must be refused");
        assert!(error.contains("analytics"), "{error}");
        assert!(error.contains("apps.admin"), "{error}");
    }

    /// With NO config file there is no label namespace, so `--database` is an
    /// id and a label-shaped value is refused rather than sent.
    #[test]
    fn without_a_config_the_database_flag_must_be_an_id() {
        let args = s(&["zeroship", "migrate", "--app=app_x", "--database=main"]);
        let error = resolve_migrate_database(&args, None, None)
            .expect_err("a label must not travel as an identifier");
        assert!(error.contains("dbs_"), "{error}");

        let args = s(&[
            "zeroship",
            "migrate",
            "--app=app_x",
            "--database=dbs_03cgepu94hyemwpcipafo7264",
        ]);
        let (label, id) =
            resolve_migrate_database(&args, None, None).expect("an id needs no namespace");
        assert_eq!(label, None);
        assert_eq!(id.as_str(), "dbs_03cgepu94hyemwpcipafo7264");
    }

    /// The optional path must not swallow a flag, and must default when absent.
    #[test]
    fn positional_path_defaults_and_ignores_flags() {
        assert_eq!(
            positional_path(&s(&["zeroship", "migrate", "--app=todos"])),
            None
        );
        assert_eq!(
            positional_path(&s(&["zeroship", "migrate", "db/migrations", "--app=todos"])),
            Some("db/migrations".to_string())
        );
        assert_eq!(positional_path(&s(&["zeroship", "migrate"])), None);
    }

    /// A RELATIVE positional is anchored to the CLI's working directory.
    ///
    /// The recorder runs with its working directory set to the migrations, so a
    /// relative path still spelled `migrations` when it reaches that child names
    /// `<migrations>/migrations` - a directory that does not exist, reported as
    /// an app with no migrations to apply. An absolute positional is the control:
    /// it must pass through untouched rather than be prefixed.
    #[test]
    fn a_relative_positional_is_anchored_to_the_working_directory() {
        let cwd = Path::new("/home/me/app");
        let relative = s(&["zeroship", "migrate", "db/migrations", "--app=todos"]);
        assert_eq!(
            resolve_migrations_dir(&relative, cwd, None, None).expect("a positional needs no file"),
            PathBuf::from("/home/me/app/db/migrations"),
        );

        let absolute = s(&["zeroship", "migrate", "/srv/app/migrations", "--app=todos"]);
        assert_eq!(
            resolve_migrations_dir(&absolute, cwd, None, None).expect("a positional needs no file"),
            PathBuf::from("/srv/app/migrations"),
        );
    }

    /// The repo root, from this crate's own manifest dir.
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("the crate sits two levels under the repo root")
    }

    /// The recorded migration set for `examples/db-todos`, as it goes on the
    /// wire.
    ///
    /// It is not free-floating. [`recording_db_todos_reproduces_the_posted_ir`]
    /// binds its `documents` to `examples/db-todos/migrations/*.ts` by recording
    /// them, and its `descriptor_sha256` to the committed `schema.runtime.json`
    /// by hashing that file - so a migration change that is not reflected here
    /// fails this suite by name instead of ageing quietly.
    const DB_TODOS_IR: &str = include_str!("../../../tests/fixtures/migrations-ir/db-todos.ir.json");

    /// The committed descriptor for the same app - the bytes the packer hashes.
    const DB_TODOS_RUNTIME_JSON: &str =
        include_str!("../../../examples/db-todos/generated/zeroship/schema.runtime.json");

    /// **The apply body is RECORDED from the creator's `.ts`, and it is exactly
    /// the pinned set - byte for byte.**
    ///
    /// This is what binds the recorder to the fold the build runs. If the two
    /// drift - a different serialisation, a dropped filename stem, ops in
    /// another order - the migration service journals different versions under
    /// different names, and nothing else in the tree would notice.
    ///
    /// It runs the REAL path: [`record_apply_request`] spawns the same Node
    /// recorder the CLI spawns, against a real app's real migrations, and that
    /// app carries no recorded set on disk for it to read instead. It needs a
    /// built `packages/vite-plugin/dist` (`pnpm build`); without one it fails
    /// naming what is missing, which is the correct outcome for a fixture that
    /// cannot reach its dependency.
    #[test]
    fn recording_db_todos_reproduces_the_posted_ir() {
        let app = repo_root().join("examples/db-todos");
        let migrations = app.join("migrations");
        assert!(
            migrations.is_dir(),
            "the db-todos example must still author migrations: {}",
            migrations.display()
        );
        // THE PRECONDITION, asserted rather than assumed: the recording cannot
        // be reading a file, because the app holds none for it to read.
        let on_disk = app.join("generated/zeroship/migrations.ir.json");
        assert!(
            !on_disk.exists(),
            "the recorded migration set must exist only on the wire: {}",
            on_disk.display()
        );

        let body = record_apply_request(&migrations).expect("record the db-todos migrations");
        assert_eq!(
            body, DB_TODOS_IR,
            "the recorded apply body must be the pinned bytes"
        );

        // The SECOND binding, and it is not a restatement of the first: the
        // fixture could be internally consistent and still name a descriptor no
        // build produces. This hashes the committed `schema.runtime.json` - the
        // same bytes `zship.ts` hashes into the manifest - and requires the
        // recorded body to declare it.
        let posted: serde_json::Value =
            serde_json::from_str(&body).expect("the apply body is JSON");
        assert_eq!(posted["kind"], "ir");
        assert_eq!(
            posted["descriptor_sha256"].as_str(),
            Some(zeroship_bundle::blob::sha256_hex(DB_TODOS_RUNTIME_JSON.as_bytes()).as_str()),
            "the recorded descriptor hash must name the committed schema.runtime.json"
        );
        assert_eq!(
            posted["documents"]
                .as_array()
                .expect("documents is an array")
                .len(),
            std::fs::read_dir(&migrations)
                .expect("read the migrations dir")
                .filter(|e| e.as_ref().is_ok_and(|e| e.path().extension().is_some_and(|x| x == "ts")))
                .count(),
            "every authored .ts migration must reach the wire"
        );
    }

    /// A recorded-migration FILE is not an input, and a positional naming one is
    /// refused rather than posted: its documents are whatever was in it, not
    /// what the app's `.ts` say today, so posting it would apply a schema
    /// nothing in the checkout derived.
    #[test]
    fn a_positional_file_is_refused_rather_than_posted() {
        let recorded = repo_root().join("tests/fixtures/migrations-ir/db-todos.ir.json");
        assert!(recorded.is_file(), "{}", recorded.display());
        let error = record_apply_request(&recorded).expect_err("a file must be refused");
        assert!(error.contains("is a file"), "{error}");
        assert!(error.contains("DIRECTORY"), "{error}");

        // The control, differing in one variable: the same call against a
        // DIRECTORY that does not exist fails for the other reason, so the
        // refusal above is about the file-ness and not about every bad path.
        let missing = repo_root().join("tests/fixtures/migrations-ir/does-not-exist");
        let error = record_apply_request(&missing).expect_err("a missing dir must be refused");
        assert!(error.contains("no migrations directory"), "{error}");
    }
}
