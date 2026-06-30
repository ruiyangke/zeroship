//! OAuth Device Authorization Grant support for the CLI.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const CLIENT_ID: &str = "zeroship-cli";
const DEFAULT_AUTH_URL: &str = "https://auth.zeroship.ai";
const DEFAULT_CONTROL_URL: &str = "http://localhost:9090";
const SCOPE: &str = "openid offline_access apps:deploy apps:read";
const TOKEN_EXPIRY_SKEW_SECS: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub auth_url: String,
    pub client_id: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub control_url: Option<String>,
    #[serde(default)]
    pub token_endpoint: Option<String>,
    #[serde(default)]
    pub anon_key: Option<String>,
    #[serde(default)]
    pub userinfo_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceAuthResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    interval: Option<u64>,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    auth_url: Option<String>,
    #[serde(default)]
    token_endpoint: Option<String>,
    #[serde(default)]
    anon_key: Option<String>,
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
    id: Option<String>,
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliDeviceFlow {
    Hydra,
    Supabase,
}

pub fn cmd_login(args: &[String]) -> Result<(), String> {
    let provider = parse_provider(args)?;
    let auth_url = crate::flag_str(args, "--auth-url=")
        .or_else(|| flag_value(args, "--auth-url"))
        .unwrap_or_else(|| DEFAULT_AUTH_URL.into());
    match provider {
        CliDeviceFlow::Hydra => login_hydra(&auth_url, true),
        CliDeviceFlow::Supabase => {
            let control_url = crate::flag_str(args, "--control=")
                .or_else(|| flag_value(args, "--control"))
                .or_else(|| std::env::var("ZEROSHIP_CONTROL_URL").ok())
                .unwrap_or_else(|| DEFAULT_CONTROL_URL.into());
            login_supabase(&control_url, true)
        }
    }
}

pub fn cmd_logout() -> Result<(), String> {
    let creds = read_credentials()?;
    if credential_provider(&creds)? == CliDeviceFlow::Hydra {
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

    let token = match credential_provider(&creds)? {
        CliDeviceFlow::Hydra => refresh_hydra(&creds)?,
        CliDeviceFlow::Supabase => refresh_supabase_credentials(&creds)?,
    };
    creds.access_token = require_access_token(token.access_token, "refresh token response")?;
    if let Some(refresh_token) = token.refresh_token {
        creds.refresh_token = refresh_token;
    }
    creds.expires_at = now_secs()?.saturating_add(token.expires_in.unwrap_or(3600));
    save_credentials(&creds)?;
    Ok(creds)
}

fn login_hydra(auth_url: &str, print_prompt: bool) -> Result<(), String> {
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
        access_token: require_access_token(token.access_token, "token response")?,
        refresh_token: token
            .refresh_token
            .ok_or_else(|| "token response did not include refresh_token".to_string())?,
        expires_at,
        auth_url: auth_url.to_string(),
        client_id: CLIENT_ID.to_string(),
        provider: "hydra".to_string(),
        control_url: None,
        token_endpoint: None,
        anon_key: None,
        userinfo_url: None,
    };
    save_credentials(&creds)?;

    let user = userinfo(&creds)?;
    let identity = user.email.or(user.sub).unwrap_or_else(|| "<unknown>".into());
    println!("Signed in as {identity}");
    Ok(())
}

