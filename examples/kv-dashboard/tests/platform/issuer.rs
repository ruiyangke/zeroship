use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use rand::rngs::OsRng;
use serde_json::json;
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};

/// A test identity provider's public keys; control still verifies real tokens.
pub struct Issuer {
    pub url: String,
    key: SigningKey,
    _server: Container<GenericImage>,
}

impl Issuer {
    pub fn start() -> Self {
        let key = SigningKey::generate(&mut OsRng);
        let jwks = json!({"keys": [{
            "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig",
            "kid": "kv-acceptance", "x": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())
        }]});
        let server = GenericImage::new("nginx", "1")
            .with_exposed_port(80.tcp())
            .with_wait_for(WaitFor::message_on_stderr("start worker processes"))
            .with_copy_to(
                "/usr/share/nginx/html/.well-known/jwks.json",
                serde_json::to_vec(&jwks).unwrap(),
            )
            .start()
            .expect("KV deployment tests require Docker to serve the issuer's JWKS");
        let url = format!(
            "http://{}:{}",
            server.get_host().unwrap(),
            server.get_host_port_ipv4(80).unwrap()
        );
        Self {
            url,
            key,
            _server: server,
        }
    }

    pub fn bearer(&self, owner: uuid::Uuid) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let header = json!({"alg": "EdDSA", "typ": "at+jwt", "kid": "kv-acceptance"});
        let claims = json!({
            "iss": self.url, "aud": "control.zeroship.ai", "sub": owner,
            "iat": now, "nbf": now - 1, "exp": now + 3600,
            "client_id": "zeroship-console", "jti": uuid::Uuid::new_v4(),
            "scope": "organization:create apps:read apps:write apps:deploy deployments:read secrets:read"
        });
        let body = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        format!(
            "{body}.{}",
            URL_SAFE_NO_PAD.encode(self.key.sign(body.as_bytes()).to_bytes())
        )
    }
}
