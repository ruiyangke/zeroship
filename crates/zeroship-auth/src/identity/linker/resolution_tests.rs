use super::{LinkOutcome, LinkResume, PendingLink, ResolvedProfile, resolve_or_link};
use crate::{
    error::AuthError,
    identity::password,
    store::{identities, users},
    test_database::Database,
};
use compio_postgres::Client;
use uuid::Uuid;
use zeroship_core::UserId;

const RETURN_TO: &str =
    "/oauth2/authorize?client_id=oac_fixture&redirect_uri=https%3A%2F%2Fapp.test%2Fcb&scope=openid";
const KEY: &[u8] = b"link-resolution-fixture-signing-key";

fn profile<'a>(
    provider: &'a str,
    subject: &'a str,
    email: &'a str,
    trusted: bool,
) -> ResolvedProfile<'a> {
    ResolvedProfile {
        provider,
        subject,
        email,
        name: Some("Provider name"),
        avatar_url: None,
        provider_trusted_for_email: trusted,
        raw_profile: None,
    }
}

/// A per-case email so cases sharing the database never act on one another's
/// user or identity rows.
fn fixture_email(label: &str) -> String {
    format!("{label}-{}@example.test", Uuid::new_v4().simple())
}

/// A per-case provider subject, unique within the `(provider, subject)` key.
fn fixture_subject() -> String {
    format!("subject-{}", Uuid::new_v4().simple())
}

