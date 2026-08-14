//! `zeroship secret` / `zeroship var` subcommands.
//!
//! Shape:
//!   zeroship secret set  KEY=value   --app=<uuid> [--expose] [--control=URL] [--token=PAT]
//!   zeroship secret list             --app=<uuid> [...]
//!   zeroship secret rm   KEY         --app=<uuid> [...]
//!   zeroship secret expose      KEY  --app=<uuid> [...]
//!   zeroship secret unexpose    KEY  --app=<uuid> [...]
//!   zeroship secret expose-list      --app=<uuid> [...]
//!   zeroship var    set  KEY=value   --app=<uuid> [...]
//!   zeroship var    list             --app=<uuid>
//!   zeroship var    rm   KEY         --app=<uuid>
//!
//! The expose list is the per-app opt-in that decides which *secrets*
//! also appear in `process.env` (and `globalThis.__env__`). The zeroship
//! `env` object always carries every secret; `process.env` carries vars
//! plus only the exposed secrets, because `process.env` is readable by
//! any npm dependency without the creator writing a line of code.
//! `PUT /api/apps/:id/env/expose` REPLACES the whole list, so `expose` /
//! `unexpose` read-modify-write it. Never PUT a bare single name.
//!
//! Uses curl for HTTP (matches `cmd_deploy`'s pattern). Returns exit
//! code 0 on success, 1 on any error.

use std::process::{Command, Stdio};

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

use crate::project_config;
use crate::{flag_str, resolve_bearer_token};

const KEY_RULE: &str = "KEY must be 1-64 bytes, start with an ASCII uppercase letter, and contain only ASCII uppercase letters, digits, or underscores";

// Encode every ASCII delimiter plus `.` so each value remains exactly one path
// segment, including values that resemble dot segments or existing escapes.
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

pub fn cmd_secret(args: &[String]) {
    run("secrets", args);
}

pub fn cmd_var(args: &[String]) {
    run("vars", args);
}

fn run(resource: &str, args: &[String]) {
    let sub = args.get(2).map(String::as_str).unwrap_or("");
    match sub {
        "set" => cmd_set(resource, args),
        "list" | "ls" => cmd_list(resource, args),
        "rm" | "del" | "delete" => cmd_rm(resource, args),
        "expose" => cmd_expose(resource, args, ExposeChange::Add),
        "unexpose" => cmd_expose(resource, args, ExposeChange::Remove),
        "expose-list" => cmd_expose_list(resource, args),
        _ => {
            usage(resource);
            std::process::exit(1);
        }
    }
}

fn usage(resource: &str) {
    if resource == "secrets" {
        eprintln!(
            concat!(
                "Usage:\n",
                "  zeroship secret set  KEY=value --app=<uuid> [--expose] [--control=URL] [--token=PAT]\n",
                "  zeroship secret list           --app=<uuid> [--control=URL] [--token=PAT]\n",
                "  zeroship secret rm   KEY       --app=<uuid> [--control=URL] [--token=PAT]\n",
                "  zeroship secret expose      KEY --app=<uuid> [--control=URL] [--token=PAT]\n",
                "  zeroship secret unexpose    KEY --app=<uuid> [--control=URL] [--token=PAT]\n",
                "  zeroship secret expose-list     --app=<uuid> [--control=URL] [--token=PAT]\n",
                "\n",
                "Secrets are encrypted at rest and always readable from the zeroship `env`\n",
                "object. They reach `process.env` (where any npm dependency can read them)\n",
                "ONLY if exposed: `--expose` on set, or `secret expose KEY` later.",
            )
        );
    } else {
        eprintln!(
            concat!(
                "Usage:\n",
                "  zeroship var set  KEY=value --app=<uuid> [--control=URL] [--token=PAT]\n",
                "  zeroship var list           --app=<uuid> [--control=URL] [--token=PAT]\n",
                "  zeroship var rm   KEY       --app=<uuid> [--control=URL] [--token=PAT]\n",
                "\n",
                "Vars are stored in PLAINTEXT and are always visible in both `env` and\n",
                "`process.env`. For credentials use `zeroship secret set` instead.",
            )
        );
    }
}

