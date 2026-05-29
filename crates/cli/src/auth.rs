//! OAuth Device Authorization Grant support for the CLI.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const CLIENT_ID: &str = "zeroship-cli";
const DEFAULT_AUTH_URL: &str = "https://auth.zeroship.ai";
const SCOPE: &str = "openid offline_access apps:deploy apps:read";
const TOKEN_EXPIRY_SKEW_SECS: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub auth_url: String,
    pub client_id: String,
}

#[derive(Debug, Deserialize)]
struct DeviceAuthResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<u64>,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    error: String,
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserInfo {
    email: Option<String>,
    sub: Option<String>,
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: String,
}

pub fn cmd_login(args: &[String]) -> Result<(), String> {
    let auth_url = crate::flag_str(args, "--auth-url=")
        .or_else(|| flag_value(args, "--auth-url"))
        .unwrap_or_else(|| DEFAULT_AUTH_URL.into());
    login(&auth_url, true)
}

pub fn cmd_logout() -> Result<(), String> {
    let creds = read_credentials()?;
    let revoke_url = endpoint(&creds.auth_url, "/oauth2/revoke");
    let resp = post_form(
        &revoke_url,
        &[
            ("token", creds.refresh_token.as_str()),
            ("token_type_hint", "refresh_token"),
            ("client_id", creds.client_id.as_str()),
        ],
    )?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "revoke failed (HTTP {}): {}",
            resp.status, resp.body
        ));
    }

    let path = credentials_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => {
            println!("Signed out");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("Signed out");
            Ok(())
        }
        Err(e) => Err(format!("delete {}: {e}", path.display())),
    }
}

pub fn cmd_whoami() -> Result<(), String> {
    let creds = load_credentials()?;
    let user = userinfo(&creds)?;
    let identity = user.email.or(user.sub).unwrap_or_else(|| "<unknown>".into());
    println!("{identity}");
    println!("Token expires at {}", creds.expires_at);
    Ok(())
}

pub fn load_credentials() -> Result<Credentials, String> {
    let mut creds = read_credentials()?;
    let now = now_secs()?;
    if creds.expires_at > now.saturating_add(TOKEN_EXPIRY_SKEW_SECS) {
        return Ok(creds);
    }

    let token_url = endpoint(&creds.auth_url, "/oauth2/token");
    let resp = post_form(
        &token_url,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", creds.refresh_token.as_str()),
            ("client_id", creds.client_id.as_str()),
        ],
    )?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "refresh failed (HTTP {}): {}",
            resp.status, resp.body
        ));
    }

    let token: TokenResponse = serde_json::from_str(&resp.body)
        .map_err(|e| format!("parse refresh token response: {e}"))?;
    creds.access_token = token.access_token;
    if let Some(refresh_token) = token.refresh_token {
        creds.refresh_token = refresh_token;
    }
    creds.expires_at = now_secs()?.saturating_add(token.expires_in.unwrap_or(3600));
    save_credentials(&creds)?;
    Ok(creds)
}

fn login(auth_url: &str, print_prompt: bool) -> Result<(), String> {
    let auth_url = auth_url.trim_end_matches('/');
    let device_url = endpoint(auth_url, "/oauth2/device/auth");
    let resp = post_form(
        &device_url,
        &[("client_id", CLIENT_ID), ("scope", SCOPE)],
    )?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "device authorization failed (HTTP {}): {}",
            resp.status, resp.body
        ));
    }
    let device: DeviceAuthResponse = serde_json::from_str(&resp.body)
        .map_err(|e| format!("parse device authorization response: {e}"))?;
    let interval = device.interval.unwrap_or(5).max(1);

    if print_prompt {
        eprintln!("To sign in:");
        eprintln!("  1. Open: {}", device.verification_uri);
        eprintln!("  2. Enter code: {}", device.user_code);
        eprintln!();
        eprintln!("Waiting for approval...");
    }

    let token = poll_for_token(auth_url, &device, interval)?;
    let expires_at = now_secs()?.saturating_add(token.expires_in.unwrap_or(3600));
    let creds = Credentials {
        access_token: token.access_token,
        refresh_token: token
            .refresh_token
            .ok_or_else(|| "token response did not include refresh_token".to_string())?,
        expires_at,
        auth_url: auth_url.to_string(),
        client_id: CLIENT_ID.to_string(),
    };
    save_credentials(&creds)?;

    let user = userinfo(&creds)?;
    let identity = user.email.or(user.sub).unwrap_or_else(|| "<unknown>".into());
    println!("Signed in as {identity}");
    Ok(())
}

