//! `zeroship db` - declare a database, and grant an app access to one.
//!
//! Shape:
//!   zeroship db create   <name> --project=prj_...
//!   zeroship db list     --project=prj_...
//!   zeroship db bind     <label> --app=<label> --capability=<capability>
//!   zeroship db unbind   <label> --app=<label>
//!   zeroship db bindings <label>
//!   zeroship db delete   <label>
//!
//! # A label never travels as an identifier
//!
//! `<label>` is the key of a `databases` entry in `zeroship.jsonc`, and
//! [`resolve_database`] dereferences it to the `dbs_` id THIS PROCESS sends,
//! before any request is planned. Two workspaces may both call a database
//! `main`; nothing on a wire can tell them apart, so nothing on a wire carries
//! the word. The same rule `--app` already follows
//! (`project_config::select_app`), told apart the same way: with a config file
//! present the argument is a label, with no file there are no labels and it is
//! an id. Never by inspecting the value - the control plane reserves the whole
//! `dbs_` prefix from database NAMES so that inspecting it could not work.
//!
//! # What this module deliberately does not know
//!
//! THE CAPABILITY LADDER. `--capability` is passed through as an opaque string
//! and `zeroship_control::databases::validate_capability` names the accepted
//! set in its refusal. A copy of that set here would be a second spelling that
//! goes stale the day the ladder moves, and it would turn a server-side
//! vocabulary change into a client that refuses valid input. `--role` in
//! `crate::organizations` is the same decision for the same reason.
//!
//! WHAT A NAME MAY BE. `create` sends the name as typed:
//! `zeroship_control::databases::validate_database_name` owns the charset, the
//! length, and the reservation of the `dbs_` namespace, and it tells the two
//! apart (`invalid request` vs `reserved name`). Re-deciding any of that here
//! would produce a refusal the server does not agree with.
//!
//! THE RESPONSE SHAPES. Reads print the server's JSON on stdout verbatim,
//! because a table built here from named fields silently drops whatever the
//! server adds.
//!
//! # Declaring a database grants nothing
//!
//! `databases` in `zeroship.jsonc` is a local name for an id. It is `bind` that
//! grants an app access, and deploy verifies the grant is live
//! (`zeroship_control::catalog::admit_bindings`), so an app declaring a
//! database it never bound is refused at deploy rather than at runtime.

use crate::project_config;
use crate::{flag_str, parse_flag, resolve_bearer_token, run_curl, ControlResponse};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use zeroship_core::project_id::ProjectId;
use zeroship_core::{AppId, DatabaseId};

/// Encode every ASCII delimiter plus `.` so a value stays exactly one path
/// segment. Every id this module puts in a path is parsed first, so nothing
/// reaching here should need escaping - which is the reason to escape anyway:
/// the day a new subcommand forgets the parse, the URL must still be the URL
/// the caller meant.
const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b'.')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Known flags. An unrecognised one would otherwise be ignored in silence and
/// the command would act on the default target instead of the one the creator
/// named - `secret`, `var` and `organization` each learned this the same way.
const KNOWN_FLAGS: &[&str] = &[
    "--project",
    "--app",
    "--capability",
    "--control",
    "--token",
    "--config",
    "--env",
    "--yes",
];

/// Every flag above that takes a VALUE, so the space form can be skipped when
/// counting positionals. `--yes` is the one that does not.
const VALUE_FLAGS: &[&str] = &[
    "--project",
    "--app",
    "--capability",
    "--control",
    "--token",
    "--config",
    "--env",
];

/// The subcommands, in the order [`usage`] prints them. One list, consulted by
/// the dispatcher and by the usage text, so a subcommand cannot exist without
/// being advertised or be advertised without existing.
///
/// `bindings` is here because `delete` and `bind` both refuse by naming edges:
/// delete says which apps still bind the database, and bind says the capability
/// the live edge already holds. A CLI that answers "unbind those first" and has
/// no way to list them would be a claim that reads as protection.
const SUBCOMMANDS: &[&str] = &["create", "list", "bind", "unbind", "bindings", "delete"];

/// Which subcommands name a DATABASE, and so need a label dereferenced.
///
/// `create` mints one and `list` spans a project, so neither has a database to
/// name yet.
fn needs_database(sub: &str) -> bool {
    !matches!(sub, "create" | "list")
}

