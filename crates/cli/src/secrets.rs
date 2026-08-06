//! `zeroship secret` / `zeroship var` subcommands.
//!
//! Shape:
//!   zeroship secret set  KEY=value   --app=<uuid> [--control=URL] [--token=PAT]
//!   zeroship secret list             --app=<uuid> [...]
//!   zeroship secret rm   KEY         --app=<uuid> [...]
//!   zeroship var    set  KEY=value   --app=<uuid> [...]
//!   zeroship var    list             --app=<uuid>
//!   zeroship var    rm   KEY         --app=<uuid>
//!
//! Uses curl for HTTP (matches `cmd_deploy`'s pattern). Returns exit
//! code 0 on success, 1 on any error.

use std::process::{Command, Stdio};

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

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
        _ => {
            eprintln!(
                concat!(
                    "Usage:\n",
                    "  zeroship {s} set  KEY=value --app=<uuid> [--control=URL] [--token=PAT]\n",
                    "  zeroship {s} list          --app=<uuid> [--control=URL] [--token=PAT]\n",
                    "  zeroship {s} rm   KEY       --app=<uuid> [--control=URL] [--token=PAT]"
                ),
                s = if resource == "secrets" { "secret" } else { "var" },
            );
            std::process::exit(1);
        }
    }
}

fn common(resource: &str, args: &[String]) -> (String, String, String) {
    let app = flag_str(args, "--app=").expect("--app=<uuid> is required");
    let control_url = flag_str(args, "--control=")
        .or_else(|| std::env::var("ZEROSHIP_CONTROL_URL").ok())
        .unwrap_or_else(|| "http://localhost:9090".into());
    let token = resolve_bearer_token(args).unwrap_or_else(|e| {
        eprintln!(
            "zeroship {}: {e}",
            if resource == "secrets" { "secret" } else { "var" }
        );
        std::process::exit(1);
    });
    (app, control_url, token)
}

fn cmd_set(resource: &str, args: &[String]) {
    // Positional arg 3 is `KEY=value`. We split on the first `=` so values
    // containing `=` themselves (e.g. base64 tokens) work correctly.
    let pair = args.get(3).cloned().unwrap_or_default();
    let (key, value) = match pair.split_once('=') {
        Some(p) => p,
        None => {
            eprintln!("error: expected KEY=value (e.g. STRIPE_KEY=sk_test_...)");
            std::process::exit(1);
        }
    };
    require_valid_key(key);
    let (app, control_url, token) = common(resource, args);

    let url = resource_url(&control_url, &app, resource, None);
    let body = serde_json::json!({ "key": key, "value": value }).to_string();

    let (status, body_out) = curl_json("POST", &url, &token, Some(&body));
    if (200..300).contains(&status) {
        eprintln!("{resource}: set {key} on {app}");
    } else {
        eprintln!("{resource}: set failed ({status}): {body_out}");
        std::process::exit(1);
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
    let key = args.get(3).cloned().unwrap_or_default();
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
