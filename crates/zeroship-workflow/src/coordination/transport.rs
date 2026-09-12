use super::{Error, Options};
use futures::StreamExt;
use serde::{de::DeserializeOwned, Serialize};
use std::{io::Write, sync::Arc};
use url::{Host, Url};
use zeroship_core::{
    service_assertion::ServiceIssuer,
    service_identity::ServiceEndpoint,
    service_peers::ServiceAuth,
    workflow_coordination::{Failure, FailureCode, AUDIENCE},
};

#[derive(Clone, Debug)]
pub(super) struct Transport {
    base: Url,
    auth: Arc<ServiceAuth>,
    audience: ServiceIssuer,
    client: cyper::Client,
    options: Options,
}

impl Transport {
    pub(super) fn new(raw: &str, auth: Arc<ServiceAuth>, options: Options) -> Result<Self, Error> {
        let base = Url::parse(raw).map_err(|_| Error::InvalidConfig)?;
        let loopback = match base.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if !(base.scheme() == "https" || (base.scheme() == "http" && loopback))
            || base.host().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.path() != "/"
            || base.query().is_some()
            || base.fragment().is_some()
            || options.timeout.is_zero()
            || options.max_request_bytes == 0
            || options.max_response_bytes == 0
        {
            return Err(Error::InvalidConfig);
        }
        Ok(Self {
            base,
            auth,
            options,
            audience: ServiceIssuer::parse(AUDIENCE).map_err(|_| Error::InvalidConfig)?,
            client: cyper::Client::new(),
        })
    }

    pub(super) async fn post<T: Serialize, R: DeserializeOwned>(
        &self,
        endpoint: ServiceEndpoint,
        request: &T,
    ) -> Result<R, Error> {
        let mut body = BoundedBody {
            bytes: Vec::new(),
            limit: self.options.max_request_bytes,
        };
        serde_json::to_writer(&mut body, request).map_err(|_| Error::RequestTooLarge)?;
        let token = self
            .auth
            .authorization_for(&self.audience)
            .ok_or(Error::Unauthenticated)?;
        let mut url = self.base.clone();
        url.set_path(endpoint.path_template());
        let exchange = async {
            // Cyper performs a single exchange; redirect responses are rejected
            // below, without forwarding an assertion to their Location target.
            let response = self
                .client
                .post(url)
                .map_err(|_| Error::InvalidConfig)?
                .header("authorization", token)
                .map_err(|_| Error::Unauthenticated)?
                .header("content-type", "application/json")
                .map_err(|_| Error::InvalidConfig)?
                .header("accept", "application/json")
                .map_err(|_| Error::InvalidConfig)?
                .body(body.bytes)
                .send()
                .await
                .map_err(|_| Error::Unavailable)?;
            let status = response.status().as_u16();
            if response.status().is_redirection() {
                return Err(Error::InvalidResponse);
            }
            if response
                .content_length()
                .is_some_and(|size| size > self.options.max_response_bytes as u64)
            {
                return Err(Error::ResponseTooLarge);
            }
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| Error::Unavailable)?;
                if chunk.len() > self.options.max_response_bytes.saturating_sub(bytes.len()) {
                    return Err(Error::ResponseTooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            if status != 200 {
                return Err(refusal(status, &bytes));
            }
            serde_json::from_slice(&bytes).map_err(|_| Error::InvalidResponse)
        };
        compio::time::timeout(self.options.timeout, exchange)
            .await
            .map_err(|_| Error::Timeout)?
    }
}

fn refusal(status: u16, bytes: &[u8]) -> Error {
    let expected = match status {
        400 => FailureCode::Invalid,
        401 => FailureCode::Unauthenticated,
        403 => FailureCode::Denied,
        409 => FailureCode::Conflict,
        413 => FailureCode::RequestTooLarge,
        429 => FailureCode::Capacity,
        503 => FailureCode::Unavailable,
        500..=599 => return Error::Unavailable,
        _ => return Error::InvalidResponse,
    };
    match serde_json::from_slice::<Failure>(bytes) {
        Ok(failure) if failure.code == expected => Error::Refused(failure.code),
        _ => Error::InvalidResponse,
    }
}

struct BoundedBody {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for BoundedBody {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("metadata request exceeds its bound"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