// ---------------------------------------------------------------------------
// The planned call
// ---------------------------------------------------------------------------

/// One control-plane call: a method, a path under the control origin, and an
/// optional JSON body.
///
/// Separating this from sending it is what makes the routing testable without a
/// server: [`plan`] is a pure function of its arguments, so a test can state
/// the exact method, path and body a subcommand produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    pub(crate) method: &'static str,
    pub(crate) path: String,
    pub(crate) body: Option<String>,
}

impl Call {
    fn get(path: String) -> Self {
        Self {
            method: "GET",
            path,
            body: None,
        }
    }

    fn post(path: String, body: serde_json::Value) -> Self {
        Self {
            method: "POST",
            path,
            body: Some(body.to_string()),
        }
    }

    fn delete(path: String) -> Self {
        Self {
            method: "DELETE",
            path,
            body: None,
        }
    }
}

fn segment(value: &str) -> String {
    utf8_percent_encode(value, PATH_SEGMENT_ENCODE_SET).to_string()
}

/// The `n`th positional argument after `zeroship db`, counting the subcommand
/// as 0.
///
/// NOT an argv index: [`crate::parse_flag`] accepts `--app main` as well as
/// `--app=main`, so in the space form the value is an ordinary word in argv,
/// and a flag written anywhere shifts every later index. `organization`'s
/// `nth_positional` carries the same two scars.
fn nth_positional(args: &[String], n: usize) -> Option<&str> {
    let mut seen = 0usize;
    let mut iter = args.iter().skip(2); // past the binary and `db`
    while let Some(arg) = iter.next() {
        if VALUE_FLAGS.contains(&arg.as_str()) {
            iter.next(); // its value, which is not a positional
            continue;
        }
        if arg.starts_with("--") {
            continue; // `--flag=value`, or a valueless flag
        }
        if seen == n {
            return Some(arg.as_str());
        }
        seen += 1;
    }
    None
}

fn required_flag(args: &[String], flag: &'static str, what: &str) -> Result<String, String> {
    parse_flag(args, flag)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("missing {flag}=<{what}>"))
}

/// The project a `create` or `list` names.
///
/// There is no `zeroship.jsonc` key for it and no recorded selection: the file
/// names databases by id, and an id already carries its project. A project is
/// only needed where no database exists yet to carry one, which is exactly
/// these two subcommands.
fn require_project(args: &[String]) -> Result<String, String> {
    let raw = required_flag(args, "--project", "prj_...")?;
    ProjectId::parse(&raw)
        .map(|id| segment(id.as_str()))
        .map_err(|_| {
            format!(
                "--project={raw}: not a project id (it looks like `prj_<22 chars>`). \
                 `zeroship organization projects` lists yours."
            )
        })
}

/// The database one subcommand acts on: the `dbs_` id, and the label it came
/// from when a config file supplied one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DatabaseSelection {
    /// The local label, when a config file declared one. `None` only when there
    /// is no file, because labels exist nowhere else.
    pub(crate) label: Option<String>,
    pub(crate) id: DatabaseId,
}

/// Dereference the positional database argument to the id that will be sent.
///
/// **With a config file present the argument is a LABEL**, because the file is
/// the namespace the CLI resolves in: the id comes from the file, so a label
/// never travels as an identifier and a typo names the labels that exist. With
/// NO file there are no labels, so it is a `dbs_` id. The two cases are told
/// apart by whether there is a file, never by inspecting the value.
pub(crate) fn resolve_database(
    args: &[String],
    cfg: Option<&project_config::Resolved>,
) -> Result<DatabaseSelection, String> {
    let raw = nth_positional(args, 1).ok_or_else(|| {
        match cfg {
            Some(cfg) => format!(
                "missing <label>; {} declares {}. With a {} present the argument is one of \
                 its `databases` labels, which this command dereferences to the id it sends.",
                cfg.path.display(),
                join_or_none(&cfg.database_labels()),
                project_config::CONFIG_FILENAME
            ),
            None => format!(
                "missing <dbs_...>; there is no {} in this directory to read a label from, \
                 so the argument is a database id. `zeroship db list --project=prj_...` \
                 shows them.",
                project_config::CONFIG_FILENAME
            ),
        }
    })?;

    let Some(cfg) = cfg else {
        let id = DatabaseId::parse(raw).map_err(|_| {
            format!(
                "{raw:?} is not a database id (it looks like `dbs_<22 chars>`). There is no \
                 {} in this directory, so labels do not exist here.",
                project_config::CONFIG_FILENAME
            )
        })?;
        return Ok(DatabaseSelection { label: None, id });
    };

    let declared = cfg.database_labels();
    if !declared.contains(&raw) {
        return Err(format!(
            "{raw:?} names no database in {} (declared: {}).\n\
             With a {} present the argument names one of its labels: the id comes from the \
             file, so a label never travels as an identifier.",
            cfg.path.display(),
            join_or_none(&declared),
            project_config::CONFIG_FILENAME
        ));
    }
    let raw_id = cfg.database_id(raw)?;
    let id = DatabaseId::parse(raw_id).map_err(|_| {
        format!(
            "{} declares `databases.{raw}.id` as {raw_id:?}, which is not a database id \
             (it looks like `dbs_<22 chars>`)",
            cfg.path.display()
        )
    })?;
    Ok(DatabaseSelection {
        label: Some(raw.to_string()),
        id,
    })
}