/// First non-flag argument after the subcommand. Positional, like the
/// rest of this module, but tolerant of a flag written before the
/// positional (`secret set --expose FOO=bar`). A KEY never starts with
/// `--`, so this cannot swallow one.
fn positional(args: &[String]) -> String {
    args.iter()
        .skip(3)
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_default()
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// Known flags accepted by `secret` and `var` (bare names, no `=`).
///
/// THESE TWO COMMANDS HAD NO GATE AT ALL until this change, and the gap became
/// load-bearing the moment they started reading a config file: an unrecognised
/// `--contrl=` was silently ignored, which used to mean "fall back to
/// localhost" and would now mean "silently use whatever the file says". Adding
/// a config layer to a command that swallows typos makes a wrong target MORE
/// reachable, not less (proposal 2.4), so the gate lands with the layer.
const ENV_KNOWN_FLAGS: &[&str] = &[
    "--app", "--control", "--token", "--expose", "--config", "--env",
];

fn check_unknown_flags(resource: &str, args: &[String]) -> Result<(), String> {
    // args[0] = binary, args[1] = secret|var, args[2] = subcommand.
    for arg in args.iter().skip(3) {
        if !arg.starts_with("--") {
            continue;
        }
        let name = match arg.find('=') {
            Some(i) => &arg[..i],
            None => arg.as_str(),
        };
        if !ENV_KNOWN_FLAGS.contains(&name) {
            return Err(format!(
                "unknown flag `{name}`; it would have been ignored in silence. \
                 Known flags: {}. Run `zeroship {}` with no subcommand for usage.",
                ENV_KNOWN_FLAGS.join(" "),
                if resource == "secrets" { "secret" } else { "var" }
            ));
        }
    }
    Ok(())
}

fn common(resource: &str, args: &[String]) -> (String, String, String) {
    let label = if resource == "secrets" { "secret" } else { "var" };
    let die = |e: String| -> ! {
        eprintln!("zeroship {label}: {e}");
        std::process::exit(1);
    };
    if let Err(e) = check_unknown_flags(resource, args) {
        die(e);
    }

    let cwd = std::env::current_dir().unwrap_or_else(|e| die(format!("cannot read the working directory: {e}")));
    let file = project_config::locate(args, &cwd).unwrap_or_else(|e| die(e));
    let config = file
        .as_deref()
        .map(project_config::ProjectConfig::load)
        .transpose()
        .unwrap_or_else(|e| die(e));
    let resolved = match (&config, flag_str(args, "--env=")) {
        (Some(cfg), env) => Some(cfg.resolve(env.as_deref()).unwrap_or_else(|e| die(e))),
        (None, Some(env)) => die(format!(
            "--env={env} needs a {} in this directory to read the environment from",
            project_config::CONFIG_FILENAME
        )),
        (None, None) => None,
    };

    let app = project_config::resolve_value(args, "--app", None, None, resolved.as_ref(), "app", None)
        .unwrap_or_else(|e| die(e));
    let control_url = project_config::resolve_value(
        args,
        "--control",
        Some("ZEROSHIP_CONTROL_URL"),
        zeroship_core::declared_env!(cli, "ZEROSHIP_CONTROL_URL", crate::ZeroshipCliConsumer),
        resolved.as_ref(),
        "control",
        Some("http://localhost:9090"),
    )
    .unwrap_or_else(|e| die(e));
    let token = resolve_bearer_token(args).unwrap_or_else(|e| die(e));

    project_config::print_provenance(label, &[("app", &app), ("control", &control_url)]);
    (app.value, control_url.value, token)
}

fn cmd_set(resource: &str, args: &[String]) {
    // The positional is `KEY=value`. We split on the first `=` so values
    // containing `=` themselves (e.g. base64 tokens) work correctly.
    let pair = positional(args);
    let (key, value) = match pair.split_once('=') {
        Some(p) => p,
        None => {
            eprintln!("error: expected KEY=value (e.g. STRIPE_KEY=sk_test_...)");
            std::process::exit(1);
        }
    };
    require_valid_key(key);
    let expose = has_flag(args, "--expose");
    if expose && resource != "secrets" {
        eprintln!(
            "error: --expose applies to secrets only; vars are plaintext and are \
             already visible in process.env"
        );
        std::process::exit(1);
    }
    let (app, control_url, token) = common(resource, args);

    let url = resource_url(&control_url, &app, resource, None);
    let body = serde_json::json!({ "key": key, "value": value }).to_string();

    let (status, body_out) = curl_json("POST", &url, &token, Some(&body));
    if !(200..300).contains(&status) {
        eprintln!("{resource}: set failed ({status}): {body_out}");
        std::process::exit(1);
    }
    eprintln!("{resource}: set {key} on {app}");

    // Say, at the point of use, which surface the value landed on. A
    // creator who reads nothing else reads this line.
    if resource == "secrets" {
        if expose {
            // Store first, expose second: a failure here leaves the
            // secret set but unexposed, which is the safe direction.
            apply_expose(&app, &control_url, &token, key, ExposeChange::Add);
        } else {
            eprintln!(
                "secrets: {key} is readable as env.{key} but NOT in process.env. \
                 If a library reads process.env.{key} (e.g. the AI SDK reading \
                 OPENAI_API_KEY), re-run with --expose."
            );
        }
    } else {
        eprintln!(
            "vars: {key} is stored in PLAINTEXT and is visible in both env and \
             process.env. Use `zeroship secret set` for credentials."
        );
    }
}

fn cmd_list(resource: &str, args: &[String]) {
    let (app, control_url, token) = common(resource, args);
    let url = resource_url(&control_url, &app, resource, None);
    let (status, body) = curl_json("GET", &url, &token, None);
    if (200..300).contains(&status) {
        println!("{}", body);
    } else {
        eprintln!("{resource}: list failed ({status}): {body}");
        std::process::exit(1);
    }
}

fn cmd_rm(resource: &str, args: &[String]) {
    let key = positional(args);
    if key.is_empty() {
        eprintln!(
            "error: missing KEY (e.g. `zeroship {} rm FOO --app=<uuid>`)",
            if resource == "secrets" { "secret" } else { "var" }
        );
        std::process::exit(1);
    }
    require_valid_key(&key);
    let (app, control_url, token) = common(resource, args);
    let url = resource_url(&control_url, &app, resource, Some(&key));
    let (status, body) = curl_json("DELETE", &url, &token, None);
    match status {
        204 => eprintln!("{resource}: removed {key} from {app}"),
        404 => {
            eprintln!("{resource}: {key} not found on {app}");
            std::process::exit(1);
        }
        _ => {
            eprintln!("{resource}: rm failed ({status}): {body}");
            std::process::exit(1);
        }
    }
}

// ------------------------------------------------------------------
// Expose list (which secrets also reach `process.env`)
// ------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ExposeChange {
    Add,
    Remove,
}

