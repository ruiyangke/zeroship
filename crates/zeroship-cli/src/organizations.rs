//! `zeroship organization` - the ownership root a creator's projects and apps
//! hang from, and the party that is billed.
//!
//! Shape:
//!   zeroship organization create   <name> [--slug=SLUG] [--billing-email=ADDR]
//!   zeroship organization list
//!   zeroship organization show     [--organization=org_...]
//!   zeroship organization use      <org_...> | --clear
//!   zeroship organization members  [--organization=org_...]
//!   zeroship organization invite   <email> --role=ROLE [--organization=org_...]
//!   zeroship organization revoke   <ivt_...> [--organization=org_...]
//!   zeroship organization join     <token>
//!   zeroship organization role     <user-uuid> --role=ROLE [--organization=org_...]
//!   zeroship organization remove   <user-uuid> [--organization=org_...]
//!   zeroship organization leave    [--organization=org_...]
//!   zeroship organization transfer <user-uuid> [--organization=org_...]
//!   zeroship organization dissolve [--organization=org_...]
//!   zeroship organization projects [<verb> ...] [--organization=org_...]
//!
//! THE WORD IS SPELLED OUT AND THERE IS NO ALIAS. `org` appears only inside the
//! opaque id value; a command named `org` would make the abbreviation a word a
//! creator types, which is the tax the naming decision exists to refuse.
//!
//! # Why `use` exists
//!
//! Every other subcommand here needs to know WHICH organization. Threading
//! `--organization=` through all of them makes the common case - a creator with
//! one organization - pay for the uncommon one. `use` records a selection in
//! this CLI's own state directory (beside the credentials `zeroship login`
//! writes), so the flag is needed only to override it. The precedence is
//! `--organization=` flag, then the selection, and nothing else: there is no
//! environment variable and no `zeroship.jsonc` key, because an organization is
//! an account-level fact about the person running the command, not a property
//! of the checkout they are standing in.
//!
//! The resolved value and where it came from are printed before every call, so
//! a stale selection is visible rather than silent.
//!
//! # What this module deliberately does not know
//!
//! THE LADDER. `--role` is passed through as an opaque string. The roles are
//! rows in `zeroship.organization_roles`, seeded by a migration, and the
//! control plane already answers an unknown one with a `400` that NAMES the
//! whole ladder (`classify_seat_refusal`). A copy of the role list here would
//! be a second spelling that goes stale the day the ladder moves, and it would
//! turn a server-side vocabulary change into a client that refuses valid input.
//!
//! THE RESPONSE SHAPES. Reads print the server's JSON on stdout verbatim. This
//! CLI does not re-render control-plane records, for the same reason: a table
//! built here from named fields silently drops whatever the server adds.

use crate::project_config;
use crate::{flag_str, parse_flag, resolve_bearer_token, run_curl, ControlResponse};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use std::path::PathBuf;
use zeroship_core::invite_id::InviteId;
use zeroship_core::organization_id::OrganizationId;
use zeroship_core::project_id::ProjectId;

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

/// Known flags. An unrecognised `--organisation=` (the other spelling) would
/// otherwise be ignored in silence and the command would act on the SELECTED
/// organization instead of the one the creator named - a membership change
/// landing on the wrong company. `secret`/`var` learned this the same way.
const KNOWN_FLAGS: &[&str] = &[
    "--organization",
    "--control",
    "--token",
    "--config",
    "--env",
    "--role",
    "--slug",
    "--name",
    "--billing-email",
    "--clear",
];

/// The subcommands, in the order `usage` prints them. One list, consulted by
/// the dispatcher and by the usage text, so a subcommand cannot exist without
/// being advertised or be advertised without existing.
///
/// `revoke` is here although nothing asked for it, and the reason is that
/// `invite` PRINTS a remedy: the token is shown once, and the note beside it
/// says a lost one is revoked and re-issued. A CLI that says that and then has
/// no way to revoke would be a claim that reads as protection.
/// `leave` and `dissolve` are the two ends of the lifecycle and they are
/// deliberately different words. `leave` gives up the caller's OWN seat and
/// needs no rank at all; `dissolve` closes the organization for everyone and is
/// reserved to its owners. A single `remove --self` spelling would have made
/// the more dangerous of the two reachable by a typo.
const SUBCOMMANDS: &[&str] = &[
    "create", "list", "show", "use", "members", "invite", "revoke", "join", "role", "remove",
    "leave", "transfer", "dissolve", "projects",
];

// ---------------------------------------------------------------------------
// The planned call
// ---------------------------------------------------------------------------