/// The app a `bind` or `unbind` names, as the `app_` id that will be sent.
///
/// `--app` follows the same label rule as everywhere else in this CLI
/// ([`project_config::select_app`]); the id it yields is parsed here so a
/// malformed one in the file is named with the file, not with a 400 from the
/// control plane.
fn require_app(
    args: &[String],
    cfg: Option<&project_config::Resolved>,
) -> Result<AppId, String> {
    let selection = project_config::select_app(args, cfg)?;
    let sourced = selection.id.ok_or_else(|| {
        format!(
            "`apps.{}` carries no `app` id yet. A binding names an app that exists; \
             run `zeroship deploy` first.",
            selection.label.as_deref().unwrap_or("<none>")
        )
    })?;
    AppId::parse(&sourced.value).map_err(|_| {
        format!(
            "{:?} is not an app id (it looks like `app_<22 chars>`), from {}",
            sourced.value,
            sourced.source.describe()
        )
    })
}

/// Build the control-plane call for one subcommand.
///
/// `database` is `Some` exactly when [`needs_database`] said so, and is already
/// dereferenced. A subcommand that reads it when it is `None` is a routing bug,
/// not a user error, which is why the two are wired together rather than each
/// subcommand re-deciding.
pub(crate) fn plan(
    sub: &str,
    args: &[String],
    cfg: Option<&project_config::Resolved>,
    database: Option<&DatabaseId>,
) -> Result<Call, String> {
    let database_path = || {
        segment(
            database
                .expect("needs_database gates every arm that reads it")
                .as_str(),
        )
    };

    match sub {
        "create" => {
            let project = require_project(args)?;
            let name = nth_positional(args, 1).ok_or_else(|| {
                "missing <name>; e.g. `zeroship db create main --project=prj_...`. The name is \
                 how you recognise the database in a listing; the id it returns is what \
                 `zeroship.jsonc` records."
                    .to_string()
            })?;
            Ok(Call::post(
                format!("/api/projects/{project}/databases"),
                serde_json::json!({ "name": name }),
            ))
        }
        "list" => {
            let project = require_project(args)?;
            Ok(Call::get(format!("/api/projects/{project}/databases")))
        }
        "bindings" => Ok(Call::get(format!(
            "/api/databases/{}/bindings",
            database_path()
        ))),
        "bind" => {
            let app = require_app(args, cfg)?;
            let capability = required_flag(args, "--capability", "capability")?;
            Ok(Call::post(
                format!("/api/databases/{}/bindings", database_path()),
                serde_json::json!({ "app_id": app.as_str(), "capability": capability }),
            ))
        }
        "unbind" => {
            let app = require_app(args, cfg)?;
            Ok(Call::delete(format!(
                "/api/databases/{}/bindings/{}",
                database_path(),
                segment(app.as_str())
            )))
        }
        "delete" => Ok(Call::delete(format!("/api/databases/{}", database_path()))),
        other => Err(unknown_subcommand(other)),
    }
}

fn unknown_subcommand(sub: &str) -> String {
    if sub.is_empty() {
        format!("missing subcommand; one of {}", SUBCOMMANDS.join(", "))
    } else {
        format!(
            "unknown subcommand {sub:?}; one of {}",
            SUBCOMMANDS.join(", ")
        )
    }
}