impl ExposeChange {
    fn verb(self) -> &'static str {
        match self {
            ExposeChange::Add => "expose",
            ExposeChange::Remove => "unexpose",
        }
    }
}

fn require_secrets_resource(resource: &str, sub: &str) {
    if resource != "secrets" {
        eprintln!(
            "error: `{sub}` applies to secrets only; vars are plaintext and are always \
             visible in process.env"
        );
        std::process::exit(1);
    }
}

fn cmd_expose(resource: &str, args: &[String], change: ExposeChange) {
    require_secrets_resource(resource, change.verb());
    let key = positional(args);
    if key.is_empty() {
        eprintln!(
            "error: missing KEY (e.g. `zeroship secret {} OPENAI_API_KEY --app=<uuid>`)",
            change.verb()
        );
        std::process::exit(1);
    }
    require_valid_key(&key);
    let (app, control_url, token) = common(resource, args);
    apply_expose(&app, &control_url, &token, &key, change);
}

fn cmd_expose_list(resource: &str, args: &[String]) {
    require_secrets_resource(resource, "expose-list");
    let (app, control_url, token) = common(resource, args);
    let names = fetch_expose(&control_url, &app, &token);
    if names.is_empty() {
        eprintln!("secrets: no secrets are exposed to process.env on {app}");
    } else {
        for name in names {
            println!("{name}");
        }
    }
}