#[allow(clippy::future_not_send, reason = "the ORM belongs to its compio runtime")]
async fn require_confirmation(
    pg: &Client,
    orm: &zeroship_data_orm::Database,
    profile: &ResolvedProfile<'_>,
    user_id: &UserId,
) {
    let outcome = resolve_or_link(pg, orm, profile, LinkResume::ReturnTo(RETURN_TO), KEY)
        .await
        .unwrap();
    let LinkOutcome::NeedsConfirmation {
        pending_token,
        existing_email,
        provider,
    } = outcome
    else {
        panic!("expected confirmation, got {outcome:?}");
    };
    assert_eq!(existing_email, profile.email);
    assert_eq!(provider, profile.provider);
    let pending = PendingLink::decode(&pending_token, KEY).unwrap();
    assert_eq!(&pending.user_id, user_id);
    assert_eq!(pending.provider, profile.provider);
    assert_eq!(pending.subject, profile.subject);
    assert_eq!(pending.email, profile.email);
    assert_eq!(pending.return_to.as_deref(), Some(RETURN_TO));
    assert!(
        identities::list_for_user(pg, user_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        users::find_by_id(orm, user_id).await.unwrap().unwrap().name,
        "Local name"
    );
}

#[compio::test]
async fn a_password_requires_confirmation_and_preserves_the_native_continuation() {
    Database::run(async |database| {
        let pg = database.connect_as_auth().await;
        let orm = database.orm().await;
        let hash = password::hash("existing-password").await.unwrap();
        let email = fixture_email("link-password");
        let subject = fixture_subject();
        let user = users::create(&orm, &email, "Local name", Some(&hash))
            .await
            .unwrap();
        for provider in ["google", "github"] {
            require_confirmation(&pg, &orm, &profile(provider, &subject, &email, true), &user.id)
                .await;
        }
        assert_eq!(
            users::find_by_id(&orm, &user.id)
                .await
                .unwrap()
                .unwrap()
                .password_hash
                .as_deref(),
            Some(hash.as_str())
        );
    })
    .await;
}

#[compio::test]
async fn untrusted_email_requires_confirmation_for_an_account_without_a_password() {
    Database::run(async |database| {
        let pg = database.connect_as_auth().await;
        let orm = database.orm().await;
        let email = fixture_email("link-untrusted-confirm");
        let subject = fixture_subject();
        let user = users::create(&orm, &email, "Local name", None)
            .await
            .unwrap();
        require_confirmation(&pg, &orm, &profile("google", &subject, &email, false), &user.id).await;
    })
    .await;
}

#[compio::test]
async fn trusted_email_links_an_existing_account_without_overwriting_its_profile() {
    Database::run(async |database| {
        let pg = database.connect_as_auth().await;
        let orm = database.orm().await;
        let email = fixture_email("link-trusted");
        let subject = fixture_subject();
        let user = users::create(&orm, &email, "Local name", None)
            .await
            .unwrap();
        let profile = profile("github", &subject, &email, true);
        let outcome = resolve_or_link(&pg, &orm, &profile, LinkResume::ReturnTo(RETURN_TO), KEY)
            .await
            .unwrap();
        let LinkOutcome::Existing { user_id } = outcome else {
            panic!("expected existing account, got {outcome:?}");
        };
        assert_eq!(user_id, user.id);
        let linked = identities::find_by_provider_subject(&pg, profile.provider, profile.subject)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(linked.user_id, user.id);
        assert_eq!(
            users::find_by_id(&orm, &user.id)
                .await
                .unwrap()
                .unwrap()
                .name,
            "Local name"
        );
        assert_eq!(
            identities::list_for_user(&pg, &user.id)
                .await
                .unwrap()
                .len(),
            1
        );

        let changed_email = fixture_email("link-changed");
        let other = users::create(&orm, &changed_email, "Other account", None)
            .await
            .unwrap();
        let changed_profile = ResolvedProfile {
            email: &changed_email,
            ..profile
        };
        let outcome = resolve_or_link(
            &pg,
            &orm,
            &changed_profile,
            LinkResume::ReturnTo(RETURN_TO),
            KEY,
        )
        .await
        .unwrap();
        let LinkOutcome::Existing { user_id } = outcome else {
            panic!("expected established identity, got {outcome:?}");
        };
        assert_eq!(
            user_id, user.id,
            "a changed provider email cannot rebind the established subject"
        );
        assert!(
            identities::list_for_user(&pg, &other.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            identities::list_for_user(&pg, &user.id)
                .await
                .unwrap()
                .len(),
            1
        );
    })
    .await;
}

#[compio::test]
async fn untrusted_email_creates_no_account_or_identity_and_trusted_retry_can_succeed() {
    Database::run(async |database| {
        let pg = database.connect_as_auth().await;
        let orm = database.orm().await;
        let provider = "google";
        let email = fixture_email("link-untrusted-create");
        let subject = fixture_subject();
        let profile = profile(provider, &subject, &email, false);
        let error = resolve_or_link(&pg, &orm, &profile, LinkResume::ReturnTo(RETURN_TO), KEY).await.unwrap_err();
        assert!(matches!(error, AuthError::Internal(reason) if reason.contains("untrusted provider email")));
        assert!(pg
            .query("SELECT id FROM zeroship.users WHERE email = $1::citext", &[&email])
            .await
            .unwrap()
            .is_empty());
        assert!(pg
            .query(
                "SELECT id FROM zeroship.federated_identities WHERE provider = $1 AND subject = $2",
                &[&provider, &subject],
            )
            .await
            .unwrap()
            .is_empty());

        let trusted_profile = ResolvedProfile { provider_trusted_for_email: true, ..profile };
        let outcome = resolve_or_link(&pg, &orm, &trusted_profile, LinkResume::ReturnTo(RETURN_TO), KEY).await.unwrap();
        let LinkOutcome::Created { user_id } = outcome else { panic!("expected created account, got {outcome:?}"); };
        let user = users::find_by_id(&orm, &user_id).await.unwrap().unwrap();
        assert_eq!(user.email, trusted_profile.email);
        assert_eq!(user.name, trusted_profile.name.unwrap());
        assert!(user.email_verified_at.is_some());
        assert_eq!(identities::find_by_provider_subject(&pg, trusted_profile.provider, trusted_profile.subject).await.unwrap().unwrap().user_id, user_id);
    }).await;
}