fn join_or_none(names: &[&str]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// The seam the tests replace. [`plan`] decides WHAT to send; this sends it.
pub(crate) trait DatabaseTransport {
    fn send(
        &mut self,
        method: &str,
        url: &str,
        token: &str,
        body: Option<&str>,
    ) -> Result<ControlResponse, String>;
}

struct CurlTransport;

impl DatabaseTransport for CurlTransport {
    fn send(
        &mut self,
        method: &str,
        url: &str,
        token: &str,
        body: Option<&str>,
    ) -> Result<ControlResponse, String> {
        let auth = format!("Authorization: Bearer {token}");
        let mut command = std::process::Command::new("curl");
        command.args([
            "-s",
            "-w",
            "\n%{http_code}",
            "--connect-timeout",
            "10",
            "-X",
            method,
            url,
            "-H",
            &auth,
        ]);
        if body.is_some() {
            command.args([
                "-H",
                "Content-Type: application/json",
                "--data-binary",
                "@-",
            ]);
        }
        run_curl(&mut command, body.map(str::as_bytes))
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn cmd_db(args: &[String]) -> Result<(), String> {
    run(&mut CurlTransport, args)
}

pub(crate) fn run<T: DatabaseTransport>(transport: &mut T, args: &[String]) -> Result<(), String> {
    let sub = args.get(2).map(String::as_str).unwrap_or("");
    if !SUBCOMMANDS.contains(&sub) {
        eprintln!("{}", usage());
        return Err(unknown_subcommand(sub));
    }
    check_unknown_flags(args)?;

    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot read the working directory: {e}"))?;
    let file = project_config::locate(args, &cwd)?;
    let config = file
        .as_deref()
        .map(project_config::ProjectConfig::load)
        .transpose()?;
    let resolved = match (&config, flag_str(args, "--env=")) {
        (Some(cfg), env) => Some(cfg.resolve(env.as_deref())?),
        // NO IMPLICIT ENVIRONMENT: `--env=` with no file is a typo, not a
        // request, and ignoring it would act on the wrong target with the
        // creator believing otherwise.
        (None, Some(env)) => {
            return Err(format!(
                "--env={env} needs a {} in this directory to read the environment from",
                project_config::CONFIG_FILENAME
            ))
        }
        (None, None) => None,
    };

    let selection = if needs_database(sub) {
        Some(resolve_database(args, resolved.as_ref())?)
    } else {
        None
    };

    // A CORRECT command run against the wrong environment is the one failure a
    // provenance line cannot stop, and `delete` is the only subcommand here
    // whose effect an error message afterwards cannot undo: it drops the
    // schema and everything in it. `"protected": true` is the creator saying
    // this environment is the real one; `--yes` is them saying they meant it.
    // The same lever `zeroship migrate` uses, for the same reason.
    if sub == "delete"
        && resolved
            .as_ref()
            .is_some_and(project_config::Resolved::is_protected)
        && !args.iter().any(|a| a == "--yes")
    {
        return Err(format!(
            "the selected environment is marked \"protected\": true and this would delete \
             {} and every row in it. Re-run with --yes if that is what you meant.",
            describe(selection.as_ref())
        ));
    }

    let call = plan(sub, args, resolved.as_ref(), selection.as_ref().map(|s| &s.id))?;

    let control = project_config::resolve_control(args, resolved.as_ref())?;
    let token = resolve_bearer_token(args)?;

    project_config::print_provenance("db", &[("control", &control)]);
    // THE LABEL AND THE ID IT DEREFERENCED TO, side by side, before the call.
    // The label is local and the id is what travels; printing only one of them
    // is how a creator discovers afterwards that `main` meant a different
    // database in the environment they had selected.
    if let Some(selected) = &selection {
        eprintln!("zeroship db: database = {}", describe(Some(selected)));
    }

    let url = format!("{}{}", control.value.trim_end_matches('/'), call.path);
    let response = transport.send(call.method, &url, &token, call.body.as_deref())?;
    report(sub, selection.as_ref(), &response)
}

/// `label (dbs_...)` when a file supplied the label, the bare id otherwise.
fn describe(selection: Option<&DatabaseSelection>) -> String {
    match selection {
        Some(DatabaseSelection {
            label: Some(label),
            id,
        }) => format!("{label} ({})", id.as_str()),
        Some(DatabaseSelection { label: None, id }) => id.as_str().to_string(),
        None => "this database".to_string(),
    }
}

/// Turn one response into this process's output and exit status.
///
/// A `204` carries no body and every other success carries JSON; the body goes
/// to stdout verbatim so it can be piped, and every explanation goes to stderr
/// so piping stays clean.
fn report(
    sub: &str,
    selection: Option<&DatabaseSelection>,
    response: &ControlResponse,
) -> Result<(), String> {
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "{} failed ({}): {}",
            sub, response.status, response.body
        ));
    }
    if !response.body.trim().is_empty() {
        println!("{}", response.body);
    }
    match sub {
        "create" => note_created(&response.body),
        // Control declares and stops: the reconciler holding the cluster's
        // credential is what makes the cluster match, so an app deploying
        // against a binding that has not been granted yet is refused by
        // `admit_bindings` rather than failing at its first query.
        "bind" => eprintln!(
            "zeroship db: binding declared. It is `pending` until the cluster it names has \
             been granted the role; `zeroship db bindings` shows the status, and a deploy \
             naming this database is refused until it is active."
        ),
        "unbind" => eprintln!(
            "zeroship db: binding revoked. The app keeps its code and loses its access; a \
             deploy that still names this database is refused until you bind it again."
        ),
        "delete" => eprintln!(
            "zeroship db: {} is marked for deletion. Remove its entry from `databases` in {} \
             and the label from every app that named it.",
            describe(selection),
            project_config::CONFIG_FILENAME
        ),
        _ => {}
    }
    Ok(())
}

