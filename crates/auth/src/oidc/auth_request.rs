//! First-class native authorization request context.

use std::fmt;

use crate::return_to;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthRequest {
    pub return_to: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    pub state: Option<String>,
    pub nonce: Option<String>,
    pub prompt: Option<String>,
    pub idp_hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRequestError {
    ReturnToNotSameOrigin,
    ReturnToParse,
    WrongPath,
    MissingClientId,
    MissingRedirectUri,
    InvalidRedirectUri,
}

impl fmt::Display for AuthRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ReturnToNotSameOrigin => "return_to is not a same-origin path",
            Self::ReturnToParse => "return_to parse failed",
            Self::WrongPath => "return_to must target /oauth2/authorize",
            Self::MissingClientId => "missing client_id",
            Self::MissingRedirectUri => "missing redirect_uri",
            Self::InvalidRedirectUri => "redirect_uri is not a valid URL",
        })
    }
}

impl std::error::Error for AuthRequestError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdpHintProvider {
    Google,
    Github,
}

impl IdpHintProvider {
    fn start_path(self) -> &'static str {
        match self {
            Self::Google => "/oauth/google/start",
            Self::Github => "/oauth/github/start",
        }
    }
}

impl AuthRequest {
    pub fn parse_return_to(return_to: &str) -> Result<Self, AuthRequestError> {
        let Some(return_to) = return_to::valid_path(return_to) else {
            return Err(AuthRequestError::ReturnToNotSameOrigin);
        };
        let parsed = url::Url::parse(&format!("http://zeroship.local{return_to}"))
            .map_err(|_| AuthRequestError::ReturnToParse)?;
        if parsed.path() != "/oauth2/authorize" {
            return Err(AuthRequestError::WrongPath);
        }

        let mut client_id = None;
        let mut redirect_uri = None;
        let mut scope = None;
        let mut state = None;
        let mut nonce = None;
        let mut prompt = None;
        let mut idp_hint = None;
        for (key, value) in parsed.query_pairs() {
            match key.as_ref() {
                "client_id" => client_id = Some(value.into_owned()),
                "redirect_uri" => redirect_uri = Some(value.into_owned()),
                "scope" => scope = Some(value.into_owned()),
                "state" => state = Some(value.into_owned()),
                "nonce" => nonce = Some(value.into_owned()),
                "prompt" => prompt = Some(value.into_owned()),
                "idp_hint" => idp_hint = Some(value.into_owned()),
                _ => {}
            }
        }

        Self::from_parts(
            return_to,
            client_id.as_deref(),
            redirect_uri.as_deref(),
            scope.as_deref(),
            state.as_deref(),
            nonce.as_deref(),
            prompt.as_deref(),
            idp_hint.as_deref(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        return_to: &str,
        client_id: Option<&str>,
        redirect_uri: Option<&str>,
        scope: Option<&str>,
        state: Option<&str>,
        nonce: Option<&str>,
        prompt: Option<&str>,
        idp_hint: Option<&str>,
    ) -> Result<Self, AuthRequestError> {
        let client_id = required(client_id, AuthRequestError::MissingClientId)?;
        let redirect_uri = required(redirect_uri, AuthRequestError::MissingRedirectUri)?;
        url::Url::parse(&redirect_uri).map_err(|_| AuthRequestError::InvalidRedirectUri)?;

        Ok(Self {
            return_to: return_to.to_string(),
            client_id,
            redirect_uri,
            scopes: parse_scopes(scope.unwrap_or("")),
            state: drop_empty(state),
            nonce: drop_empty(nonce),
            prompt: clean_optional(prompt),
            idp_hint: clean_optional(idp_hint),
        })
    }

    pub fn provider_start_location(
        &self,
        google_enabled: bool,
        github_enabled: bool,
    ) -> Option<String> {
        let provider = self.idp_hint_provider()?;
        match provider {
            IdpHintProvider::Google if google_enabled => {
                Some(return_to::with_return_to(provider.start_path(), &self.return_to))
            }
            IdpHintProvider::Github if github_enabled => {
                Some(return_to::with_return_to(provider.start_path(), &self.return_to))
            }
            _ => None,
        }
    }

    fn idp_hint_provider(&self) -> Option<IdpHintProvider> {
        match self.idp_hint.as_deref()? {
            hint if hint.eq_ignore_ascii_case("google") => Some(IdpHintProvider::Google),
            hint if hint.eq_ignore_ascii_case("github") => Some(IdpHintProvider::Github),
            _ => None,
        }
    }
}

fn required(value: Option<&str>, err: AuthRequestError) -> Result<String, AuthRequestError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or(err)
}

fn clean_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn drop_empty(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_scopes(scope: &str) -> Vec<String> {
    let mut scopes = scope
        .split_ascii_whitespace()
        .filter(|scope| !scope.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    scopes.sort();
    scopes.dedup();
    scopes
}

#[cfg(test)]
mod tests {
    use super::{AuthRequest, AuthRequestError};

    fn return_to(pairs: &[(&str, &str)]) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in pairs {
            query.append_pair(key, value);
        }
        format!("/oauth2/authorize?{}", query.finish())
    }

    #[test]
    fn valid_query_populates_fields() {
        let request = AuthRequest::parse_return_to(&return_to(&[
            ("client_id", "oac_123"),
            ("redirect_uri", "https://app.zeroship.test/callback"),
            ("scope", "email openid email"),
            ("state", " st-1 "),
            ("nonce", " nc-1 "),
            ("prompt", "login"),
            ("idp_hint", "google"),
        ]))
        .expect("parse auth request");

        assert_eq!(request.client_id, "oac_123");
        assert_eq!(request.redirect_uri, "https://app.zeroship.test/callback");
        assert_eq!(
            request.scopes,
            vec!["email".to_string(), "openid".to_string()]
        );
        assert_eq!(request.state.as_deref(), Some(" st-1 "));
        assert_eq!(request.nonce.as_deref(), Some(" nc-1 "));
        assert_eq!(request.prompt.as_deref(), Some("login"));
        assert_eq!(request.idp_hint.as_deref(), Some("google"));

        let direct = AuthRequest::from_parts(
            "/oauth2/authorize?state=a\\b",
            Some("oac_123"),
            Some("https://app.zeroship.test/callback"),
            Some("openid"),
            Some("a\\b"),
            Some("nc-1"),
            None,
            None,
        )
        .expect("direct authorize params should not validate request target as return_to");
        assert_eq!(direct.return_to, "/oauth2/authorize?state=a\\b");
        assert_eq!(direct.state.as_deref(), Some("a\\b"));
    }

    #[test]
    fn missing_required_param_errors() {
        let err = AuthRequest::parse_return_to(&return_to(&[(
            "redirect_uri",
            "https://app.zeroship.test/callback",
        )]))
        .expect_err("missing client_id must fail");

        assert_eq!(err, AuthRequestError::MissingClientId);
    }

    #[test]
    fn malformed_redirect_uri_errors() {
        let err = AuthRequest::parse_return_to(&return_to(&[
            ("client_id", "oac_123"),
            ("redirect_uri", "not a url"),
        ]))
        .expect_err("malformed redirect_uri must fail");

        assert_eq!(err, AuthRequestError::InvalidRedirectUri);
    }
}
