use crate::common;
use crate::common::database::Database;
use crate::common::mock_control::{untrusted_auth_keyring, Answer, MockControl};
use std::sync::Arc;
use uuid::Uuid;
use zeroship_auth::cron::account_reaper;
use zeroship_auth::sessions::login as session_cookie;
use zeroship_auth::store::{sessions, users};
use zeroship_core::service_peers::ServiceKeyring;

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn the_emailed_link_cancels_deletion_without_a_session() {
    Database::run(async |database| {
    let db = database.connect().await;
    let control = MockControl::start(Answer::Clear).await;
    let fixture = DeletionServer::start(database, &control, control.keyring()).await;
    let (user, session) = signed_in_user(&db).await;

    let requested = fixture.request_deletion(session.id).await;
    assert_eq!(requested.status().as_u16(), 302);
    assert_eq!(control.asked(), [user.id.as_str().to_owned()]);
    assert!(sessions::validate(&db, session.id).await.unwrap().is_none());
    let pending: bool = db
        .query_one(
            "SELECT deletion_requested_at IS NOT NULL AND deletion_scheduled_for IS NOT NULL
             FROM zeroship.users WHERE id = $1",
            &[&user.id.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert!(pending, "the HTTP request schedules deletion");

    let sent = fixture.mailer.sent();
    let [email] = sent.as_slice() else {
        panic!("deletion must send its confirmation email");
    };
    assert_eq!(email.to.email, user.email);
    let links: Vec<_> = email
        .text
        .lines()
        .filter_map(|line| url::Url::parse(line.trim()).ok())
        .collect();
    let [link] = links.as_slice() else {
        panic!("the rendered email must contain its undo URL");
    };
    assert_eq!(
        link.origin(),
        url::Url::parse(&fixture.base).unwrap().origin()
    );
    assert_eq!(link.path(), "/me/delete/cancel");
    let token = common::extract_query_param(link.as_str(), "token").expect("mailed undo token");
    assert!(!token.is_empty());
    assert!(
        email
            .html
            .as_ref()
            .expect("HTML confirmation")
            .contains(&format!("href=\"{link}\"")),
        "the HTML and plain-text messages offer the same undo link"
    );

    // Follow the actual message with a fresh client: deletion revoked the
    // login session, so cancellation must work without its cookie.
    let http = cyper::Client::new();
    let get = http.get(link.as_str()).unwrap().send().await.unwrap();
    assert_eq!(get.status().as_u16(), 200);
    let csrf = common::read_set_cookie(&get, "__Host-zsidp_csrf").expect("csrf cookie");
    assert!(get.text().await.unwrap().contains(&token));
    let post = fixture.cancel(&csrf, &token).await;
    assert_eq!(post.status().as_u16(), 200);

    let restored: bool = db
        .query_one(
            "SELECT deletion_requested_at IS NULL AND deletion_scheduled_for IS NULL AND disabled_at IS NULL
             FROM zeroship.users WHERE id = $1",
            &[&user.id.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert!(restored, "the route clears the deletion state");
    assert!(sessions::validate(&db, session.id).await.unwrap().is_none());
    assert_eq!(
        fixture.cancel(&csrf, &token).await.status().as_u16(),
        400,
        "the mailed token is single-use"
    );
    }).await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn the_cancel_route_refuses_a_token_it_never_issued() {
    Database::run(async |database| {
        let mut db = database.connect().await;
        let control = MockControl::start(Answer::Clear).await;
        let fixture = DeletionServer::start(database, &control, control.keyring()).await;
        let (user, _) = signed_in_user(&db).await;
        users::request_deletion(&mut db, &user.id, account_reaper::GRACE_DAYS)
            .await
            .unwrap()
            .unwrap();

        let forged = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let get = cyper::Client::new()
            .get(format!("{}/me/delete/cancel?token={forged}", fixture.base))
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(get.status().as_u16(), 200);
        let csrf = common::read_set_cookie(&get, "__Host-zsidp_csrf").expect("csrf cookie");
        assert_eq!(fixture.cancel(&csrf, forged).await.status().as_u16(), 400);
        let pending: bool = db
            .query_one(
                "SELECT deletion_requested_at IS NOT NULL FROM zeroship.users WHERE id = $1",
                &[&user.id.as_str()],
            )
            .await
            .unwrap()
            .get(0);
        assert!(pending, "the pending deletion survives a forged token");
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn a_refused_preflight_leaves_the_account_and_session_active() {
    Database::run(async |database| {
    let db = database.connect().await;
    for (answer, trusted, status) in [
        (
            Answer::SoleOwnerOf {
                slug: "owned".into(),
            },
            true,
            409,
        ),
        (
            Answer::OwesBilling {
                slug: "owing".into(),
                owed_cents: 500,
            },
            true,
            409,
        ),
        (Answer::Unavailable, true, 503),
        (Answer::Clear, false, 503),
    ] {
        let control = MockControl::start(answer.clone()).await;
        let keyring = if trusted {
            control.keyring()
        } else {
            untrusted_auth_keyring()
        };
        let fixture = DeletionServer::start(database, &control, keyring).await;
        let (user, session) = signed_in_user(&db).await;
        let response = fixture.request_deletion(session.id).await;
        assert_eq!(
            response.status().as_u16(),
            status,
            "{answer:?}, trusted={trusted}"
        );
        assert_eq!(
            control.asked(),
            if trusted {
                vec![user.id.as_str().to_owned()]
            } else {
                vec![]
            }
        );
        assert!(
            fixture.mailer.sent().is_empty(),
            "a refusal sends no deletion email"
        );
        let row = db
            .query_one(
                "SELECT credential_version,
                        disabled_at IS NULL AND deletion_requested_at IS NULL AND deletion_scheduled_for IS NULL,
                        NOT EXISTS (SELECT FROM zeroship.magic_links WHERE user_id = $1 AND purpose = 'deletion_cancel')
                 FROM zeroship.users WHERE id = $1",
                &[&user.id.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, i64>(0), session.credential_version);
        assert!(
            row.get::<_, bool>(1),
            "a refusal cannot disable or schedule erasure"
        );
        assert!(
            row.get::<_, bool>(2),
            "a refusal cannot issue an undo token"
        );
        assert!(sessions::validate(&db, session.id).await.unwrap().is_some());
    }
    }).await;
}

#[allow(clippy::future_not_send)]
async fn signed_in_user(db: &compio_postgres::Client) -> (users::UserRow, sessions::Session) {
    let user = users::create(
        db,
        &format!("acctdel-http-{}@zeroship.test", Uuid::new_v4().simple()),
        "Deletion request",
        None,
    )
    .await
    .unwrap();
    let session = sessions::create(
        db,
        &sessions::CreateSession {
            user_id: user.id.clone(),
            auth_method: "password",
            amr: vec!["pwd".into()],
            acr: None,
            expected_credential_version: None,
            idle_minutes: session_cookie::IDLE_MINUTES,
            absolute_hours: session_cookie::ABSOLUTE_HOURS,
        },
    )
    .await
    .unwrap();
    (user, session)
}

struct DeletionServer {
    base: String,
    mailer: Arc<common::CapturingMailer>,
    _srv: ntex::web::test::TestServer,
}

impl DeletionServer {
    #[allow(clippy::future_not_send)]
    async fn start(
        database: &Database,
        control: &MockControl,
        keyring: Arc<ServiceKeyring>,
    ) -> Self {
        let dsn = database.auth_url().to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve auth listener");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut settings =
            common::test_auth_config_with(&dsn, &["--control-url", &control.base]).settings;
        settings.public_url = zeroship_core::config::Operational::new(base.clone());
        let cfg = Arc::new(zeroship_auth::config::AuthConfig::from_resolved(settings).unwrap());
        let db = Arc::new(database.connect_as_auth().await);
        let refresh_pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(dsn, 2);
        let issuer = Arc::new(
            zeroship_auth::oidc::Issuer::from_signing_key(
                &ed25519_dalek::SigningKey::from_bytes(&[22; 32]),
                zeroship_core::auth::derive_pairwise_salt(b"deletion-http-fixture"),
                format!("{base}/oauth2"),
            )
            .unwrap(),
        );
        issuer
            .publish_active_key(&db)
            .await
            .expect("publish fixture signing key");
        let mailer = Arc::new(common::CapturingMailer::default());
        let mailer_state: Arc<dyn zeroship_mailer::Mailer> = mailer.clone();
        let srv =
            ntex::web::test::server_with(ntex::web::test::config().listener(listener), move || {
                let cfg = cfg.clone();
                let db = db.clone();
                let refresh_pool = refresh_pool.clone();
                let issuer = issuer.clone();
                let keyring = keyring.clone();
                let mailer = mailer_state.clone();
                async move {
                    ntex::web::App::new()
                        .state(cfg)
                        .state(db)
                        .state(refresh_pool)
                        .state(issuer)
                        .state(keyring)
                        .state(mailer)
                        .configure(zeroship_auth::server::configure(false, false))
                }
            })
            .await;
        Self {
            base,
            mailer,
            _srv: srv,
        }
    }

    #[allow(clippy::future_not_send)]
    async fn request_deletion(&self, session: Uuid) -> cyper::Response {
        let csrf = zeroship_auth::csrf::generate_token();
        cyper::Client::new()
            .post(format!("{}/me/delete", self.base))
            .unwrap()
            .header("content-type", "application/x-www-form-urlencoded")
            .unwrap()
            .header(
                "cookie",
                format!(
                    "{}={session}; __Host-zsidp_csrf={csrf}",
                    session_cookie::COOKIE_NAME
                ),
            )
            .unwrap()
            .body(
                url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("csrf", &csrf)
                    .finish(),
            )
            .send()
            .await
            .unwrap()
    }

    #[allow(clippy::future_not_send)]
    async fn cancel(&self, csrf: &str, token: &str) -> cyper::Response {
        cyper::Client::new()
            .post(format!("{}/me/delete/cancel", self.base))
            .unwrap()
            .header("content-type", "application/x-www-form-urlencoded")
            .unwrap()
            .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
            .unwrap()
            .body(
                url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("csrf", csrf)
                    .append_pair("token", token)
                    .finish(),
            )
            .send()
            .await
            .unwrap()
    }
}
