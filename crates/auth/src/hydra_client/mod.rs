//! Hand-rolled hydra admin API client over `cyper` (compio HTTP).
//!
//! Hydra's auto-generated `ory-hydra-client` crate pulls in reqwest+tokio
//! which conflicts with the zero-tokio invariant. We hand-roll a small
//! cyper-based client. The cyper invocation idiom mirrors
//! `crates/control/src/oauth.rs`.

pub mod types;

use serde::{de::DeserializeOwned, Serialize};

use crate::error::{AuthError, Result};

#[derive(Clone)]
pub struct HydraAdmin {
    base: String,
    client: cyper::Client,
}

impl std::fmt::Debug for HydraAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HydraAdmin").field("base", &self.base).finish()
    }
}

impl HydraAdmin {
    pub fn new(base: impl Into<String>) -> Self {
        Self { base: base.into(), client: cyper::Client::new() }
    }

    pub(crate) fn url(&self, path: &str) -> String {
        let trimmed = self.base.trim_end_matches('/');
        format!("{trimmed}{path}")
    }

    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let mut url = self.url(path);
        if !query.is_empty() {
            let q = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(query.iter().copied())
                .finish();
            url.push('?');
            url.push_str(&q);
        }
        let res = self.client
            .request(http::Method::GET, url)
            .map_err(|e| AuthError::Hydra(format!("build GET {path}: {e}")))?
            .send().await
            .map_err(|e| AuthError::Hydra(format!("GET {path}: {e}")))?;
        finish::<T>(res, path).await
    }

    pub(crate) async fn put<B: Serialize, T: DeserializeOwned>(
        &self, path: &str, query: &[(&str, &str)], body: &B,
    ) -> Result<T> {
        let mut url = self.url(path);
        if !query.is_empty() {
            let q = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(query.iter().copied())
                .finish();
            url.push('?');
            url.push_str(&q);
        }
        let body_bytes = serde_json::to_vec(body)
            .map_err(|e| AuthError::Hydra(format!("PUT {path} encode: {e}")))?;
        let res = self.client
            .request(http::Method::PUT, url)
            .map_err(|e| AuthError::Hydra(format!("build PUT {path}: {e}")))?
            .header("content-type", "application/json")
            .map_err(|e| AuthError::Hydra(format!("PUT {path} header: {e}")))?
            .body(body_bytes)
            .send().await
            .map_err(|e| AuthError::Hydra(format!("PUT {path}: {e}")))?;
        finish::<T>(res, path).await
    }

    pub(crate) async fn post<B: Serialize, T: DeserializeOwned>(
        &self, path: &str, body: &B,
    ) -> Result<T> {
        let url = self.url(path);
        let body_bytes = serde_json::to_vec(body)
            .map_err(|e| AuthError::Hydra(format!("POST {path} encode: {e}")))?;
        let res = self.client
            .request(http::Method::POST, url)
            .map_err(|e| AuthError::Hydra(format!("build POST {path}: {e}")))?
            .header("content-type", "application/json")
            .map_err(|e| AuthError::Hydra(format!("POST {path} header: {e}")))?
            .body(body_bytes)
            .send().await
            .map_err(|e| AuthError::Hydra(format!("POST {path}: {e}")))?;
        finish::<T>(res, path).await
    }

    pub(crate) async fn delete(&self, path: &str, query: &[(&str, &str)]) -> Result<()> {
        let mut url = self.url(path);
        if !query.is_empty() {
            let q = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(query.iter().copied())
                .finish();
            url.push('?');
            url.push_str(&q);
        }
        let res = self.client
            .request(http::Method::DELETE, url)
            .map_err(|e| AuthError::Hydra(format!("build DELETE {path}: {e}")))?
            .send().await
            .map_err(|e| AuthError::Hydra(format!("DELETE {path}: {e}")))?;
        let status = res.status().as_u16();
        if !(200..300).contains(&status) {
            let body = res.text().await.unwrap_or_else(|_| "<no body>".into());
            return Err(AuthError::Hydra(format!("DELETE {path} → {status}: {body}")));
        }
        Ok(())
    }
}

async fn finish<T: DeserializeOwned>(res: cyper::Response, path: &str) -> Result<T> {
    let status = res.status().as_u16();
    let body = res.text().await
        .map_err(|e| AuthError::Hydra(format!("read body {path}: {e}")))?;
    if !(200..300).contains(&status) {
        return Err(AuthError::Hydra(format!("{path} → {status}: {body}")));
    }
    serde_json::from_str(&body)
        .map_err(|e| AuthError::Hydra(format!("decode {path}: {e}\nbody: {body}")))
}
