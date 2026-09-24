use super::{Error, Options, RunError};
use futures::StreamExt;
use serde::{Serialize, de::DeserializeOwned};
use std::{io::Write, sync::Arc};
use url::{Host, Url};
use zeroship_core::{
    service_assertion::ServiceIssuer,
    service_identity::ServiceEndpoint,
    service_peers::ServiceAuth,
    workflow_coordination::{Failure, FailureCode, RunFailure},
};

/// Bounded authenticated exchanges shared by native metadata client adapters.
///
/// Public while deployment retention adapters live outside this crate. Callers
/// must validate each receipt against their request; coordinator operations use
/// the typed [`crate::WorkerCoordinator`] and [`crate::ControlCoordinator`] clients.
#[derive(Clone, Debug)]
pub struct Transport {
    base: Url,
    auth: Arc<ServiceAuth>,
    audience: ServiceIssuer,
    client: cyper::Client,
    options: Options,
}

/// Why an origin passed the fence, so the one predicate that decides it is
/// also the one thing that reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    /// HTTPS, or plain HTTP to a literal loopback address.
    Fenced,
    /// Plain HTTP to an origin the operator named.
    NamedPlaintextPeer,
}

impl Transport {
    /// Bind a metadata peer origin, service signer, audience and exchange limits.
    ///
    /// # Errors
    /// Rejects ambiguous origins, empty limits, and plaintext remote peers the
    /// options did not name.
    pub fn new(
        raw: &str,
        auth: Arc<ServiceAuth>,
        audience: ServiceIssuer,
        options: Options,
    ) -> Result<Self, Error> {
        let (base, admission) = Self::configuration(raw, &options)?;
        if admission == Admission::NamedPlaintextPeer {
            // Emitted from the CONSTRUCTOR rather than from each service's
            // `main`, because this is the one place a client that will speak
            // plaintext comes into existence: a process cannot reach a named
            // peer without passing through here, so the posture cannot be
            // active and unreported.
            //
            // Not from the validation path below. That path serves
            // `--check-config`, whose stdout is a machine-read JSON report, and
            // a log line there would be another JSON object on the same stream.
            // The dry run states the posture in its own `plaintext_peers`
            // report field instead.
            tracing::warn!(
                peer = %base.origin().ascii_serialization(),
                "workflow coordination transport admitted a named plaintext peer; \
                 this exchange and its service assertion cross the network in clear"
            );
        }
        Ok(Self {
            base,
            auth,
            options,
            audience,
            client: cyper::Client::new(),
        })
    }

    /// Validate the origin and exchange bounds without keys, sockets or a client.
    ///
    /// # Errors
    /// Rejects the same origin and bounds as [`Self::new`].
    pub fn validate_config(raw: &str, options: &Options) -> Result<(), Error> {
        Self::configuration(raw, options).map(|_| ())
    }

    fn configuration(raw: &str, options: &Options) -> Result<(Url, Admission), Error> {
        let base = Url::parse(raw).map_err(|_| Error::InvalidConfig)?;
        let loopback = match base.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        // THE FENCE. HTTPS always; plain HTTP only to a literal loopback
        // address, or to an exact origin an operator named in
        // `plaintext_peers`. Nothing here resolves a name or classifies an
        // address range, so a name that resolves elsewhere tomorrow cannot move
        // the fence on its own.
        //
        // WHAT THE LIST CANNOT DO. It decides WHICH peer may be reached in
        // clear. It cannot make plaintext safe. Service assertions on these
        // edges are verified under the full profile with a single-use `jti`
        // (`zeroship_core::service_assertion`), so straight replay is closed -
        // but an assertion carries `iss, sub, aud, exp, iat, jti` and NO digest
        // of the request it accompanies. An on-path attacker inside that
        // network therefore reads every exchange in clear and can lift a live
        // assertion onto a MODIFIED body within its window, winning the `jti`
        // race against the legitimate request. That is the full authority of
        // the calling service, once per observed request.
        //
        // WHAT ITS GRANULARITY IS. The list belongs to the PROCESS, not to a
        // role or a setting. Naming an origin so one client can reach it
        // authorizes EVERY client this process builds to speak plaintext to
        // that origin. In compose that is harmless only because the origins
        // happen to be distinct - it is a fact about that deployment, not a
        // property enforced here.
        //
        // So every origin on this list is one whose entire network path is
        // trusted. Internal TLS is the end state this DEFERS, not one it
        // replaces; a reader adding a fourth origin is signing up for the two
        // paragraphs above, for that origin's whole path.
        let plaintext_peer = base.scheme() == "http" && options.plaintext_peers.admits(&base);
        if !(base.scheme() == "https" || (base.scheme() == "http" && loopback) || plaintext_peer)
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
        Ok((
            base,
            if plaintext_peer {
                Admission::NamedPlaintextPeer
            } else {
                Admission::Fenced
            },
        ))
    }

    /// One authenticated exchange, returning its status and bounded body.
    ///
    /// Reading the refusal is the CALLER own, because two contracts share this
    /// transport: coordination refuses with a closed code, and a creator-facing
    /// run call refuses in the engine own terms.
    ///
    /// # Errors
    /// Rejects missing credentials, oversized metadata, failed exchanges,
    /// redirects and oversized responses.
    async fn exchange<T: Serialize>(
        &self,
        endpoint: ServiceEndpoint,
        request: &T,
    ) -> Result<(u16, Vec<u8>), Error> {
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
            Ok((status, bytes))
        };
        compio::time::timeout(self.options.timeout, exchange)
            .await
            .map_err(|_| Error::Timeout)?
    }

    /// Send metadata with a fresh assertion and deserialize a bounded receipt.
    /// The caller binds the receipt to the operation scope and identity.
    ///
    /// # Errors
    /// Rejects failed exchanges and responses outside the closed service error
    /// contract.
    pub async fn post<T: Serialize, R: DeserializeOwned>(
        &self,
        endpoint: ServiceEndpoint,
        request: &T,
    ) -> Result<R, Error> {
        let (status, bytes) = self.exchange(endpoint, request).await?;
        if status != 200 {
            return Err(refusal(status, &bytes));
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::InvalidResponse)
    }

    /// Send a creator-facing run call and deserialize its reply.
    ///
    /// A refusal arrives in the engine own terms rather than as a closed
    /// coordination code, because creator code branches on the code and reads
    /// the message.
    ///
    /// # Errors
    /// Rejects failed exchanges, and a reply whose status and refusal body
    /// disagree about which refusal it is.
    pub(crate) async fn post_run<T: Serialize, R: DeserializeOwned>(
        &self,
        endpoint: ServiceEndpoint,
        request: &T,
    ) -> Result<R, RunError> {
        let (status, bytes) = self
            .exchange(endpoint, request)
            .await
            .map_err(RunError::Transport)?;
        if status != 200 {
            return Err(run_refusal(status, &bytes));
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| RunError::Transport(Error::InvalidResponse))
    }
}

/// A refusal body is believed only when the status it arrived with is the one
/// its own code pairs with. A mismatch is a peer this contract does not
/// describe, not a refusal to hand a creator.
fn run_refusal(status: u16, bytes: &[u8]) -> RunError {
    match serde_json::from_slice::<RunFailure>(bytes) {
        Ok(failure) if failure.status() == status => RunError::Refused(failure),
        _ => RunError::Transport(Error::InvalidResponse),
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