/// One control-plane call: a method, a path under the control origin, and an
/// optional JSON body.
///
/// Separating this from sending it is what makes the routing testable without a
/// server: `plan` is a pure function of the arguments, so a test can state the
/// exact method, path and body a subcommand produces.
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

    fn patch(path: String, body: serde_json::Value) -> Self {
        Self {
            method: "PATCH",
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

/// Which subcommands act ON an organization, and so need one resolved.
///
/// `create` mints one, `list` spans them, `join` is told which by the token,
/// and `use` only records a name. Everything else needs a target.
fn needs_organization(sub: &str) -> bool {
    !matches!(sub, "create" | "list" | "join" | "use")
}

/// Every flag here that takes a VALUE, so the space form can be skipped over.
///
/// `--clear` is the one flag that does not, which is why this is a list and not
/// `KNOWN_FLAGS`.
const VALUE_FLAGS: &[&str] = &[
    "--organization",
    "--control",
    "--token",
    "--config",
    "--env",
    "--role",
    "--slug",
    "--name",
    "--billing-email",
];

/// The `n`th positional argument after `zeroship organization`, counting the
/// subcommand as 0.
///
/// TWO THINGS MAKE THIS NOT AN ARGV INDEX, and both were found by a test rather
/// than by reading:
///
/// - `crate::parse_flag` accepts `--organization org_x` as well as
///   `--organization=org_x`, so in the space form THE ID IS AN ORDINARY WORD in
///   argv. A naive "first argument not starting with `--`" claims it: the first
///   version of this returned `org_x` as the email for
///   `organization invite --organization org_x a@b.test --role=viewer`, and the
///   server refused the invitation with a complaint about the ADDRESS - a
///   message pointing at the one value the creator typed correctly.
/// - a flag written anywhere shifts every later argv index, so counting
///   POSITIONS ("the second word of `projects create <name>`") cannot be done by
///   adding one to a slot number. The second version did exactly that and read
///   the organization id as a project name.
fn nth_positional(args: &[String], n: usize) -> Option<&str> {
    let mut seen = 0usize;
    let mut iter = args.iter().skip(2); // past the binary and `organization`
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

fn require_user_id(raw: Option<&str>, example: &str) -> Result<String, String> {
    let Some(raw) = raw else {
        return Err(format!("missing <user-id>; e.g. `{example}`"));
    };
    // A USER id, not an app id, so this parses uuid directly rather than
    // borrowing `app_id_or_refuse`: that helper also accepts `app_<base62>`,
    // which is a different entity and would be accepted here for no reason.
    // `zeroship.users.id` is a uuid column, so uuid is the whole vocabulary.
    if raw.parse::<uuid::Uuid>().is_err() {
        return Err(format!(
            "{raw:?} is not a user id. A user id is a UUID; \
             `zeroship organization members` lists the ones seated here."
        ));
    }
    Ok(raw.to_string())
}

/// Build the control-plane call for one subcommand.
///
/// `organization` is `Some` exactly when [`needs_organization`] said so, and is
/// already a parsed [`OrganizationId`]. A subcommand that reads it when it is
/// `None` is a routing bug, not a user error, which is why the two are wired
/// together rather than each subcommand re-deciding.
pub(crate) fn plan(sub: &str, args: &[String], organization: Option<&str>) -> Result<Call, String> {
    // Positionals start after `zeroship organization <sub>`.
    let first = nth_positional(args, 1);
    let organization_path = || {
        organization
            .map(segment)
            .expect("needs_organization gates every arm that reads it")
    };

    match sub {
        "create" => {
            let name = first.ok_or_else(|| {
                "missing <name>; e.g. `zeroship organization create \"Acme\"`".to_string()
            })?;
            let mut body = serde_json::Map::new();
            body.insert("name".into(), name.into());
            if let Some(slug) = parse_flag(args, "--slug") {
                body.insert("slug".into(), slug.into());
            }
            if let Some(email) = parse_flag(args, "--billing-email") {
                body.insert("billing_email".into(), email.into());
            }
            Ok(Call::post(
                "/api/organizations".to_string(),
                serde_json::Value::Object(body),
            ))
        }
        "list" => Ok(Call::get("/api/organizations".to_string())),
        "show" => Ok(Call::get(format!(
            "/api/organizations/{}",
            organization_path()
        ))),
        "members" => Ok(Call::get(format!(
            "/api/organizations/{}/members",
            organization_path()
        ))),
        "invite" => {
            let email = first.ok_or_else(|| {
                "missing <email>; e.g. `zeroship organization invite you@example.com \
                 --role=developer`"
                    .to_string()
            })?;
            let role = required_flag(args, "--role", "role")?;
            Ok(Call::post(
                format!("/api/organizations/{}/invites", organization_path()),
                serde_json::json!({ "email": email, "role": role }),
            ))
        }
        "revoke" => {
            let raw = first.ok_or_else(|| {
                "missing <invite-id>; `zeroship organization invite` prints it as `id`, and \
                 nothing else can reach an invitation once its token is spent or lost"
                    .to_string()
            })?;
            let invite = InviteId::parse(raw).map_err(|_| {
                format!("{raw:?} is not an invitation id (it looks like `ivt_<22 chars>`)")
            })?;
            Ok(Call::delete(format!(
                "/api/organizations/{}/invites/{}",
                organization_path(),
                segment(invite.as_str())
            )))
        }
        "join" => {
            let token = first.ok_or_else(|| {
                "missing <token>; paste the value from the invitation you were sent".to_string()
            })?;
            Ok(Call::post(
                "/api/organization-invites/redeem".to_string(),
                serde_json::json!({ "token": token }),
            ))
        }
        "role" => {
            let user = require_user_id(first, "zeroship organization role <user-id> --role=admin")?;
            let role = required_flag(args, "--role", "role")?;
            Ok(Call::patch(
                format!(
                    "/api/organizations/{}/members/{}",
                    organization_path(),
                    segment(&user)
                ),
                serde_json::json!({ "role": role }),
            ))
        }
        "remove" => {
            let user = require_user_id(first, "zeroship organization remove <user-id>")?;
            Ok(Call::delete(format!(
                "/api/organizations/{}/members/{}",
                organization_path(),
                segment(&user)
            )))
        }
        // No positional at all, and that is the shape of the route: nothing in
        // this path names a user, so the seat it can reach is the caller's own
        // by construction rather than by a check the server has to make.
        "leave" => Ok(Call::delete(format!(
            "/api/organizations/{}/membership",
            organization_path()
        ))),
        "dissolve" => Ok(Call::delete(format!(
            "/api/organizations/{}",
            organization_path()
        ))),
        "transfer" => {
            let user = require_user_id(first, "zeroship organization transfer <user-id>")?;
            Ok(Call::post(
                format!("/api/organizations/{}/transfer", organization_path()),
                serde_json::json!({ "user_id": user }),
            ))
        }
        "projects" => plan_projects(args, &organization_path(), first),
        other => Err(unknown_subcommand(other)),
    }
}

/// Every `projects` verb, in the order `usage` prints them. One list, so a verb
/// cannot exist without being advertised or be advertised without existing -
/// the same rule [`SUBCOMMANDS`] holds one level up.
///
/// A bare `zeroship organization projects` (no verb) LISTS, which is why the
/// empty string is not in here: it is the default rather than a verb.
const PROJECT_VERBS: &[&str] = &[
    "create", "rename", "delete", "members", "add", "role", "remove",
];

/// Parse the `prj_...` a project verb acts on.
///
/// Every verb below except `create` takes one, and it is parsed HERE rather
/// than sent as typed: a slug where an id belongs resolves to no membership at
/// all, so the server can only answer it with a refusal that names the wrong
/// thing.
fn require_project_id(raw: Option<&str>, verb: &str) -> Result<String, String> {
    let Some(raw) = raw else {
        return Err(format!(
            "missing <prj_...>; e.g. `zeroship organization projects {verb} prj_...`. \
             `zeroship organization projects` lists them."
        ));
    };
    ProjectId::parse(raw)
        .map(|id| segment(id.as_str()))
        .map_err(|_| format!("{raw:?} is not a project id (it looks like `prj_<22 chars>`)"))
}

fn plan_projects(args: &[String], organization: &str, first: Option<&str>) -> Result<Call, String> {
    // The verb's own argument, which is the SECOND positional of
    // `zeroship organization projects <verb> <arg>`.
    let target = nth_positional(args, 2);
    match first {
        None => Ok(Call::get(format!(
            "/api/organizations/{organization}/projects"
        ))),
        Some("create") => {
            let name = target.ok_or_else(|| {
                "missing <name>; e.g. `zeroship organization projects create \"Checkout\"`"
                    .to_string()
            })?;
            let mut body = serde_json::Map::new();
            body.insert("name".into(), name.into());
            if let Some(slug) = parse_flag(args, "--slug") {
                body.insert("slug".into(), slug.into());
            }
            Ok(Call::post(
                format!("/api/organizations/{organization}/projects"),
                serde_json::Value::Object(body),
            ))
        }
        Some("rename") => {
            let project = require_project_id(target, "rename")?;
            let mut body = serde_json::Map::new();
            if let Some(name) = parse_flag(args, "--name") {
                body.insert("name".into(), name.into());
            }
            if let Some(slug) = parse_flag(args, "--slug") {
                body.insert("slug".into(), slug.into());
            }
            if body.is_empty() {
                return Err(
                    "nothing to change; pass --name=<name> and/or --slug=<slug>".to_string()
                );
            }
            Ok(Call::patch(
                format!("/api/projects/{project}"),
                serde_json::Value::Object(body),
            ))
        }
        Some("delete") => {
            let project = require_project_id(target, "delete")?;
            Ok(Call::delete(format!("/api/projects/{project}")))
        }
        Some("members") => {
            let project = require_project_id(target, "members")?;
            Ok(Call::get(format!("/api/projects/{project}/members")))
        }
        Some("add") => {
            let project = require_project_id(target, "add")?;
            let user = require_user_id(
                nth_positional(args, 3),
                "zeroship organization projects add prj_... <user-id> --role=developer",
            )?;
            let role = required_flag(args, "--role", "role")?;
            Ok(Call::post(
                format!("/api/projects/{project}/members"),
                serde_json::json!({ "user_id": user, "role": role }),
            ))
        }
        Some("role") => {
            let project = require_project_id(target, "role")?;
            let user = require_user_id(
                nth_positional(args, 3),
                "zeroship organization projects role prj_... <user-id> --role=viewer",
            )?;
            let role = required_flag(args, "--role", "role")?;
            Ok(Call::patch(
                format!("/api/projects/{project}/members/{}", segment(&user)),
                serde_json::json!({ "role": role }),
            ))
        }
        Some("remove") => {
            let project = require_project_id(target, "remove")?;
            let user = require_user_id(
                nth_positional(args, 3),
                "zeroship organization projects remove prj_... <user-id>",
            )?;
            Ok(Call::delete(format!(
                "/api/projects/{project}/members/{}",
                segment(&user)
            )))
        }
        Some(other) => Err(format!(
            "unknown `projects` verb {other:?}; \
             `zeroship organization projects` lists them, and the verbs are {}",
            PROJECT_VERBS.join(", ")
        )),
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

// ---------------------------------------------------------------------------
// The selection `use` records
// ---------------------------------------------------------------------------

/// Where the selected organization is remembered.
///
/// Beside `token.json`, in the directory [`crate::auth::state_dir`] resolves,
/// so `ZEROSHIP_CONFIG_HOME` relocates the credentials and the selection
/// together. A selection pointing at an organization the current credentials
/// cannot reach is not a state this file can prevent; the server answers `403`
/// and the provenance line printed before the call says which selection asked.
fn selection_path() -> Result<PathBuf, String> {
    Ok(crate::auth::state_dir()?.join("organization.json"))
}

fn read_selection() -> Result<Option<String>, String> {
    let path = selection_path()?;
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let Some(raw) = json.get("organization").and_then(serde_json::Value::as_str) else {
        return Err(format!(
            "{} has no `organization`; re-run `zeroship organization use <org_...>`",
            path.display()
        ));
    };
    // Parse on the way OUT as well as in. The file is editable by hand, and a
    // malformed id would otherwise travel as far as a 400 from the control
    // plane, naming the id and not the file that supplied it.
    let id = OrganizationId::parse(raw).map_err(|_| {
        format!(
            "{} names {raw:?}, which is not an organization id; \
             re-run `zeroship organization use <org_...>`",
            path.display()
        )
    })?;
    Ok(Some(id.as_str().to_string()))
}

fn write_selection(id: &str) -> Result<PathBuf, String> {
    let path = selection_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid selection path {}", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    let body = serde_json::json!({ "organization": id }).to_string();
    std::fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

fn clear_selection() -> Result<PathBuf, String> {
    let path = selection_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(e) => Err(format!("delete {}: {e}", path.display())),
    }
}

fn cmd_use(args: &[String]) -> Result<(), String> {
    if args.iter().any(|a| a == "--clear") {
        let path = clear_selection()?;
        eprintln!(
            "zeroship organization: selection cleared ({}); \
             pass --organization=<org_...> until you select one again",
            path.display()
        );
        return Ok(());
    }
    let raw = nth_positional(args, 1).ok_or_else(|| {
        "missing <org_...>; `zeroship organization list` shows the ones you belong to, \
         and `--clear` forgets the current selection"
            .to_string()
    })?;
    let id = OrganizationId::parse(raw).map_err(|_| {
        format!("{raw:?} is not an organization id (it looks like `org_<22 chars>`)")
    })?;
    let path = write_selection(id.as_str())?;
    eprintln!(
        "zeroship organization: selected {} ({})",
        id.as_str(),
        path.display()
    );
    Ok(())
}

/// Resolve which organization to act on: the flag, else the recorded selection.
///
/// Returns the id and the human description of where it came from, because a
/// command that acts on the wrong company because of a selection made days ago
/// must say so before it acts, not after.
fn resolve_organization(args: &[String]) -> Result<(String, String), String> {
    if let Some(raw) = parse_flag(args, "--organization") {
        let id = OrganizationId::parse(&raw).map_err(|_| {
            format!("--organization={raw}: not an organization id (it looks like `org_<22 chars>`)")
        })?;
        return Ok((id.as_str().to_string(), "--organization flag".to_string()));
    }
    let path = selection_path()?;
    match read_selection()? {
        Some(id) => Ok((id, path.display().to_string())),
        None => Err(format!(
            "no organization selected. Run `zeroship organization list` to see yours, \
             then `zeroship organization use <org_...>`, or pass --organization=<org_...>. \
             (The selection is recorded in {}.)",
            path.display()
        )),
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// The seam the tests replace. `plan` decides WHAT to send; this sends it.
pub(crate) trait OrganizationTransport {
    fn send(
        &mut self,
        method: &str,
        url: &str,
        token: &str,
        body: Option<&str>,
    ) -> Result<ControlResponse, String>;
}

struct CurlTransport;

impl OrganizationTransport for CurlTransport {
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

pub fn cmd_organization(args: &[String]) -> Result<(), String> {
    run(&mut CurlTransport, args)
}

pub(crate) fn run<T: OrganizationTransport>(
    transport: &mut T,
    args: &[String],
) -> Result<(), String> {
    let sub = args.get(2).map(String::as_str).unwrap_or("");
    if !SUBCOMMANDS.contains(&sub) {
        eprintln!("{}", usage());
        return Err(unknown_subcommand(sub));
    }
    check_unknown_flags(args)?;

    // `use` touches no server, so it must not require a token or a control URL.
    if sub == "use" {
        return cmd_use(args);
    }

    let (organization, origin) = if needs_organization(sub) {
        let (id, origin) = resolve_organization(args)?;
        (Some(id), Some(origin))
    } else {
        (None, None)
    };

    let call = plan(sub, args, organization.as_deref())?;

    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot read the working directory: {e}"))?;
    let file = project_config::locate(args, &cwd)?;
    let config = file
        .as_deref()
        .map(project_config::ProjectConfig::load)
        .transpose()?;
    let resolved = match (&config, flag_str(args, "--env=")) {
        (Some(cfg), env) => Some(cfg.resolve(env.as_deref())?),
        (None, Some(env)) => {
            return Err(format!(
                "--env={env} needs a {} in this directory to read the environment from",
                project_config::CONFIG_FILENAME
            ))
        }
        (None, None) => None,
    };
    let control = project_config::resolve_control(args, resolved.as_ref())?;
    let token = resolve_bearer_token(args)?;

    project_config::print_provenance("organization", &[("control", &control)]);
    if let (Some(id), Some(origin)) = (&organization, &origin) {
        eprintln!("zeroship organization: organization = {id} (from {origin})");
    }

    let url = format!("{}{}", control.value.trim_end_matches('/'), call.path);
    let response = transport.send(call.method, &url, &token, call.body.as_deref())?;
    report(sub, &response)
}

/// Turn one response into this process's output and exit status.
///
/// A `204` carries no body and every other success carries JSON; the body goes
/// to stdout verbatim so it can be piped, and every explanation goes to stderr
/// so piping stays clean.
fn report(sub: &str, response: &ControlResponse) -> Result<(), String> {
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
        // Both hand back an organization the caller now belongs to, and both
        // are the moment a first-time creator would otherwise have to name it
        // again.
        "create" | "join" => note_selection_after(&response.body),
        "invite" => note_invite_delivery(&response.body),
        "transfer" | "remove" | "revoke" => eprintln!("zeroship organization: {sub} applied"),
        "leave" => eprintln!(
            "zeroship organization: seat given up. You keep no authority here; rejoining \
             needs a new invitation."
        ),
        "dissolve" => eprintln!(
            "zeroship organization: closed. It stays readable and accepts no further \
             changes, and its name is free for a new organization to use."
        ),
        _ => {}
    }
    Ok(())
}

/// Say what became of the invitation email, and show the token only when the
/// caller has to deliver it themselves.
///
/// The platform mails the invitation now, so telling every caller to "send the
/// token" would be advice that is wrong in the ordinary case - and advice that
/// is usually wrong is advice nobody reads on the day it matters. The response
/// carries `delivery`, so the line below is the one that applies.
fn note_invite_delivery(body: &str) {
    let delivery = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("delivery")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    match delivery.as_deref() {
        Some("sent") => eprintln!(
            "zeroship organization: the invitation was emailed. The `token` above is shown \
             ONCE and is stored only as a digest; you do not need to pass it on, but it is \
             there if you would rather deliver it yourself."
        ),
        Some("suppressed") => eprintln!(
            "zeroship organization: the invitation was NOT emailed - that address is on the \
             platform's suppression list after a bounce or complaint. Send the `token` above \
             another way; it is shown once and cannot be re-read."
        ),
        Some("failed") => eprintln!(
            "zeroship organization: the invitation was created but the email FAILED to send. \
             Send the `token` above another way; it is shown once and cannot be re-read, and \
             a lost one means revoking the invitation and issuing another."
        ),
        // An unrecognised (or absent) outcome must not be reported as a
        // success. The token is the thing that always works, so say that.
        _ => eprintln!(
            "zeroship organization: the `token` above is shown ONCE and is stored only as a \
             digest. Delivery is not confirmed, so send it to the person you invited; a lost \
             token means revoking the invitation and issuing another."
        ),
    }
}

/// After minting or joining an organization, select it when nothing is selected.
///
/// The zero-config first run is the whole point: a creator who has just made
/// their first organization should not have to name it again. When a selection
/// already exists it is left alone and the command that would change it is
/// printed - silently re-pointing an existing selection is how the NEXT command
/// acts on the wrong company.
fn note_selection_after(body: &str) {
    let Some(id) = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
    else {
        return;
    };
    match read_selection() {
        Ok(Some(current)) if current == id => {}
        Ok(Some(current)) => eprintln!(
            "zeroship organization: still selected: {current}. \
             Run `zeroship organization use {id}` to switch."
        ),
        Ok(None) => match write_selection(&id) {
            Ok(path) => eprintln!("zeroship organization: selected {id} ({})", path.display()),
            Err(e) => eprintln!("zeroship organization: could not record the selection: {e}"),
        },
        Err(e) => eprintln!("zeroship organization: could not read the selection: {e}"),
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
        "  zeroship organization create   <name> [--slug=SLUG] [--billing-email=ADDR]\n",
        "  zeroship organization list\n",
        "  zeroship organization show     [--organization=org_...]\n",
        "  zeroship organization use      <org_...> | --clear\n",
        "  zeroship organization members  [--organization=org_...]\n",
        "  zeroship organization invite   <email> --role=ROLE [--organization=org_...]\n",
        "  zeroship organization revoke   <ivt_...> [--organization=org_...]\n",
        "  zeroship organization join     <token>\n",
        "  zeroship organization role     <user-id> --role=ROLE [--organization=org_...]\n",
        "  zeroship organization remove   <user-id> [--organization=org_...]\n",
        "  zeroship organization leave    [--organization=org_...]\n",
        "  zeroship organization transfer <user-id> [--organization=org_...]\n",
        "  zeroship organization dissolve [--organization=org_...]\n",
        "  zeroship organization projects [--organization=org_...]\n",
        "  zeroship organization projects create <name> [--slug=SLUG]\n",
        "  zeroship organization projects rename <prj_...> [--name=NAME] [--slug=SLUG]\n",
        "  zeroship organization projects delete <prj_...>\n",
        "  zeroship organization projects members <prj_...>\n",
        "  zeroship organization projects add    <prj_...> <user-id> --role=ROLE\n",
        "  zeroship organization projects role   <prj_...> <user-id> --role=ROLE\n",
        "  zeroship organization projects remove <prj_...> <user-id>\n",
        "\n",
        "An organization owns projects, a project owns apps, and the organization is the\n",
        "party that is billed. `use` records which one the other subcommands act on, so\n",
        "--organization= is only needed to override it.\n",
        "\n",
        "`leave` gives up YOUR seat and needs no rank; `remove` takes someone else's and\n",
        "needs authority over them. `dissolve` closes the organization for everyone: it\n",
        "is owner-only, it is refused while any project remains, and it cannot be undone.\n",
        "A closed organization stays readable and releases its name for reuse.\n",
        "\n",
        "A project seat NARROWS: a member's authority on a project is the lower of their\n",
        "organization rank and their project rank, and admins reach every project with no\n",
        "seat at all.\n",
        "\n",
        "Roles are rows in the platform's role ladder; the control plane names the whole\n",
        "set if you pass one it does not have."
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(rest: &[&str]) -> Vec<String> {
        std::iter::once("zeroship".to_string())
            .chain(std::iter::once("organization".to_string()))
            .chain(rest.iter().map(|s| (*s).to_string()))
            .collect()
    }

    const ORG: &str = "org_0123456789abcdefghijkl";
    const PRJ: &str = "prj_0123456789abcdefghijkl";
    const INVITE: &str = "ivt_0123456789abcdefghijkl";
    const USER: &str = "11111111-2222-3333-4444-555555555555";

    /// The routing table, stated as data. Every remote subcommand appears, and
    /// the assertion is on the METHOD, PATH and BODY - the three things a
    /// mistake here sends to the wrong place.
    #[test]
    fn every_subcommand_targets_its_route() {
        let cases: &[(&[&str], &str, String, Option<&str>)] = &[
            (
                &["create", "Acme"],
                "POST",
                "/api/organizations".into(),
                Some(r#"{"name":"Acme"}"#),
            ),
            (&["list"], "GET", "/api/organizations".into(), None),
            (&["show"], "GET", format!("/api/organizations/{ORG}"), None),
            (
                &["members"],
                "GET",
                format!("/api/organizations/{ORG}/members"),
                None,
            ),
            (
                &["invite", "a@b.test", "--role=developer"],
                "POST",
                format!("/api/organizations/{ORG}/invites"),
                Some(r#"{"email":"a@b.test","role":"developer"}"#),
            ),
            (
                &["revoke", INVITE],
                "DELETE",
                format!("/api/organizations/{ORG}/invites/{INVITE}"),
                None,
            ),
            (
                &["join", "tok"],
                "POST",
                "/api/organization-invites/redeem".into(),
                Some(r#"{"token":"tok"}"#),
            ),
            (
                &["role", USER, "--role=admin"],
                "PATCH",
                format!("/api/organizations/{ORG}/members/{USER}"),
                Some(r#"{"role":"admin"}"#),
            ),
            (
                &["remove", USER],
                "DELETE",
                format!("/api/organizations/{ORG}/members/{USER}"),
                None,
            ),
            (
                &["transfer", USER],
                "POST",
                format!("/api/organizations/{ORG}/transfer"),
                Some(&format!(r#"{{"user_id":"{USER}"}}"#)),
            ),
            (
                &["leave"],
                "DELETE",
                format!("/api/organizations/{ORG}/membership"),
                None,
            ),
            (
                &["dissolve"],
                "DELETE",
                format!("/api/organizations/{ORG}"),
                None,
            ),
            (
                &["projects"],
                "GET",
                format!("/api/organizations/{ORG}/projects"),
                None,
            ),
            (
                &["projects", "create", "Checkout"],
                "POST",
                format!("/api/organizations/{ORG}/projects"),
                Some(r#"{"name":"Checkout"}"#),
            ),
        ];

        for (rest, method, path, body) in cases {
            let args = argv(rest);
            let sub = rest[0];
            let organization = needs_organization(sub).then_some(ORG);
            let call = plan(sub, &args, organization)
                .unwrap_or_else(|e| panic!("{rest:?} did not plan: {e}"));
            assert_eq!(call.method, *method, "{rest:?} method");
            assert_eq!(call.path, *path, "{rest:?} path");
            assert_eq!(call.body.as_deref(), *body, "{rest:?} body");
        }

        // The list above must cover every remote subcommand. Without this an
        // arm added to `plan` and forgotten here is tested by nothing, and the
        // table above still passes because the cases it does name are correct.
        let covered: Vec<&str> = cases.iter().map(|(rest, ..)| rest[0]).collect();
        for sub in SUBCOMMANDS {
            if *sub == "use" {
                continue; // local; it issues no call and is covered separately
            }
            assert!(
                covered.contains(sub),
                "`{sub}` is dispatched but has no routing case"
            );
        }
    }

    /// Every `projects` verb, stated the same way and covering the SET.
    ///
    /// Kept separate from the subcommand table above because the verbs live in
    /// their own list (`PROJECT_VERBS`) and their own planner: folding them in
    /// would make one exhaustiveness assertion answer for two vocabularies, and
    /// a verb added to one list and not the other would still pass.
    #[test]
    fn every_project_verb_targets_its_route() {
        let cases: &[(&[&str], &str, String, Option<&str>)] = &[
            (
                &["projects", "create", "Checkout"],
                "POST",
                format!("/api/organizations/{ORG}/projects"),
                Some(r#"{"name":"Checkout"}"#),
            ),
            (
                &["projects", "rename", PRJ, "--name=Checkout"],
                "PATCH",
                format!("/api/projects/{PRJ}"),
                Some(r#"{"name":"Checkout"}"#),
            ),
            (
                &["projects", "delete", PRJ],
                "DELETE",
                format!("/api/projects/{PRJ}"),
                None,
            ),
            (
                &["projects", "members", PRJ],
                "GET",
                format!("/api/projects/{PRJ}/members"),
                None,
            ),
            (
                &["projects", "add", PRJ, USER, "--role=developer"],
                "POST",
                format!("/api/projects/{PRJ}/members"),
                Some(r#"{"user_id":"11111111-2222-3333-4444-555555555555","role":"developer"}"#),
            ),
            (
                &["projects", "role", PRJ, USER, "--role=viewer"],
                "PATCH",
                format!("/api/projects/{PRJ}/members/{USER}"),
                Some(r#"{"role":"viewer"}"#),
            ),
            (
                &["projects", "remove", PRJ, USER],
                "DELETE",
                format!("/api/projects/{PRJ}/members/{USER}"),
                None,
            ),
        ];

        for (rest, method, path, body) in cases {
            let args = argv(rest);
            let call = plan("projects", &args, Some(ORG))
                .unwrap_or_else(|e| panic!("{rest:?} did not plan: {e}"));
            assert_eq!(call.method, *method, "{rest:?} method");
            assert_eq!(call.path, *path, "{rest:?} path");
            assert_eq!(call.body.as_deref(), *body, "{rest:?} body");
        }

        // The SET, not a sample: a verb the planner handles and this table
        // omits would be tested by nothing while every case above still passes.
        let covered: Vec<&str> = cases.iter().map(|(rest, ..)| rest[1]).collect();
        for verb in PROJECT_VERBS {
            assert!(covered.contains(verb), "`projects {verb}` has no routing case");
        }
        assert_eq!(covered.len(), PROJECT_VERBS.len(), "a verb is covered twice");
    }

    /// A project verb that needs an id refuses a slug, an organization id and a
    /// missing argument, HERE - before a request goes anywhere.
    ///
    /// A slug where an id belongs resolves to no membership at all, so the
    /// server can only answer it with a refusal about authority: a message
    /// pointing at the one thing the caller did not get wrong.
    #[test]
    fn a_project_verb_refuses_anything_that_is_not_a_project_id() {
        for verb in ["rename", "delete", "members", "add", "role", "remove"] {
            let err = plan("projects", &argv(&["projects", verb]), Some(ORG))
                .expect_err("{verb} with no id must be refused");
            assert!(err.contains("prj_"), "{verb}: {err:?}");

            for wrong in ["checkout", ORG] {
                let err = plan(
                    "projects",
                    &argv(&["projects", verb, wrong, USER, "--role=viewer"]),
                    Some(ORG),
                )
                .expect_err("a non-project id must be refused");
                assert!(
                    err.contains("not a project id"),
                    "{verb} accepted {wrong:?}: {err:?}"
                );
            }
        }
    }

    /// `rename` naming nothing to change is refused here rather than becoming a
    /// request the server answers with a 400.
    #[test]
    fn renaming_a_project_must_name_something_to_change() {
        let err = plan("projects", &argv(&["projects", "rename", PRJ]), Some(ORG))
            .expect_err("an empty rename is not a request");
        assert!(err.contains("--name"), "{err:?}");
        assert!(err.contains("--slug"), "{err:?}");

        // The control: either flag alone is enough, and both compose.
        let both = plan(
            "projects",
            &argv(&["projects", "rename", PRJ, "--name=Checkout", "--slug=checkout"]),
            Some(ORG),
        )
        .unwrap();
        assert_eq!(
            both.body.as_deref(),
            Some(r#"{"name":"Checkout","slug":"checkout"}"#)
        );
        let slug_only = plan(
            "projects",
            &argv(&["projects", "rename", PRJ, "--slug=checkout"]),
            Some(ORG),
        )
        .unwrap();
        assert_eq!(slug_only.body.as_deref(), Some(r#"{"slug":"checkout"}"#));
    }

    /// `leave` and `dissolve` are different routes, and neither can be aimed at
    /// somebody else.
    ///
    /// `leave` sends no user id at all - the route has no segment for one - so
    /// a stray positional cannot redirect it. `dissolve` targets the
    /// organization itself, and the two paths must not be confusable.
    #[test]
    fn leaving_and_dissolving_cannot_be_aimed_at_anyone_else() {
        let leave = plan("leave", &argv(&["leave", USER]), Some(ORG)).unwrap();
        assert_eq!(leave.path, format!("/api/organizations/{ORG}/membership"));
        assert!(!leave.path.contains(USER), "leave must name no user");
        assert_eq!(leave.body, None);

        let dissolve = plan("dissolve", &argv(&["dissolve"]), Some(ORG)).unwrap();
        assert_eq!(dissolve.path, format!("/api/organizations/{ORG}"));
        assert_ne!(dissolve.path, leave.path);

        // `remove` is the one that takes a target, and it still does.
        let remove = plan("remove", &argv(&["remove", USER]), Some(ORG)).unwrap();
        assert_eq!(
            remove.path,
            format!("/api/organizations/{ORG}/members/{USER}")
        );
    }

    /// The invitation note tells the truth about delivery, per outcome.
    ///
    /// A CLI that told every caller to "send the token" would be wrong in the
    /// ordinary case now that the platform mails it, and advice that is usually
    /// wrong is advice nobody reads on the day it matters.
    #[test]
    fn the_invite_note_follows_the_reported_delivery() {
        // The function writes to stderr, so what is asserted here is that each
        // outcome PARSES to a distinct arm. The bodies are the shapes the
        // server sends.
        for body in [
            r#"{"delivery":"sent","token":"t"}"#,
            r#"{"delivery":"suppressed","token":"t"}"#,
            r#"{"delivery":"failed","token":"t"}"#,
            r#"{"token":"t"}"#,
            "not json",
        ] {
            note_invite_delivery(body);
        }
        let parsed: Option<String> = serde_json::from_str::<serde_json::Value>(
            r#"{"delivery":"suppressed"}"#,
        )
        .ok()
        .and_then(|v| {
            v.get("delivery")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
        assert_eq!(parsed.as_deref(), Some("suppressed"));
    }

    #[test]
    fn optional_creation_fields_appear_only_when_passed() {
        let bare = plan("create", &argv(&["create", "Acme"]), None).unwrap();
        assert_eq!(bare.body.as_deref(), Some(r#"{"name":"Acme"}"#));

        let full = plan(
            "create",
            &argv(&[
                "create",
                "Acme",
                "--slug=acme",
                "--billing-email=pay@acme.test",
            ]),
            None,
        )
        .unwrap();
        assert_eq!(
            full.body.as_deref(),
            Some(r#"{"name":"Acme","slug":"acme","billing_email":"pay@acme.test"}"#)
        );

        let project = plan(
            "projects",
            &argv(&["projects", "create", "Checkout", "--slug=checkout"]),
            Some(ORG),
        )
        .unwrap();
        assert_eq!(
            project.body.as_deref(),
            Some(r#"{"name":"Checkout","slug":"checkout"}"#)
        );
    }

    #[test]
    fn a_flag_before_the_positional_does_not_swallow_it() {
        let call = plan(
            "invite",
            &argv(&["invite", "--role=viewer", "someone@example.test"]),
            Some(ORG),
        )
        .unwrap();
        assert_eq!(
            call.body.as_deref(),
            Some(r#"{"email":"someone@example.test","role":"viewer"}"#)
        );
    }

    /// The regression: `parse_flag` accepts `--organization org_x` as well as
    /// `--organization=org_x`, so in the space form the id sits in argv as an
    /// ordinary word. The first version of `positional` returned it as the
    /// email, and the server refused the invitation with a complaint about the
    /// ADDRESS - a message that points at the value the creator typed
    /// correctly, not at the one the parser stole.
    #[test]
    fn a_space_form_flag_value_is_not_mistaken_for_the_positional() {
        let call = plan(
            "invite",
            &argv(&[
                "invite",
                "--organization",
                ORG,
                "someone@example.test",
                "--role=viewer",
            ]),
            Some(ORG),
        )
        .unwrap();
        assert_eq!(
            call.body.as_deref(),
            Some(r#"{"email":"someone@example.test","role":"viewer"}"#)
        );

        // The same for a two-word positional command and for `use`, whose
        // argument is itself an organization id and so cannot be told apart
        // from a stolen flag value by inspection.
        let project = plan(
            "projects",
            &argv(&["projects", "--organization", ORG, "create", "Checkout"]),
            Some(ORG),
        )
        .unwrap();
        assert_eq!(project.body.as_deref(), Some(r#"{"name":"Checkout"}"#));

        // Every value-taking flag must be listed, or this fails for that one.
        for flag in VALUE_FLAGS {
            assert!(
                KNOWN_FLAGS.contains(flag),
                "{flag} takes a value but is not a known flag"
            );
        }
        assert_eq!(
            VALUE_FLAGS.len() + 1,
            KNOWN_FLAGS.len(),
            "--clear is the only valueless flag"
        );
    }

    #[test]
    fn a_missing_role_is_refused_before_the_call_is_built() {
        for rest in [
            vec!["invite", "a@b.test"],
            vec!["role", USER],
            vec!["invite", "a@b.test", "--role="],
        ] {
            let err = plan(rest[0], &argv(&rest), Some(ORG)).unwrap_err();
            assert!(err.contains("--role"), "{rest:?} said {err:?}");
        }
    }

    #[test]
    fn a_user_id_that_is_not_a_uuid_is_refused_here() {
        for sub in ["role", "remove", "transfer"] {
            let args = argv(&[sub, "someone@example.test", "--role=viewer"]);
            let err = plan(sub, &args, Some(ORG)).unwrap_err();
            assert!(
                err.contains("not a user id"),
                "{sub} accepted an email as a user id: {err:?}"
            );
        }
    }

    /// The typed-id parse is what keeps a SLUG out of a path the server can
    /// only answer with a 400. The control phase's notes call this out: a slug
    /// where an id belongs resolves rank 0 and denies with nothing to read.
    #[test]
    fn a_slug_is_not_an_organization_id() {
        assert!(OrganizationId::parse("acme").is_err());
        assert!(OrganizationId::parse(ORG).is_ok());
        // The two id types this module sends must not accept each other's
        // values: `revoke` takes both, one per path segment.
        assert!(
            InviteId::parse(ORG).is_err(),
            "an organization id parsed as an invitation id"
        );
        assert!(
            OrganizationId::parse(INVITE).is_err(),
            "an invitation id parsed as an organization id"
        );
    }

    #[test]
    fn unknown_flags_are_refused_rather_than_ignored() {
        // The misspelling that matters: it would act on the SELECTED
        // organization while the creator believes they named one.
        let err = check_unknown_flags(&argv(&["members", "--organisation=org_x"])).unwrap_err();
        assert!(err.contains("--organisation"), "{err:?}");
        check_unknown_flags(&argv(&["members", "--organization=org_x"])).unwrap();
    }

    #[test]
    fn unknown_subcommands_name_the_whole_set() {
        let err = unknown_subcommand("teams");
        for sub in SUBCOMMANDS {
            assert!(err.contains(sub), "{err:?} omits {sub}");
        }
        // The reserved word must not have become a command by any route.
        assert!(!SUBCOMMANDS.contains(&"team"));
        assert!(!SUBCOMMANDS.contains(&"org"));
    }

    #[test]
    fn only_the_subcommands_that_act_on_one_need_an_organization() {
        // Stated as a PARTITION of the dispatch list, not as two lists that
        // happen to agree with it: a subcommand added to `SUBCOMMANDS` and to
        // neither of these would otherwise be ruled on by neither.
        let (targeted, free): (Vec<&str>, Vec<&str>) = SUBCOMMANDS
            .iter()
            .copied()
            .partition(|sub| needs_organization(sub));
        assert_eq!(free, ["create", "list", "use", "join"]);
        assert_eq!(
            targeted,
            [
                "show",
                "members",
                "invite",
                "revoke",
                "role",
                "remove",
                "leave",
                "transfer",
                "dissolve",
                "projects"
            ]
        );
    }

    #[test]
    fn the_usage_text_lists_every_subcommand() {
        let text = usage();
        for sub in SUBCOMMANDS {
            assert!(
                text.contains(&format!("organization {sub}")),
                "usage omits `{sub}`"
            );
        }
    }

    struct Recorder {
        calls: Vec<(String, String, Option<String>)>,
        status: u16,
        body: String,
    }

    impl OrganizationTransport for Recorder {
        fn send(
            &mut self,
            method: &str,
            url: &str,
            _token: &str,
            body: Option<&str>,
        ) -> Result<ControlResponse, String> {
            self.calls.push((
                method.to_string(),
                url.to_string(),
                body.map(str::to_string),
            ));
            Ok(ControlResponse {
                status: self.status,
                body: self.body.clone(),
            })
        }
    }

    #[test]
    fn a_refusal_becomes_an_error_carrying_the_server_status_and_body() {
        let response = ControlResponse {
            status: 403,
            body: r#"{"error":"insufficient authority"}"#.to_string(),
        };
        let err = report("role", &response).unwrap_err();
        assert!(err.contains("403"), "{err:?}");
        assert!(err.contains("insufficient authority"), "{err:?}");

        // A 204 is a success with nothing to print, not an empty failure.
        report(
            "remove",
            &ControlResponse {
                status: 204,
                body: String::new(),
            },
        )
        .unwrap();
    }

    #[test]
    fn the_url_is_the_control_origin_plus_the_planned_path() {
        // Reproduces the join `run` performs, including the trailing-slash
        // trim: `http://x/` + `/api/...` would otherwise produce `//api/...`,
        // which is a different path to actix and 404s.
        for base in ["http://control.test", "http://control.test/"] {
            let call = plan("show", &argv(&["show"]), Some(ORG)).unwrap();
            let url = format!("{}{}", base.trim_end_matches('/'), call.path);
            assert_eq!(url, format!("http://control.test/api/organizations/{ORG}"));
        }
    }

    #[test]
    fn the_transport_seam_carries_the_planned_call_verbatim() {
        let mut recorder = Recorder {
            calls: Vec::new(),
            status: 201,
            body: r#"{"id":"prj_0123456789abcdefghijkl"}"#.to_string(),
        };
        let call = plan(
            "projects",
            &argv(&["projects", "create", "Checkout"]),
            Some(ORG),
        )
        .unwrap();
        let url = format!("http://control.test{}", call.path);
        recorder
            .send(call.method, &url, "tok", call.body.as_deref())
            .unwrap();
        assert_eq!(
            recorder.calls,
            vec![(
                "POST".to_string(),
                format!("http://control.test/api/organizations/{ORG}/projects"),
                Some(r#"{"name":"Checkout"}"#.to_string())
            )]
        );
    }
}