/// After minting a database, say the one thing the creator has to do next.
///
/// The id is the whole product of this command: `zeroship.jsonc` records it
/// under a label, and nothing else can reach the database by the name that was
/// just typed - names are display text, resolved nowhere.
fn note_created(body: &str) {
    let id = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    match id {
        Some(id) => eprintln!(
            "zeroship db: created {id}. Record it in {} as \
             `databases.<label>.id`, name that label in the app's `databases`, then \
             `zeroship db bind <label> --app=<label> --capability=<capability>`. \
             Declaring it grants nothing: deploy verifies the binding.",
            project_config::CONFIG_FILENAME
        ),
        None => eprintln!(
            "zeroship db: created, but the response carried no `id`. Run \
             `zeroship db list --project=prj_...` to read it; the id is what \
             {} records, never the name.",
            project_config::CONFIG_FILENAME
        ),
    }
}

fn check_unknown_flags(args: &[String]) -> Result<(), String> {
    for arg in args.iter().skip(3) {
        if !arg.starts_with("--") {
            continue;
        }
        let name = match arg.find('=') {
            Some(i) => &arg[..i],
            None => arg.as_str(),
        };
        if !KNOWN_FLAGS.contains(&name) {
            return Err(format!(
                "unknown flag `{name}`; it would have been ignored in silence. \
                 Known flags: {}.",
                KNOWN_FLAGS.join(" ")
            ));
        }
    }
    Ok(())
}

