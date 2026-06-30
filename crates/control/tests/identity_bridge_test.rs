//! Regression coverage for the control-plane JIT identity bridge.

use compio_postgres::{connect, Client, NoTls};
use futures::join;
use uuid::Uuid;
use zeroship_control::identity_bridge::{
    parse_gotrue_admin_email_verified, provision_or_link,
};

const PROVIDER: &str = "supabase";

fn db_url() -> String {
    std::env::var("CONTROL_TEST_DB")
        .expect("CONTROL_TEST_DB must be set so identity_bridge_test runs against Postgres")
}

async fn open_conn() -> Client {
    let (client, connection) = connect(&db_url(), NoTls)
        .await
        .expect("connect CONTROL_TEST_DB");
    compio::runtime::spawn(async move {
        if let Err(err) = connection.run().await {
            eprintln!("[identity_bridge_test] pg connection error: {err}");
        }
    })
    .detach();
    client
}

struct Fixture {
    conn: Client,
    users: Vec<Uuid>,
    subjects: Vec<String>,
}

impl Fixture {
    async fn new() -> Self {
        Self {
            conn: open_conn().await,
            users: Vec::new(),
            subjects: Vec::new(),
        }
    }

    fn track_user(&mut self, principal_id: Uuid) {
        if !self.users.contains(&principal_id) {
            self.users.push(principal_id);
        }
    }

    fn track_subject(&mut self, subject: &str) {
        if !self.subjects.iter().any(|seen| seen == subject) {
            self.subjects.push(subject.to_string());
        }
    }

    async fn seed_user(&mut self, email: &str, grants: &[&str]) -> Uuid {
        let principal_id = Uuid::new_v4();
        self.conn
            .execute(
                "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
                 VALUES ($1, $2::citext, 'Identity Bridge Victim', NOW())",
                &[&principal_id, &email],
            )
            .await
            .expect("insert seed user");
        for grant in grants {
            self.conn
                .execute(
                    "INSERT INTO zeroship.principal_grants (principal_id, grant_name) \
                     VALUES ($1, $2)",
                    &[&principal_id, grant],
                )
                .await
                .expect("insert seed grant");
        }
        self.track_user(principal_id);
        principal_id
    }

    async fn cleanup(&mut self) {
        for subject in &self.subjects {
            let _ = self
                .conn
                .execute(
                    "DELETE FROM zeroship.identity_links \
                     WHERE provider = $1 AND provider_subject = $2",
                    &[&PROVIDER, subject],
                )
                .await;
        }
        for user_id in &self.users {
            let _ = self
                .conn
                .execute(
                    "DELETE FROM zeroship.principal_grants WHERE principal_id = $1",
                    &[user_id],
                )
                .await;
        }
        for user_id in &self.users {
            let _ = self
                .conn
                .execute("DELETE FROM zeroship.users WHERE id = $1", &[user_id])
                .await;
        }
    }
}

fn unique_email(label: &str) -> String {
    format!("{label}-{}@x.com", Uuid::new_v4().simple())
}

async fn linked_principal(conn: &Client, subject: &str) -> Uuid {
    conn.query_one(
        "SELECT principal_id \
         FROM zeroship.identity_links \
         WHERE provider = $1 AND provider_subject = $2",
        &[&PROVIDER, &subject],
    )
    .await
    .expect("select identity link")
    .get("principal_id")
}

async fn link_count(conn: &Client, subject: &str) -> i64 {
    conn.query_one(
        "SELECT count(*)::bigint AS n \
         FROM zeroship.identity_links \
         WHERE provider = $1 AND provider_subject = $2",
        &[&PROVIDER, &subject],
    )
    .await
    .expect("count identity links")
    .get("n")
}

async fn user_count_by_email(conn: &Client, email: &str) -> i64 {
    conn.query_one(
        "SELECT count(*)::bigint AS n \
         FROM zeroship.users \
         WHERE email = $1::citext",
        &[&email],
    )
    .await
    .expect("count users by email")
    .get("n")
}

async fn grants(conn: &Client, principal_id: Uuid) -> Vec<String> {
    conn.query(
        "SELECT grant_name \
         FROM zeroship.principal_grants \
         WHERE principal_id = $1 \
         ORDER BY grant_name",
        &[&principal_id],
    )
    .await
    .expect("select grants")
    .iter()
    .map(|row| row.get("grant_name"))
    .collect()
}

#[compio::test]
async fn first_login_creates_principal_link_and_default_grants() {
    let mut fx = Fixture::new().await;
    let subject = Uuid::new_v4().to_string();
    let email = unique_email("first-login");

    let principal_id =
        provision_or_link(&mut fx.conn, PROVIDER, &subject, Some(&email), true)
            .await
            .expect("provision first login");
    fx.track_subject(&subject);
    fx.track_user(principal_id);

    assert_eq!(linked_principal(&fx.conn, &subject).await, principal_id);
    assert_eq!(link_count(&fx.conn, &subject).await, 1);
    assert_eq!(
        grants(&fx.conn, principal_id).await,
        vec!["apps:deploy".to_string(), "apps:read".to_string()]
    );

    fx.cleanup().await;
}

