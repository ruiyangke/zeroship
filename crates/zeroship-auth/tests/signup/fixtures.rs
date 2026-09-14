//! Signup forms, delivered verification links and persisted outcomes.

#![allow(clippy::future_not_send)]

use crate::common::{self, CapturingMailer, CookieJar, auth_server::AuthServer};
use zeroship_auth::{
    identity::password,
    store::{sessions, users},
};

pub(super) const PASSWORD: &str = "signup fixture password phrase";
pub(super) const NAME: &str = "Signup Creator";
pub(super) const IP: &str = "192.0.2.1";

pub(super) struct Form {
    pub cookies: CookieJar,
    pub csrf: String,
    pub return_to: String,
}

impl Form {
    pub async fn get(server: &AuthServer, path: &str) -> Self {
        let response = server
            .http
            .get(format!("{}{path}", server.auth_base))
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(response.headers().get("location").is_none());
        let csrf = common::read_set_cookie(&response, "__Host-zsidp_csrf").unwrap();
        let mut cookies = CookieJar::default();
        cookies.absorb(&response);
        let html = response.text().await.unwrap();
        assert!(
            !html.contains("role=\"alert\""),
            "the form must be usable: {html}"
        );
        assert_eq!(form_field(&html, "csrf"), csrf);
        Self {
            cookies,
            csrf,
            return_to: form_field(&html, "return_to"),
        }
    }

    pub async fn submit(
        &self,
        server: &AuthServer,
        email: &str,
        name: &str,
        ip: &str,
    ) -> cyper::Response {
        post(
            server,
            "/signup",
            &self.cookies.header(),
            &[
                ("csrf", &self.csrf),
                ("return_to", &self.return_to),
                ("email", email),
                ("name", name),
                ("password", PASSWORD),
            ],
            ip,
        )
        .await
    }
}

pub(super) fn query(path: &str, return_to: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", return_to)
        .finish();
    format!("{path}?{query}")
}

pub(super) async fn signup_href(server: &AuthServer, login_path: &str) -> String {
    let response = server
        .http
        .get(format!("{}{login_path}", server.auth_base))
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let html = response.text().await.unwrap();
    let start = html
        .find("href=\"/signup")
        .expect("the login page offers signup");
    let value = &html[start + "href=\"".len()..];
    decode_attribute(&value[..value.find('"').expect("closed signup href")])
}

pub(super) fn form_field(html: &str, name: &str) -> String {
    let at = html.find(&format!("name=\"{name}\"")).expect("form field");
    let start = html[..at].rfind('<').unwrap();
    let end = at + html[at..].find('>').unwrap();
    let tag = &html[start..end];
    let value = &tag[tag.find("value=\"").expect("field value") + "value=\"".len()..];
    decode_attribute(&value[..value.find('"').expect("closed field value")])
}

fn decode_attribute(value: &str) -> String {
    let mut decoded = String::new();
    let mut remaining = value;
    while let Some((before, entity)) = remaining.split_once('&') {
        decoded.push_str(before);
        let (entity, rest) = entity.split_once(';').expect("closed HTML entity");
        let character = match entity {
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            "lt" => '<',
            "gt" => '>',
            value => {
                let scalar = value
                    .strip_prefix("#x")
                    .or_else(|| value.strip_prefix("#X"))
                    .map_or_else(
                        || {
                            value
                                .strip_prefix('#')
                                .expect("rendered numeric entity")
                                .parse()
                                .unwrap()
                        },
                        |hex| u32::from_str_radix(hex, 16).unwrap(),
                    );
                char::from_u32(scalar).unwrap()
            }
        };
        decoded.push(character);
        remaining = rest;
    }
    decoded.push_str(remaining);
    decoded
}

pub(super) async fn post(
    server: &AuthServer,
    path: &str,
    cookies: &str,
    fields: &[(&str, &str)],
    ip: &str,
) -> cyper::Response {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().copied())
        .finish();
    server
        .http
        .request(http::Method::POST, format!("{}{path}", server.auth_base))
        .unwrap()
        .header("content-type", "application/x-www-form-urlencoded")
        .unwrap()
        .header("cookie", cookies)
        .unwrap()
        .header("x-forwarded-for", ip)
        .unwrap()
        .body(body)
        .send()
        .await
        .unwrap()
}

pub(super) async fn created(server: &AuthServer, email: &str) -> users::UserRow {
    let user = users::find_by_email(&server.orm, email)
        .await
        .unwrap()
        .expect("signup creates the account");
    assert_eq!(user.email, email);
    assert_eq!(user.name, NAME);
    assert!(password::verify(PASSWORD, user.password_hash.as_deref().unwrap()).unwrap());
    user
}

pub(super) fn verification_link(
    server: &AuthServer,
    mailer: &CapturingMailer,
    email: &str,
) -> url::Url {
    let messages = mailer.sent();
    let messages: Vec<_> = messages
        .iter()
        .filter(|message| message.to.email == email)
        .collect();
    let [message] = messages.as_slice() else {
        panic!("expected verification mail for {email}")
    };
    assert_eq!(message.subject, "Verify your zeroship email");
    let link = message
        .text
        .split_whitespace()
        .find(|word| word.starts_with("http://"))
        .unwrap();
    let link = url::Url::parse(link).unwrap();
    assert_eq!(
        link.origin(),
        url::Url::parse(&server.auth_base).unwrap().origin()
    );
    assert_eq!(link.path(), "/verify");
    assert!(message.html.as_deref().unwrap().contains(link.as_str()));
    link
}

pub(super) async fn assert_counts(server: &AuthServer, users: i64, verifications: i64) {
    let row = server
        .pg
        .query_one(
            "SELECT (SELECT COUNT(*) FROM zeroship.users), \
         (SELECT COUNT(*) FROM zeroship.email_verifications), \
         (SELECT COUNT(*) FROM zeroship.idp_sessions)",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), users);
    assert_eq!(row.get::<_, i64>(1), verifications);
    assert_eq!(
        row.get::<_, i64>(2),
        0,
        "signup does not sign the browser in"
    );
}

pub(super) async fn assert_redirect(response: cyper::Response, target: &str) -> Vec<u8> {
    assert_eq!(response.status().as_u16(), 302);
    assert_eq!(common::location(&response), query("/login", target));
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
    response.bytes().await.unwrap().to_vec()
}

pub(super) async fn sign_in(server: &AuthServer, user: &users::UserRow, target: &str) {
    let form = Form::get(server, &query("/login", target)).await;
    assert_eq!(form.return_to, target);
    let response = post(
        server,
        "/login",
        &form.cookies.header(),
        &[
            ("csrf", &form.csrf),
            ("return_to", &form.return_to),
            ("email", &user.email),
            ("password", PASSWORD),
        ],
        IP,
    )
    .await;
    assert_eq!(response.status().as_u16(), 303);
    assert_eq!(common::location(&response), target);
    let cookie = common::read_set_cookie(&response, "__Host-zsidp_session").unwrap();
    let session = sessions::validate(&server.pg, cookie.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.user_id, user.id);
    assert_eq!(session.amr, ["pwd"]);
}