fn poll_for_token(
    auth_url: &str,
    device: &DeviceAuthResponse,
    initial_interval: u64,
) -> Result<TokenResponse, String> {
    let token_url = endpoint(auth_url, "/oauth2/token");
    let deadline = now_secs()?.saturating_add(device.expires_in);
    let mut interval = initial_interval;

    loop {
        let resp = post_form(
            &token_url,
            &[
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:device_code",
                ),
                ("device_code", device.device_code.as_str()),
                ("client_id", CLIENT_ID),
            ],
        )?;
        if (200..300).contains(&resp.status) {
            return serde_json::from_str(&resp.body)
                .map_err(|e| format!("parse token response: {e}"));
        }

        let err = serde_json::from_str::<TokenErrorResponse>(&resp.body).ok();
        let kind = err.as_ref().map(|e| e.error.clone());
        match kind.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval = interval.saturating_add(5),
            Some("expired_token") => return Err("device code expired".into()),
            Some("access_denied") => return Err("device authorization denied".into()),
            Some(other) => {
                let description = err
                    .and_then(|e| e.error_description)
                    .map(|s| format!(": {s}"))
                    .unwrap_or_default();
                return Err(format!("token polling failed: {other}{description}"));
            }
            None => {
                return Err(format!(
                    "token polling failed (HTTP {}): {}",
                    resp.status, resp.body
                ));
            }
        }

        if now_secs()? >= deadline {
            return Err("device authorization timed out".into());
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}

fn userinfo(creds: &Credentials) -> Result<UserInfo, String> {
    let url = endpoint(&creds.auth_url, "/userinfo");
    let resp = get_bearer(&url, &creds.access_token)?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("userinfo failed (HTTP {}): {}", resp.status, resp.body));
    }
    serde_json::from_str(&resp.body).map_err(|e| format!("parse userinfo response: {e}"))
}

fn save_credentials(creds: &Credentials) -> Result<(), String> {
    let path = credentials_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid credentials path {}", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;

    let body = serde_json::to_vec_pretty(creds).map_err(|e| format!("serialize credentials: {e}"))?;
    write_private_file(&path, &body)
}

fn read_credentials() -> Result<Credentials, String> {
    let path = credentials_path()?;
    let body = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("not signed in; run `zeroship login`")
        } else {
            format!("read {}: {e}", path.display())
        }
    })?;
    serde_json::from_str(&body).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn credentials_path() -> Result<PathBuf, String> {
    let base = if let Some(path) = std::env::var_os("ZEROSHIP_CONFIG_HOME") {
        PathBuf::from(path)
    } else if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(path)
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        return Err("HOME is not set".into());
    };
    Ok(base.join("zeroship").join("token.json"))
}

#[cfg(unix)]
fn write_private_file(path: &std::path::Path, body: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    file.write_all(body)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod 0600 {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn write_private_file(path: &std::path::Path, body: &[u8]) -> Result<(), String> {
    std::fs::write(path, body).map_err(|e| format!("write {}: {e}", path.display()))
}

fn post_form(url: &str, pairs: &[(&str, &str)]) -> Result<HttpResponse, String> {
    let body = form_body(pairs);
    let output = Command::new("curl")
        .args([
            "-sS",
            "-w",
            "\n%{http_code}",
            "-X",
            "POST",
            url,
            "-H",
            "Content-Type: application/x-www-form-urlencoded",
            "--data-binary",
            "@-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(body.as_bytes()).ok();
            }
            child.wait_with_output()
        })
        .map_err(|e| format!("curl spawn error: {e}"))?;
    parse_curl_output(output)
}

fn get_bearer(url: &str, token: &str) -> Result<HttpResponse, String> {
    let output = Command::new("curl")
        .args([
            "-sS",
            "-w",
            "\n%{http_code}",
            "-H",
            &format!("Authorization: Bearer {token}"),
            url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("curl spawn error: {e}"))?;
    parse_curl_output(output)
}

fn parse_curl_output(output: std::process::Output) -> Result<HttpResponse, String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("curl exited with {}", output.status)
        } else {
            stderr
        });
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let (body, status) = text
        .trim_end()
        .rsplit_once('\n')
        .ok_or_else(|| "curl output did not include HTTP status".to_string())?;
    Ok(HttpResponse {
        status: status
            .parse()
            .map_err(|e| format!("parse HTTP status '{status}': {e}"))?,
        body: body.to_string(),
    })
}

fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        form.append_pair(key, value);
    }
    form.finish()
}

fn endpoint(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

fn now_secs() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| format!("system clock before UNIX epoch: {e}"))
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_grant_request_shape() {
        let body = form_body(&[("client_id", CLIENT_ID), ("scope", SCOPE)]);
        assert_eq!(
            body,
            "client_id=zeroship-cli&scope=openid+offline_access+apps%3Adeploy+apps%3Aread"
        );
    }
}