/// Read-modify-write the expose list. `PUT .../env/expose` REPLACES the
/// whole list, so we must GET the current names first and PUT the union
/// (or the remainder). PUTting a bare single name would silently
/// un-expose every other secret on the app.
fn apply_expose(app: &str, control_url: &str, token: &str, key: &str, change: ExposeChange) {
    let current = fetch_expose(control_url, app, token);
    let Some(next) = merge_expose(&current, key, change) else {
        match change {
            ExposeChange::Add => {
                eprintln!("secrets: {key} is already exposed to process.env on {app}")
            }
            ExposeChange::Remove => {
                eprintln!("secrets: {key} is not exposed to process.env on {app}")
            }
        }
        return;
    };

    let url = expose_url(control_url, app);
    let body = serde_json::json!({ "keys": next }).to_string();
    let (status, body_out) = curl_json("PUT", &url, token, Some(&body));
    if !(200..300).contains(&status) {
        eprintln!(
            "secrets: {} failed ({status}): {body_out}",
            change.verb()
        );
        std::process::exit(1);
    }
    match change {
        ExposeChange::Add => eprintln!(
            "secrets: {key} is now readable as process.env.{key} (and stays in env.{key}); \
             {} secret(s) exposed on {app}",
            next.len()
        ),
        ExposeChange::Remove => eprintln!(
            "secrets: {key} no longer reaches process.env (still readable as env.{key}); \
             {} secret(s) exposed on {app}",
            next.len()
        ),
    }
}

/// Apply `change` to `current`, returning the new list, or `None` when
/// the list would be unchanged (so we can skip a pointless PUT: every
/// PUT bumps `env_version` and reloads workers).
fn merge_expose(current: &[String], key: &str, change: ExposeChange) -> Option<Vec<String>> {
    let present = current.iter().any(|k| k == key);
    let mut next: Vec<String> = match change {
        ExposeChange::Add if present => return None,
        ExposeChange::Add => current.iter().cloned().chain([key.to_string()]).collect(),
        ExposeChange::Remove if !present => return None,
        ExposeChange::Remove => current.iter().filter(|k| *k != key).cloned().collect(),
    };
    next.sort();
    next.dedup();
    Some(next)
}

fn expose_url(control_url: &str, app: &str) -> String {
    format!(
        "{control_url}/api/apps/{}/env/expose",
        encode_path_segment(app)
    )
}

/// GET the current expose list. Any failure (transport, non-2xx, or a
/// response we can't parse) exits. Falling back to "assume empty" here
/// would turn a read error into a silent wipe of the whole list on the
/// PUT that follows.
fn fetch_expose(control_url: &str, app: &str, token: &str) -> Vec<String> {
    let url = expose_url(control_url, app);
    let (status, body) = curl_json("GET", &url, token, None);
    if !(200..300).contains(&status) {
        eprintln!("secrets: reading the expose list failed ({status}): {body}");
        std::process::exit(1);
    }
    parse_expose(&body).unwrap_or_else(|| {
        eprintln!("secrets: could not parse the expose list response: {body}");
        std::process::exit(1);
    })
}

/// `{"expose": ["A", "B"]}` -> `["A", "B"]`. `None` on any other shape.
fn parse_expose(body: &str) -> Option<Vec<String>> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    json.get("expose")?
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn require_valid_key(key: &str) {
    if !valid_key(key) {
        eprintln!("error: invalid KEY {key:?}: {KEY_RULE}");
        std::process::exit(1);
    }
}

fn valid_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.len() <= 64
        && bytes
            .first()
            .is_some_and(u8::is_ascii_uppercase)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
}

fn resource_url(control_url: &str, app: &str, resource: &str, key: Option<&str>) -> String {
    let app = encode_path_segment(app);
    let resource = encode_path_segment(resource);
    let mut url = format!("{control_url}/api/apps/{app}/{resource}");
    if let Some(key) = key {
        url.push('/');
        url.push_str(&encode_path_segment(key));
    }
    url
}

fn encode_path_segment(segment: &str) -> String {
    utf8_percent_encode(segment, PATH_SEGMENT_ENCODE_SET).to_string()
}

