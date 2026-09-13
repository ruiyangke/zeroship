use super::*;
use std::collections::BTreeMap;

pub struct Login {
    pub code: String,
    pub state: String,
    pub idp_session: String,
}

pub fn query(url: &url::Url) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    for (key, value) in url.query_pairs() {
        assert!(
            params
                .insert(key.into_owned(), value.into_owned())
                .is_none(),
            "duplicate URL parameter"
        );
    }
    params
}

fn location(response: &cyper::Response, base: &str) -> url::Url {
    let location = response
        .headers()
        .get(http::header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    url::Url::parse(base).unwrap().join(location).unwrap()
}

fn cookie(response: &cyper::Response, name: &str) -> String {
    response
        .headers()
        .get_all(http::header::SET_COOKIE)
        .iter()
        .find_map(|value| {
            let (key, value) = value
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .split_once('=')?;
            (key == name).then(|| value.to_owned())
        })
        .expect("provider issued the requested cookie")
}

pub fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().copied())
        .finish()
}

pub async fn login(provider: &Provider, auth_url: &str, email: &str, redirect: &str) -> Login {
    let http = cyper::Client::new();
    let first = http.get(auth_url).unwrap().send().await.unwrap();
    assert_eq!(first.status().as_u16(), 303);
    let login_url = location(&first, &provider.base);
    assert_eq!(
        login_url.origin(),
        url::Url::parse(&provider.base).unwrap().origin()
    );
    assert_eq!(login_url.path(), "/login");
    let return_to = query(&login_url).remove("return_to").unwrap();
    let login_get = http.get(login_url.as_str()).unwrap().send().await.unwrap();
    assert_eq!(login_get.status().as_u16(), 200);
    let csrf = cookie(&login_get, "__Host-zsidp_csrf");
    let logged_in = http
        .post(format!("{}/login", provider.base))
        .unwrap()
        .header("content-type", "application/x-www-form-urlencoded")
        .unwrap()
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
        .unwrap()
        .body(form(&[
            ("csrf", &csrf),
            ("email", email),
            ("password", PASSWORD),
            ("return_to", &return_to),
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(logged_in.status().as_u16(), 303);
    let resumed = location(&logged_in, &provider.base);
    assert_eq!(
        resumed,
        url::Url::parse(&provider.base)
            .unwrap()
            .join(&return_to)
            .unwrap()
    );
    let idp_session = cookie(&logged_in, "__Host-zsidp_session");
    let authorized = http
        .get(resumed.as_str())
        .unwrap()
        .header("cookie", format!("__Host-zsidp_session={idp_session}"))
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(authorized.status().as_u16(), 303);
    let mut callback = location(&authorized, &provider.base);
    let mut params = query(&callback);
    callback.set_query(None);
    assert_eq!(
        callback,
        url::Url::parse(redirect).unwrap(),
        "registered callback destination"
    );
    assert_eq!(params.remove("iss").as_deref(), Some(ISSUER));
    let state = params.remove("state").unwrap();
    assert_eq!(
        Some(&state),
        query(&url::Url::parse(auth_url).unwrap()).get("state")
    );
    Login {
        code: params.remove("code").unwrap(),
        state,
        idp_session,
    }
}

pub async fn browser_login(provider: &Provider, app: &App, redirect: &str) -> (Login, String) {
    let verifier = zeroship_core::pkce::generate_verifier();
    let challenge = zeroship_core::pkce::s256_challenge(&verifier);
    let url = rp(&provider.base).build_browser_authorize_url(
        &app.client,
        &BrowserAuthorizeParams {
            code_challenge: &challenge,
            state: &Uuid::new_v4().to_string(),
            nonce: &Uuid::new_v4().to_string(),
            scope: "openid offline_access email profile",
            redirect_uri: redirect,
            prompt: None,
            idp_hint: None,
        },
    );
    (login(provider, &url, &app.email, redirect).await, verifier)
}

pub async fn tokens(provider: &Provider, app: &App) -> crate::oidc_rp::TokenSet {
    let (login, verifier) = browser_login(provider, app, REDIRECT_URI).await;
    rp(&provider.base)
        .exchange_code_public(&app.client, &login.code, &verifier, REDIRECT_URI)
        .await
        .expect("exchange the real authorization code with broker credentials and PKCE")
}