#[compio::test]
async fn idempotent_relogin_returns_same_principal_without_duplicate_rows() {
    let mut fx = Fixture::new().await;
    let subject = Uuid::new_v4().to_string();
    let email = unique_email("relogin");

    let first =
        provision_or_link(&mut fx.conn, PROVIDER, &subject, Some(&email), true)
            .await
            .expect("first provision");
    let second =
        provision_or_link(&mut fx.conn, PROVIDER, &subject, Some(&email), true)
            .await
            .expect("second provision");
    fx.track_subject(&subject);
    fx.track_user(first);

    assert_eq!(second, first);
    assert_eq!(link_count(&fx.conn, &subject).await, 1);
    assert_eq!(
        grants(&fx.conn, first).await,
        vec!["apps:deploy".to_string(), "apps:read".to_string()]
    );

    fx.cleanup().await;
}

#[compio::test]
async fn verified_email_collision_merges_onto_existing_principal_without_new_grants() {
    let mut fx = Fixture::new().await;
    let email = unique_email("verified-victim");
    let victim = fx.seed_user(&email, &[]).await;
    let subject = Uuid::new_v4().to_string();

    let linked =
        provision_or_link(&mut fx.conn, PROVIDER, &subject, Some(&email), true)
            .await
            .expect("verified email merge");
    fx.track_subject(&subject);

    assert_eq!(linked, victim);
    assert_eq!(linked_principal(&fx.conn, &subject).await, victim);
    assert_eq!(user_count_by_email(&fx.conn, &email).await, 1);
    assert!(grants(&fx.conn, victim).await.is_empty());

    fx.cleanup().await;
}

#[compio::test]
async fn unverified_email_collision_creates_distinct_principal_not_victim_takeover() {
    let mut fx = Fixture::new().await;
    let email = unique_email("unverified-victim");
    let victim = fx
        .seed_user(&email, &["apps:deploy", "apps:read"])
        .await;
    let subject = Uuid::new_v4().to_string();

    let attacker =
        provision_or_link(&mut fx.conn, PROVIDER, &subject, Some(&email), false)
            .await
            .expect("unverified email collision provisions separately");
    fx.track_subject(&subject);
    fx.track_user(attacker);

    assert_ne!(
        attacker, victim,
        "unverified email collision must not link attacker subject to victim principal"
    );
    assert_eq!(linked_principal(&fx.conn, &subject).await, attacker);
    assert_eq!(
        grants(&fx.conn, victim).await,
        vec!["apps:deploy".to_string(), "apps:read".to_string()]
    );
    assert_eq!(
        grants(&fx.conn, attacker).await,
        vec!["apps:deploy".to_string(), "apps:read".to_string()]
    );

    fx.cleanup().await;
}

#[compio::test]
async fn concurrent_first_logins_for_same_subject_collapse_to_one_link() {
    let mut fx = Fixture::new().await;
    let mut conn_a = open_conn().await;
    let mut conn_b = open_conn().await;
    let subject = Uuid::new_v4().to_string();
    let email = unique_email("concurrent");

    let (a, b) = join!(
        provision_or_link(&mut conn_a, PROVIDER, &subject, Some(&email), true),
        provision_or_link(&mut conn_b, PROVIDER, &subject, Some(&email), true),
    );
    let a = a.expect("concurrent provision A");
    let b = b.expect("concurrent provision B");
    fx.track_subject(&subject);
    fx.track_user(a);
    fx.track_user(b);

    assert_eq!(a, b);
    assert_eq!(link_count(&fx.conn, &subject).await, 1);
    assert_eq!(user_count_by_email(&fx.conn, &email).await, 1);
    assert_eq!(
        grants(&fx.conn, a).await,
        vec!["apps:deploy".to_string(), "apps:read".to_string()]
    );

    fx.cleanup().await;
}

#[test]
fn gotrue_admin_user_json_uses_email_confirmed_at_only() {
    assert_eq!(
        parse_gotrue_admin_email_verified(br#"{"email_confirmed_at":"2026-06-29T12:00:00Z"}"#),
        Some(true)
    );
    assert_eq!(
        parse_gotrue_admin_email_verified(br#"{"email_confirmed_at":null}"#),
        Some(false)
    );
    assert_eq!(
        parse_gotrue_admin_email_verified(
            br#"{"user_metadata":{"email_verified":true}}"#
        ),
        Some(false)
    );
}