fn usage() -> String {
    concat!(
        "Usage:\n",
        "  zeroship db create   <name> --project=prj_...\n",
        "  zeroship db list     --project=prj_...\n",
        "  zeroship db bind     <label> --app=<label> --capability=<capability>\n",
        "  zeroship db unbind   <label> --app=<label>\n",
        "  zeroship db bindings <label>\n",
        "  zeroship db delete   <label> [--yes]\n",
        "\n",
        "<label> is a key of `databases` in zeroship.jsonc, dereferenced to its `dbs_` id\n",
        "before any request; with no file in this directory it is the id itself. `--app`\n",
        "follows the same rule against `apps`.\n",
        "\n",
        "A database belongs to a project and outlives the apps that use it. Declaring one\n",
        "in zeroship.jsonc grants nothing: `bind` grants access, and a deploy naming a\n",
        "database the app holds no live binding to is refused.\n",
        "\n",
        "Create and bind DECLARE. The cluster is made to match by the reconciler holding\n",
        "its credential, so a fresh database is `provisioning` and a fresh binding is\n",
        "`pending` until then.\n",
        "\n",
        "`delete` is refused while any app still binds the database, and the refusal names\n",
        "them; it needs --yes when the selected environment is `\"protected\": true`."
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(rest: &[&str]) -> Vec<String> {
        std::iter::once("zeroship".to_string())
            .chain(std::iter::once("db".to_string()))
            .chain(rest.iter().map(|s| (*s).to_string()))
            .collect()
    }

    const PRJ: &str = "prj_0000000002e4nenowz3qmamtd";
    const DBS: &str = "dbs_0000000002e4nenowz3qmamtd";
    const APP: &str = "app_0000000002e4nenowz3qmamtd";

    fn database(id: &str) -> DatabaseId {
        DatabaseId::parse(id).expect("fixture id")
    }

    /// The routing table, stated as data. Every subcommand appears, and the
    /// assertion is on the METHOD, PATH and BODY - the three things a mistake
    /// here sends to the wrong place.
    #[test]
    fn every_subcommand_targets_its_route() {
        let cases: &[(&[&str], &str, String, Option<String>)] = &[
            (
                &["create", "main", "--project", PRJ],
                "POST",
                format!("/api/projects/{PRJ}/databases"),
                Some(r#"{"name":"main"}"#.to_string()),
            ),
            (
                &["list", "--project", PRJ],
                "GET",
                format!("/api/projects/{PRJ}/databases"),
                None,
            ),
            (
                &["bind", DBS, "--app", APP, "--capability", "readwrite"],
                "POST",
                format!("/api/databases/{DBS}/bindings"),
                Some(format!(
                    r#"{{"app_id":"{APP}","capability":"readwrite"}}"#
                )),
            ),
            (
                &["unbind", DBS, "--app", APP],
                "DELETE",
                format!("/api/databases/{DBS}/bindings/{APP}"),
                None,
            ),
            (
                &["bindings", DBS],
                "GET",
                format!("/api/databases/{DBS}/bindings"),
                None,
            ),
            (
                &["delete", DBS],
                "DELETE",
                format!("/api/databases/{DBS}"),
                None,
            ),
        ];
        // Every subcommand the dispatcher accepts is exercised, so a new one
        // cannot be added without a route assertion.
        let covered: Vec<&str> = cases.iter().map(|(argv, ..)| argv[0]).collect();
        assert_eq!(
            covered, SUBCOMMANDS,
            "every subcommand must appear in the routing table, in order"
        );
        for (rest, method, path, body) in cases {
            let args = argv(rest);
            let id = database(DBS);
            let selected = needs_database(rest[0]).then_some(&id);
            let call = plan(rest[0], &args, None, selected)
                .unwrap_or_else(|e| panic!("plan {rest:?}: {e}"));
            assert_eq!(&call.method, method, "method for {rest:?}");
            assert_eq!(&call.path, path, "path for {rest:?}");
            assert_eq!(call.body.as_deref(), body.as_deref(), "body for {rest:?}");
        }
    }

    /// The capability travels as typed. The accepted set lives in
    /// `zeroship_control::databases::validate_capability`, and a copy here
    /// would refuse a value the server accepts the day the ladder moves.
    #[test]
    fn an_unknown_capability_is_sent_rather_than_refused_locally() {
        let args = argv(&["bind", DBS, "--app", APP, "--capability", "appendonly"]);
        let id = database(DBS);
        let call = plan("bind", &args, None, Some(&id)).expect("plan");
        assert_eq!(
            call.body.as_deref(),
            Some(format!(r#"{{"app_id":"{APP}","capability":"appendonly"}}"#).as_str())
        );
    }

    #[test]
    fn bind_without_a_capability_names_the_flag() {
        let args = argv(&["bind", DBS, "--app", APP]);
        let id = database(DBS);
        let error = plan("bind", &args, None, Some(&id)).expect_err("must refuse");
        assert_eq!(error, "missing --capability=<capability>");
    }

    /// A malformed project id is caught HERE, with the flag named. Sent as
    /// typed it would come back as a 404 naming a project that never existed.
    #[test]
    fn a_malformed_project_is_refused_before_the_request() {
        let args = argv(&["list", "--project", "proj_not_an_id"]);
        let error = plan("list", &args, None, None).expect_err("must refuse");
        assert!(
            error.contains("not a project id"),
            "expected a project-id refusal, got {error:?}"
        );
    }

    /// `--app app_x` in the SPACE form puts the id in argv as an ordinary
    /// word. A positional reader that takes the first non-`--` argument reads
    /// it as the database, which is the bug `organization` hit twice.
    #[test]
    fn a_space_form_flag_value_is_not_read_as_the_positional() {
        let args = argv(&["bind", "--app", APP, "main", "--capability", "readwrite"]);
        assert_eq!(nth_positional(&args, 1), Some("main"));
    }

    #[test]
    fn an_unknown_flag_is_refused_rather_than_ignored() {
        let args = argv(&["bind", DBS, "--app", APP, "--readwrite"]);
        let error = check_unknown_flags(&args).expect_err("must refuse");
        assert!(
            error.contains("--readwrite"),
            "the refusal must name the flag, got {error:?}"
        );
    }

    // -----------------------------------------------------------------
    // The label dereference
    // -----------------------------------------------------------------

    /// The shared cross-tool fixture: two databases, two apps, and a `staging`
    /// environment that overrides the ID under each label and nothing else.
    fn fixture(environment: Option<&str>) -> project_config::Resolved {
        let text = include_str!("../../../tests/fixtures/project-config/zeroship.jsonc");
        project_config::ProjectConfig::parse(
            std::path::PathBuf::from("zeroship.jsonc"),
            text.to_string(),
        )
        .expect("the committed fixture must parse")
        .resolve(environment)
        .expect("the committed fixture must resolve")
    }

    /// THE WHOLE POINT OF A LOCAL LABEL. `main` is a word two workspaces may
    /// both choose, so it is dereferenced to the `dbs_` the file declares
    /// before a path is built, and the word itself never appears on the wire.
    #[test]
    fn a_label_is_dereferenced_to_its_id_and_never_travels() {
        let cfg = fixture(None);
        let args = argv(&["delete", "main"]);
        let selected = resolve_database(&args, Some(&cfg)).expect("dereference main");
        assert_eq!(selected.label.as_deref(), Some("main"));
        assert_eq!(selected.id.as_str(), "dbs_03evr3oqx1200yyd6zj2cebfw");

        let call = plan("delete", &args, Some(&cfg), Some(&selected.id)).expect("plan");
        assert_eq!(call.path, "/api/databases/dbs_03evr3oqx1200yyd6zj2cebfw");
        assert!(
            !call.path.contains("main"),
            "the label must not reach the wire: {}",
            call.path
        );
    }

    /// The SAME label under `--env=staging` is a DIFFERENT database. An
    /// environment overrides the id under a label, never the label, so a
    /// command that carried the word would act on whichever database the
    /// reader happened to resolve last.
    #[test]
    fn an_environment_moves_the_id_under_the_same_label() {
        let args = argv(&["bindings", "main"]);
        let root = resolve_database(&args, Some(&fixture(None))).expect("root");
        let staging = resolve_database(&args, Some(&fixture(Some("staging")))).expect("staging");
        assert_eq!(root.label, staging.label, "the label is the same word");
        assert_ne!(
            root.id.as_str(),
            staging.id.as_str(),
            "the environment must move the id under it"
        );
        assert_eq!(staging.id.as_str(), "dbs_03evr3oqx1200uzh8k6gycpgg");
        let call = plan("bindings", &args, Some(&fixture(Some("staging"))), Some(&staging.id))
            .expect("plan");
        assert_eq!(
            call.path,
            "/api/databases/dbs_03evr3oqx1200uzh8k6gycpgg/bindings"
        );
    }

    /// With a file present the argument names one of its labels, and a typo is
    /// answered with the labels that exist rather than sent as an id.
    #[test]
    fn an_undeclared_label_names_the_declared_ones() {
        let cfg = fixture(None);
        let args = argv(&["delete", "reporting"]);
        let error = resolve_database(&args, Some(&cfg)).expect_err("must refuse");
        assert!(
            error.contains("names no database") && error.contains("main, analytics"),
            "the refusal must list the declared labels, got {error:?}"
        );
    }

    /// A `dbs_` id passed where a file declares labels is refused, not sent.
    /// The control plane reserves the whole `dbs_` prefix from database NAMES
    /// so that telling the two apart by inspecting the value could not work;
    /// the rule is the presence of a file, and it has to hold in both
    /// directions or it is not a rule.
    #[test]
    fn an_id_is_refused_where_a_file_declares_labels() {
        let cfg = fixture(None);
        let args = argv(&["delete", DBS]);
        let error = resolve_database(&args, Some(&cfg)).expect_err("must refuse");
        assert!(
            error.contains("names no database"),
            "expected the label refusal, got {error:?}"
        );
    }

    /// With NO file there are no labels, so the argument is an id - and a
    /// label passed there is refused naming the absent file rather than sent
    /// as if it were one.
    #[test]
    fn without_a_file_the_argument_is_an_id() {
        let args = argv(&["delete", DBS]);
        let selected = resolve_database(&args, None).expect("parse the id");
        assert_eq!(selected.label, None);
        assert_eq!(selected.id.as_str(), DBS);

        let labelled = argv(&["delete", "main"]);
        let error = resolve_database(&labelled, None).expect_err("must refuse");
        assert!(
            error.contains("not a database id"),
            "expected an id refusal, got {error:?}"
        );
    }

    /// A workspace whose app entries carry TYPED ids, which the shared
    /// cross-tool fixture does not: that file exists to exercise JSONC parsing
    /// quirks and still spells its app ids as bare UUIDs.
    const TYPED: &str = r#"{
  "$schema": "https://zeroship.ai/schema/project-v1.json",
  "name": "typed-ids",
  "control": "https://control.zeroship.ai",
  "runtime_date": "2026-08-14",
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "databases": {
    "main": { "id": "dbs_03evr3oqx1200yyd6zj2cebfw", "migrations": "migrations", "out": "generated/zeroship/main" },
    "analytics": { "id": "dbs_03evr3oqx1200qyvgmdnjrsla", "migrations": "migrations/analytics", "out": "generated/zeroship/analytics" }
  },
  "apps": {
    "storefront": { "app": "app_034klb07lrb9jgma6imvmx000", "databases": ["main", "analytics"], "primary": "main" }
  },
  "secrets": []
}"#;

    fn typed() -> project_config::Resolved {
        project_config::ProjectConfig::parse(
            std::path::PathBuf::from("zeroship.jsonc"),
            TYPED.to_string(),
        )
        .expect("fixture must parse")
        .resolve(None)
        .expect("fixture must resolve")
    }

    /// BOTH ends of a bind are dereferenced locally: the database label off
    /// `databases` and the app label off `apps`. Neither word reaches the URL
    /// or the body.
    #[test]
    fn bind_dereferences_both_labels_before_it_plans() {
        let cfg = typed();
        let args = argv(&["bind", "analytics", "--app=storefront", "--capability=readonly"]);
        let selected = resolve_database(&args, Some(&cfg)).expect("dereference analytics");
        let call = plan("bind", &args, Some(&cfg), Some(&selected.id)).expect("plan");
        assert_eq!(
            call.path,
            "/api/databases/dbs_03evr3oqx1200qyvgmdnjrsla/bindings"
        );
        assert_eq!(
            call.body.as_deref(),
            Some(r#"{"app_id":"app_034klb07lrb9jgma6imvmx000","capability":"readonly"}"#)
        );
        for word in ["analytics", "storefront"] {
            assert!(
                !call.path.contains(word) && !call.body.as_deref().unwrap_or("").contains(word),
                "the label {word:?} must not reach the wire"
            );
        }
    }

    /// A `--app` naming no declared label is refused with the labels that
    /// exist, rather than sent as an app id.
    #[test]
    fn bind_refuses_an_app_label_the_file_does_not_declare() {
        let cfg = typed();
        let args = argv(&["bind", "main", "--app=admin", "--capability=readwrite"]);
        let id = database(DBS);
        let error = plan("bind", &args, Some(&cfg), Some(&id)).expect_err("must refuse");
        assert!(
            error.contains("names no app") && error.contains("storefront"),
            "the refusal must list the declared app labels, got {error:?}"
        );
    }

    /// The usage text and the dispatcher read the same list, so a subcommand
    /// cannot exist without being advertised.
    #[test]
    fn every_subcommand_is_advertised() {
        let usage = usage();
        for sub in SUBCOMMANDS {
            assert!(
                usage.contains(&format!("zeroship db {sub}")),
                "{sub} is dispatchable but not in the usage text"
            );
        }
    }
}
