//! Public durable-workflow signal ingress edge.
//!
//! The gateway rate-limits and forwards only. Token verification and journal
//! writes happen at the control-plane ingress terminus.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ntex::http::StatusCode;
use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};

use crate::GateState;

const CONTROL_SIGNAL_INGRESS_PATH: &str = "/internal/workflows/signals/ingress";
const CONTROL_SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);
pub const SIGNAL_INGRESS_BODY_BYTES: usize = 128 * 1024;
const PLACEHOLDER_RATE_PER_SEC: f64 = 5.0;
const PLACEHOLDER_BURST: f64 = 10.0;

#[derive(Debug, Clone, Copy)]
struct PlaceholderBucket {
    tokens: f64,
    last: Instant,
}

impl PlaceholderBucket {
    fn new(now: Instant) -> Self {
        Self {
            tokens: PLACEHOLDER_BURST,
            last: now,
        }
    }

    fn allow(&mut self, now: Instant) -> bool {
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * PLACEHOLDER_RATE_PER_SEC).min(PLACEHOLDER_BURST);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

static PLACEHOLDER_LIMITER: OnceLock<Mutex<HashMap<String, PlaceholderBucket>>> = OnceLock::new();

fn source_key(req: &HttpRequest) -> String {
    req.peer_addr()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn check_placeholder_rate_limit(req: &HttpRequest) -> bool {
    // G5 placeholder — operator-pending durable rate-limit store.
    let limiter = PLACEHOLDER_LIMITER.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    let mut guard = limiter.lock().expect("signal ingress limiter lock poisoned");
    guard
        .entry(source_key(req))
        .or_insert_with(|| PlaceholderBucket::new(now))
        .allow(now)
}

pub async fn public_signal_ingress(
    req: HttpRequest,
    state: web::types::State<std::sync::Arc<GateState>>,
    body: Bytes,
) -> HttpResponse {
    if req.method() != ntex::http::Method::POST {
        return HttpResponse::NotFound().finish();
    }
    if !check_placeholder_rate_limit(&req) {
        return HttpResponse::TooManyRequests()
            .json(&serde_json::json!({"error": "rate limit exceeded"}));
    }
    if state.config.control_url.trim().is_empty() {
        return HttpResponse::ServiceUnavailable()
            .json(&serde_json::json!({"error": "control unavailable"}));
    }
    match forward_to_control(&state, body.as_ref()).await {
        Ok((status, response_body)) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            HttpResponse::build(status)
                .header("content-type", "application/json")
                .body(response_body)
        }
        Err(e) => {
            tracing::warn!(error = %e, "signal ingress: control forward failed");
            HttpResponse::BadGateway().json(&serde_json::json!({"error": "control unavailable"}))
        }
    }
}

async fn forward_to_control(state: &GateState, body: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let url = format!(
        "{}{}",
        state.config.control_url.trim_end_matches('/'),
        CONTROL_SIGNAL_INGRESS_PATH
    );
    let client = cyper::Client::new();
    let request = client
        .post(&url)
        .map_err(|e| format!("build control request: {e}"))?
        .header("content-type", "application/json")
        .map_err(|e| format!("set content-type: {e}"))?
        .header(
            "authorization",
            &format!("Bearer {}", state.config.control_key),
        )
        .map_err(|e| format!("set authorization: {e}"))?
        .body(body.to_vec())
        .send();
    let response = compio::time::timeout(CONTROL_SIGNAL_TIMEOUT, request)
        .await
        .map_err(|_| "control request timeout".to_string())?
        .map_err(|e| format!("control transport: {e}"))?;
    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("read control response: {e}"))?;
    Ok((status, bytes.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_bucket_enforces_burst() {
        let now = Instant::now();
        let mut bucket = PlaceholderBucket::new(now);
        for _ in 0..PLACEHOLDER_BURST as usize {
            assert!(bucket.allow(now));
        }
        assert!(!bucket.allow(now));
        assert!(bucket.allow(now + Duration::from_secs(1)));
    }
}
