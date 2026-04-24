//! `zeroship secret` / `zeroship var` subcommands.
//!
//! Shape:
//!   zeroship secret set  KEY=value   --app=<uuid> [--control=URL] [--key=MASTER]
//!   zeroship secret list             --app=<uuid> [...]
//!   zeroship secret rm   KEY         --app=<uuid> [...]
//!   zeroship var    set  KEY=value   --app=<uuid> [...]
//!   zeroship var    list             --app=<uuid>
//!   zeroship var    rm   KEY         --app=<uuid>
//!
//! Uses curl for HTTP (matches `cmd_deploy`'s pattern). Returns exit
//! code 0 on success, 1 on any error.

use std::process::{Command, Stdio};

use crate::flag_str;

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
                "Usage:\n  zeroship {s} set  KEY=value --app=<uuid>\n  zeroship {s} list          --app=<uuid>\n  zeroship {s} rm   KEY       --app=<uuid>",
                s = if resource == "secrets" { "secret" } else { "var" },
            );
            std::process::exit(1);
        }
    }
}

fn common(args: &[String]) -> (String, String, String) {
    let app = flag_str(args, "--app=").expect("--app=<uuid> is required");
    let control_url = flag_str(args, "--control=")
        .or_else(|| std::env::var("ZEROSHIP_CONTROL_URL").ok())
        .unwrap_or_else(|| "http://localhost:9090".into());
    let master_key = flag_str(args, "--key=")
        .or_else(|| std::env::var("ZEROSHIP_MASTER_KEY").ok())
        .unwrap_or_default();
    (app, control_url, master_key)
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
    let (app, control_url, master_key) = common(args);

    let url = format!("{control_url}/api/apps/{app}/{resource}");
    let body = serde_json::json!({ "key": key, "value": value }).to_string();

    let (status, body_out) = curl_json("POST", &url, &master_key, Some(&body));
    if (200..300).contains(&status) {
        eprintln!("{resource}: set {key} on {app}");
    } else {
        eprintln!("{resource}: set failed ({status}): {body_out}");
        std::process::exit(1);
    }
}

fn cmd_list(resource: &str, args: &[String]) {
    let (app, control_url, master_key) = common(args);
    let url = format!("{control_url}/api/apps/{app}/{resource}");
    let (status, body) = curl_json("GET", &url, &master_key, None);
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
        eprintln!("error: missing KEY (e.g. `zeroship {} rm FOO --app=<uuid>`)",
            if resource == "secrets" { "secret" } else { "var" });
        std::process::exit(1);
    }
    let (app, control_url, master_key) = common(args);
    let url = format!("{control_url}/api/apps/{app}/{resource}/{key}");
    let (status, body) = curl_json("DELETE", &url, &master_key, None);
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

/// Run curl with the given method + URL + optional JSON body, returning
/// `(status, body)`. Matches the pattern used by `cmd_deploy`.
fn curl_json(method: &str, url: &str, master_key: &str, body: Option<&str>) -> (u16, String) {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-s",
        "-w",
        "\n%{http_code}",
        "-X",
        method,
        url,
        "-H",
        &format!("Authorization: Bearer {master_key}"),
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