fn login_supabase(control_url: &str, print_prompt: bool) -> Result<(), String> {
    let control_url = control_url.trim_end_matches('/');
    let device_url = endpoint(control_url, "/api/device/auth");
    let resp = post_json(
        &device_url,
        &serde_json::json!({
            "client_id": CLIENT_ID,
            "scope": SCOPE,
        }),
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
        eprintln!(
            "  1. Open: {}",
            device
                .verification_uri_complete
                .as_deref()
                .unwrap_or(device.verification_uri.as_str())
        );
        eprintln!("  2. Enter code: {}", device.user_code);
        eprintln!();
        eprintln!("Waiting for approval...");
    }

    let bound = poll_for_supabase_token(control_url, &device, interval)?;
    if bound.provider.as_deref() != Some("supabase") {
        return Err("device token response did not name provider=supabase".to_string());
    }
    let refresh_token = bound
        .refresh_token
        .ok_or_else(|| "device token response did not include refresh_token".to_string())?;
    let auth_url = bound
        .auth_url
        .ok_or_else(|| "device token response did not include auth_url".to_string())?;
    let token_endpoint = bound
        .token_endpoint
        .ok_or_else(|| "device token response did not include token_endpoint".to_string())?;
    let anon_key = bound
        .anon_key
        .ok_or_else(|| "device token response did not include anon_key".to_string())?;

    let refreshed = refresh_supabase(&token_endpoint, &anon_key, &refresh_token)?;
    let access_token = require_access_token(refreshed.access_token, "Supabase refresh response")?;
    let refresh_token = refreshed.refresh_token.unwrap_or(refresh_token);
    let expires_at = now_secs()?.saturating_add(refreshed.expires_in.unwrap_or(3600));
    let creds = Credentials {
        access_token,
        refresh_token,
        expires_at,
        auth_url: auth_url.trim_end_matches('/').to_string(),
        client_id: CLIENT_ID.to_string(),
        provider: "supabase".to_string(),
        control_url: Some(control_url.to_string()),
        token_endpoint: Some(token_endpoint),
        anon_key: Some(anon_key),
        userinfo_url: None,
    };
    save_credentials(&creds)?;

    let user = userinfo(&creds)?;
    let identity = user
        .email
        .or(user.sub)
        .or(user.id)
        .unwrap_or_else(|| "<unknown>".into());
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

fn poll_for_supabase_token(
    control_url: &str,
    device: &DeviceAuthResponse,
    initial_interval: u64,
) -> Result<TokenResponse, String> {
    let token_url = endpoint(control_url, "/api/device/token");
    let deadline = now_secs()?.saturating_add(device.expires_in);
    let mut interval = initial_interval;

    loop {
        let resp = post_json(
            &token_url,
            &serde_json::json!({
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                "device_code": device.device_code.as_str(),
            }),
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
    let (url, headers) = match credential_provider(creds)? {
        CliDeviceFlow::Hydra => (endpoint(&creds.auth_url, "/userinfo"), Vec::new()),
        CliDeviceFlow::Supabase => {
            let anon_key = creds
                .anon_key
                .as_deref()
                .ok_or_else(|| "saved Supabase credentials are missing anon_key".to_string())?;
            let url = creds
                .userinfo_url
                .clone()
                .unwrap_or_else(|| endpoint(&creds.auth_url, "/auth/v1/user"));
            (url, vec![("apikey", anon_key)])
        }
    };
    let resp = get_bearer_with_headers(&url, &creds.access_token, &headers)?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("userinfo failed (HTTP {}): {}", resp.status, resp.body));
    }
    serde_json::from_str(&resp.body).map_err(|e| format!("parse userinfo response: {e}"))
}

fn refresh_hydra(creds: &Credentials) -> Result<TokenResponse, String> {
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

    serde_json::from_str(&resp.body).map_err(|e| format!("parse refresh token response: {e}"))
}

fn refresh_supabase_credentials(creds: &Credentials) -> Result<TokenResponse, String> {
    let token_endpoint = creds
        .token_endpoint
        .as_deref()
        .ok_or_else(|| "saved Supabase credentials are missing token_endpoint".to_string())?;
    let anon_key = creds
        .anon_key
        .as_deref()
        .ok_or_else(|| "saved Supabase credentials are missing anon_key".to_string())?;
    refresh_supabase(token_endpoint, anon_key, &creds.refresh_token)
}

fn refresh_supabase(
    token_endpoint: &str,
    anon_key: &str,
    refresh_token: &str,
) -> Result<TokenResponse, String> {
    let resp = post_form_with_headers(
        token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
        &[("apikey", anon_key)],
    )?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "refresh failed (HTTP {}): {}",
            resp.status, resp.body
        ));
    }

    serde_json::from_str(&resp.body).map_err(|e| format!("parse refresh token response: {e}"))
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

fn parse_provider(args: &[String]) -> Result<CliDeviceFlow, String> {
    let provider = crate::flag_str(args, "--provider=")
        .or_else(|| flag_value(args, "--provider"))
        .unwrap_or_else(default_provider);
    parse_provider_value(&provider)
}

fn credential_provider(creds: &Credentials) -> Result<CliDeviceFlow, String> {
    parse_provider_value(&creds.provider)
}

fn parse_provider_value(provider: &str) -> Result<CliDeviceFlow, String> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "" | "hydra" => Ok(CliDeviceFlow::Hydra),
        "supabase" => Ok(CliDeviceFlow::Supabase),
        other => Err(format!("unknown auth provider {other:?}; expected hydra|supabase")),
    }
}

fn default_provider() -> String {
    "hydra".to_string()
}

fn require_access_token(value: Option<String>, context: &str) -> Result<String, String> {
    value.ok_or_else(|| format!("{context} did not include access_token"))
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
    post_form_with_headers(url, pairs, &[])
}

fn post_form_with_headers(
    url: &str,
    pairs: &[(&str, &str)],
    headers: &[(&str, &str)],
) -> Result<HttpResponse, String> {
    let body = form_body(pairs);
    let mut args = vec![
        "-sS".to_string(),
        "-w".to_string(),
        "\n%{http_code}".to_string(),
        "-X".to_string(),
        "POST".to_string(),
        url.to_string(),
        "-H".to_string(),
        "Content-Type: application/x-www-form-urlencoded".to_string(),
    ];
    for (name, value) in headers {
        args.push("-H".to_string());
        args.push(format!("{name}: {value}"));
    }
    args.push("--data-binary".to_string());
    args.push("@-".to_string());
    let output = Command::new("curl")
        .args(args)
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

fn get_bearer_with_headers(
    url: &str,
    token: &str,
    headers: &[(&str, &str)],
) -> Result<HttpResponse, String> {
    let mut args = vec![
        "-sS".to_string(),
        "-w".to_string(),
        "\n%{http_code}".to_string(),
        "-H".to_string(),
        format!("Authorization: Bearer {token}"),
        url.to_string(),
    ];
    for (name, value) in headers {
        args.insert(args.len() - 1, "-H".to_string());
        args.insert(args.len() - 1, format!("{name}: {value}"));
    }
    let output = Command::new("curl")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("curl spawn error: {e}"))?;
    parse_curl_output(output)
}

fn post_json(url: &str, value: &serde_json::Value) -> Result<HttpResponse, String> {
    let body = serde_json::to_string(value).map_err(|e| format!("serialize JSON body: {e}"))?;
    let output = Command::new("curl")
        .args([
            "-sS",
            "-w",
            "\n%{http_code}",
            "-X",
            "POST",
            url,
            "-H",
            "Content-Type: application/json",
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