/// Run curl with the given method + URL + optional JSON body, returning
/// `(status, body)`. Matches the pattern used by `cmd_deploy`.
fn curl_json(method: &str, url: &str, token: &str, body: Option<&str>) -> (u16, String) {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-s",
        "-w",
        "\n%{http_code}",
        "-X",
        method,
        url,
        "-H",
        &format!("Authorization: Bearer {token}"),
    ]);
    if body.is_some() {
        cmd.args(["-H", "Content-Type: application/json", "--data-binary", "@-"]);
    }
    let out = cmd
        .stdin(if body.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let (Some(b), Some(stdin)) = (body, child.stdin.as_mut()) {
                stdin.write_all(b.as_bytes()).ok();
            }
            child.wait_with_output()
        });

    match out {
        Ok(output) => {
            let s = String::from_utf8_lossy(&output.stdout);
            let (body_out, status_str) = match s.trim().rsplit_once('\n') {
                Some((b, sc)) => (b.to_string(), sc.to_string()),
                None => (String::new(), s.trim().to_string()),
            };
            (status_str.parse().unwrap_or(0), body_out)
        }
        Err(e) => (0, format!("curl spawn error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_validation_matches_control_grammar() {
        assert!(valid_key("A"));
        assert!(valid_key("FOO_123"));
        assert!(valid_key(&format!("A{}", "_".repeat(63))));

        for invalid in [
            "",
            "1FOO",
            "_FOO",
            "Foo",
            "FOO?x",
            "FOO#x",
            "FOO-BAR",
            "É",
        ] {
            assert!(!valid_key(invalid), "accepted invalid key {invalid:?}");
        }
        assert!(!valid_key(&format!("A{}", "_".repeat(64))));
    }

    #[test]
    fn merge_expose_preserves_the_other_names() {
        let current = vec!["ALPHA".to_string(), "CHARLIE".to_string()];
        assert_eq!(
            merge_expose(&current, "BRAVO", ExposeChange::Add),
            Some(vec![
                "ALPHA".to_string(),
                "BRAVO".to_string(),
                "CHARLIE".to_string()
            ])
        );
        assert_eq!(
            merge_expose(&current, "CHARLIE", ExposeChange::Remove),
            Some(vec!["ALPHA".to_string()])
        );
        // Unchanged -> None, so we skip a PUT that would bump env_version
        // and reload every worker for nothing.
        assert_eq!(merge_expose(&current, "ALPHA", ExposeChange::Add), None);
        assert_eq!(merge_expose(&current, "BRAVO", ExposeChange::Remove), None);
        // Removing the last name yields an empty list, not None: the
        // server treats `keys: []` as "clear the list".
        assert_eq!(
            merge_expose(&["ALPHA".to_string()], "ALPHA", ExposeChange::Remove),
            Some(vec![])
        );
    }

    #[test]
    fn parse_expose_rejects_shapes_it_cannot_read() {
        assert_eq!(
            parse_expose(r#"{"expose":["A","B"]}"#),
            Some(vec!["A".to_string(), "B".to_string()])
        );
        assert_eq!(parse_expose(r#"{"expose":[]}"#), Some(vec![]));
        // Anything else must be None so the caller exits rather than
        // treating an unreadable list as empty and wiping it on the PUT.
        for bad in [
            "",
            "not json",
            "{}",
            r#"{"secrets":["A"]}"#,
            r#"{"expose":"A"}"#,
            r#"{"expose":[1]}"#,
            r#"{"expose":[null]}"#,
        ] {
            assert_eq!(parse_expose(bad), None, "accepted bad response {bad:?}");
        }
    }

    #[test]
    fn expose_url_percent_encodes_the_app_segment() {
        assert_eq!(
            expose_url("https://control.example.test", "app/other?x#y"),
            "https://control.example.test/api/apps/app%2Fother%3Fx%23y/env/expose"
        );
    }

    #[test]
    fn positional_skips_flags_written_before_the_key() {
        let args: Vec<String> = ["zeroship", "secret", "set", "--expose", "FOO=bar"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(positional(&args), "FOO=bar");
        assert!(has_flag(&args, "--expose"));
        assert!(!has_flag(&args, "--app"));
    }

    #[test]
    fn url_builder_percent_encodes_each_user_supplied_path_segment() {
        assert_eq!(
            resource_url(
                "https://control.example.test",
                "app/other?x#y",
                "secrets",
                Some("FOO?x#y/%2F")
            ),
            "https://control.example.test/api/apps/app%2Fother%3Fx%23y/secrets/FOO%3Fx%23y%2F%252F"
        );
        assert_eq!(
            resource_url(
                "https://control.example.test",
                "..",
                "secrets",
                Some("FOO")
            ),
            "https://control.example.test/api/apps/%2E%2E/secrets/FOO"
        );
    }
}
