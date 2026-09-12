//! `zeroship.sessions` + `zeroship.grants` against live `PostgreSQL`: the
//! validating reads that MINT-READS-ROW rests on.
//!
//! Every arm here drives the STATEMENTS, not a wrapper around them, because the
//! property under test is a property of the SQL: the predicate that refuses a
//! revoked session lives in `ROTATE_SESSION_SQL`, and a test that stopped short
//! of the database would pass with that predicate deleted. Each refusal arm is
//! paired with a control differing in exactly one variable, so a refusal that
//! is really a broken fixture cannot read as a fence.
//!
//! Each case owns its `PostgreSQL` server and uses the auth role.

use std::io::Write as _;

use crate::common::database::Database;
use compio_postgres::Client;
use uuid::Uuid;
use zeroship_auth::session_store::{
    self, Audience, NewSession, RotatedSession, SecretSlot, SessionKind, SessionSecretKeys,
};
use zeroship_auth::store::users;

mod liveness;

const IDLE_DAYS: i64 = 7;
const ABSOLUTE_DAYS: i64 = 30;
const IDEM_WINDOW_SECS: i64 = 30;

/// A keyring in a private directory, owner-only, as the loader demands.
fn keys() -> SessionSecretKeys {
    let directory = tempfile::tempdir().expect("private session key directory");
    let dir = directory.path();
    let hash_path = dir.join("hash");
    let idem_path = dir.join("idem");
    write_owner_only(
        &hash_path,
        b"1:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff\n",
    );
    write_owner_only(&idem_path, b"session-object-test-idempotency-master-secret");
    let keys = SessionSecretKeys::from_files(&hash_path, &idem_path).expect("load session keys");
    directory.close().expect("remove loaded session key files");
    keys
}

fn write_owner_only(path: &std::path::Path, body: &[u8]) {
    let mut file = std::fs::File::create(path).expect("create key file");
    file.write_all(body).expect("write key file");
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod key file");
    }
}

/// A person, a registered client and an app-audience grant in the case database.
async fn seed(db: &Client, tag: &str) -> (zeroship_core::UserId, String, String) {
    let email = format!("session-object-{tag}@zeroship.test");
    let user = users::create(db, &email, "Session Object", None)
        .await
        .expect("seed person");
    let person_id: zeroship_core::UserId = user.id;
    let client_id = format!("oac_sessionobject{tag}");
    db.execute(
        "INSERT INTO zeroship.oauth_clients (client_id, client_name, redirect_uris, scopes) \
         VALUES ($1, 'session object test', ARRAY['https://example.test/cb'], ARRAY['openid'])",
        &[&client_id],
    )
    .await
    .expect("seed client");
    let scopes = vec!["openid".to_string(), "offline_access".to_string()];
    let grant_id = session_store::upsert_grant(
        db,
        &person_id,
        &Audience::App {
            client_id: client_id.clone(),
        },
        &format!("pws_{tag}"),
        &scopes,
        None,
    )
    .await
    .expect("seed grant");
    (person_id, client_id, grant_id)
}

const fn new_session<'a>(
    person_id: &'a zeroship_core::UserId,
    grant_id: &'a str,
    subject: &'a str,
    scopes: &'a [String],
    amr: &'a [String],
) -> NewSession<'a> {
    NewSession {
        person_id,
        grant_id,
        subject,
        grant_scopes: scopes,
        parent_session_id: None,
        kind: SessionKind::Cli,
        scopes,
        amr,
        acr: None,
        label: None,
        expected_credential_epoch: None,
        idle_days: IDLE_DAYS,
        absolute_days: ABSOLUTE_DAYS,
        with_secret: true,
    }
}

fn tag() -> String {
    Uuid::new_v4().simple().to_string()
}

// ---------------------------------------------------------------------------
// The step's named red test: revoke, then attempt a mint.
// ---------------------------------------------------------------------------

/// THE arm this step names. Revoke the session, present the live secret, and
/// the rotating validating read must refuse - which is what stops a credential
/// being minted for a session that has ended.
///
/// Deleting `s.revoked_at IS NULL` from `ROTATE_SESSION_SQL` makes this fail by
/// returning a `RotatedSession`, and the control below keeps that failure from
/// being read as a broken fixture.
#[compio::test]
async fn a_revoked_session_cannot_mint() {
    Database::run(async |database| {
        let db = database.connect_as_auth().await;
        let tag = tag();
        let keys = keys();
        let (person_id, _client_id, grant_id) = seed(&db, &tag).await;
        let scopes = vec!["openid".to_string(), "offline_access".to_string()];
        let amr = vec!["pwd".to_string()];

        let created = session_store::create(
            &db,
            &keys,
            &new_session(&person_id, &grant_id, &format!("pws_{tag}"), &scopes, &amr),
        )
        .await
        .expect("create session")
        .expect("session created");
        let secret = created.secret.clone().expect("session carries a secret");

        session_store::revoke(&db, &created.row.id, "test")
            .await
            .expect("revoke");

        let presented = session_store::peek(&db, &keys, &secret)
            .await
            .expect("peek")
            .expect("the revoked row is still findable by its hash");
        assert_eq!(presented.slot(), SecretSlot::Current);

        let rotation =
            session_store::rotate(&db, &keys, &presented, &scopes, IDLE_DAYS, IDEM_WINDOW_SECS)
                .await
                .expect("rotate");
        assert!(rotation.is_none(), "a revoked session minted a credential");
    })
    .await;
}

/// The control for the arm above, differing in ONE variable: the session is not
/// revoked. Without it, a rotation broken for any other reason would read as
/// the revocation fence working.
#[compio::test]
async fn a_live_session_mints_where_a_revoked_one_does_not() {
    Database::run(async |database| {
        let db = database.connect_as_auth().await;
        let tag = tag();
        let keys = keys();
        let (person_id, _client_id, grant_id) = seed(&db, &tag).await;
        let scopes = vec!["openid".to_string(), "offline_access".to_string()];
        let amr = vec!["pwd".to_string()];

        let created = session_store::create(
            &db,
            &keys,
            &new_session(&person_id, &grant_id, &format!("pws_{tag}"), &scopes, &amr),
        )
        .await
        .expect("create session")
        .expect("session created");
        let secret = created.secret.clone().expect("session carries a secret");

        let presented = session_store::peek(&db, &keys, &secret)
            .await
            .expect("peek")
            .expect("live secret resolves");
        let rotation =
            session_store::rotate(&db, &keys, &presented, &scopes, IDLE_DAYS, IDEM_WINDOW_SECS)
                .await
                .expect("rotate");
        match rotation {
            Some(RotatedSession {
                row,
                secret: next,
                proof,
            }) => {
                assert_ne!(next, secret, "rotation returned the same secret");
                assert_eq!(proof.session_id(), row.id);
                assert_eq!(proof.person_id(), &person_id);
            }
            None => panic!("a live session refused to mint"),
        }
    })
    .await;
}

// ---------------------------------------------------------------------------
// The other liveness predicates on the same statement
// ---------------------------------------------------------------------------

/// An expired absolute ceiling refuses, and the control is the same session
/// before its ceiling is moved.
#[compio::test]
async fn an_expired_session_cannot_mint_and_the_same_row_could_before() {
    Database::run(async |database| {
        let db = database.connect_as_auth().await;
        let tag = tag();
        let keys = keys();
        let (person_id, _client_id, grant_id) = seed(&db, &tag).await;
        let scopes = vec!["openid".to_string()];
        let amr = vec!["pwd".to_string()];

        let created = session_store::create(
            &db,
            &keys,
            &new_session(&person_id, &grant_id, &format!("pws_{tag}"), &scopes, &amr),
        )
        .await
        .expect("create session")
        .expect("session created");
        let secret = created.secret.clone().expect("secret");

        // Control first, on the SAME row: it mints.
        let presented = session_store::peek(&db, &keys, &secret)
            .await
            .expect("peek")
            .expect("resolves");
        let Some(RotatedSession { secret: next, .. }) =
            session_store::rotate(&db, &keys, &presented, &scopes, IDLE_DAYS, IDEM_WINDOW_SECS)
                .await
                .expect("rotate")
        else {
            panic!("the control rotation refused");
        };

        // One variable moves: the ceiling goes into the past.
        db.execute(
            "UPDATE zeroship.sessions \
             SET absolute_expires_at = NOW() - INTERVAL '1 hour', \
                 idle_expires_at = NOW() - INTERVAL '1 hour' \
             WHERE id = $1",
            &[&created.row.id],
        )
        .await
        .expect("expire");

        let presented = session_store::peek(&db, &keys, &next)
            .await
            .expect("peek")
            .expect("resolves");
        let rotation =
            session_store::rotate(&db, &keys, &presented, &scopes, IDLE_DAYS, IDEM_WINDOW_SECS)
                .await
                .expect("rotate");
        assert!(rotation.is_none(), "an expired session minted a credential");
    })
    .await;
}

/// A suspended grant refuses creation, and an active one creates. The
/// suspension is audience-scoped and rides the same statement that resolves the
/// grant, so there is no second enforcement path to keep in agreement.
#[compio::test]
async fn a_suspended_grant_cannot_create_a_session() {
    Database::run(async |database| {
        let db = database.connect_as_auth().await;
        let tag = tag();
        let keys = keys();
        let (person_id, _client_id, grant_id) = seed(&db, &tag).await;
        let scopes = vec!["openid".to_string()];
        let amr = vec!["pwd".to_string()];
        let subject = format!("pws_{tag}");

        // Control: the grant is active and a session is created.
        assert!(
            session_store::create(
                &db,
                &keys,
                &new_session(&person_id, &grant_id, &subject, &scopes, &amr),
            )
            .await
            .expect("create session")
            .is_some(),
            "an active grant refused to create a session"
        );

        db.execute(
            "UPDATE zeroship.grants \
             SET subject_status = 'suspended', suspended_at = NOW(), suspended_cause = 'test' \
             WHERE id = $1",
            &[&grant_id],
        )
        .await
        .expect("suspend");

        assert!(
            session_store::create(
                &db,
                &keys,
                &new_session(&person_id, &grant_id, &subject, &scopes, &amr),
            )
            .await
            .expect("create session")
            .is_none(),
            "a suspended grant created a session"
        );
    })
    .await;
}

/// A credential epoch that moved between the authenticating event and issuance
/// refuses. The control pins the epoch the person actually carries.
#[compio::test]
async fn a_stale_credential_epoch_cannot_create_a_session() {
    Database::run(async |database| {
        let db = database.connect_as_auth().await;
        let tag = tag();
        let keys = keys();
        let (person_id, _client_id, grant_id) = seed(&db, &tag).await;
        let scopes = vec!["openid".to_string()];
        let amr = vec!["pwd".to_string()];
        let subject = format!("pws_{tag}");

        let mut params = new_session(&person_id, &grant_id, &subject, &scopes, &amr);
        params.expected_credential_epoch = Some(0);
        assert!(
            session_store::create(&db, &keys, &params)
                .await
                .expect("create session")
                .is_some(),
            "the person's own credential epoch refused"
        );

        params.expected_credential_epoch = Some(1);
        assert!(
            session_store::create(&db, &keys, &params)
                .await
                .expect("create session")
                .is_none(),
            "a stale credential epoch created a session"
        );
    })
    .await;
}

/// A person whose credential epoch advances after issuance cannot rotate. This
/// is what makes "a password change kills every session" a data dependency
/// rather than an enumeration something has to remember to run.
#[compio::test]
async fn advancing_the_credential_epoch_stops_the_next_mint() {
    Database::run(async |database| {
        let db = database.connect_as_auth().await;
        let tag = tag();
        let keys = keys();
        let (person_id, _client_id, grant_id) = seed(&db, &tag).await;
        let scopes = vec!["openid".to_string()];
        let amr = vec!["pwd".to_string()];

        let created = session_store::create(
            &db,
            &keys,
            &new_session(&person_id, &grant_id, &format!("pws_{tag}"), &scopes, &amr),
        )
        .await
        .expect("create session")
        .expect("session created");
        let secret = created.secret.clone().expect("secret");

        db.execute(
            "UPDATE zeroship.users SET credential_version = credential_version + 1 WHERE id = $1",
            &[&person_id.as_str()],
        )
        .await
        .expect("advance credential epoch");

        let presented = session_store::peek(&db, &keys, &secret)
            .await
            .expect("peek")
            .expect("resolves");
        let rotation =
            session_store::rotate(&db, &keys, &presented, &scopes, IDLE_DAYS, IDEM_WINDOW_SECS)
                .await
                .expect("rotate");
        assert!(
            rotation.is_none(),
            "a session minted after its person's credential epoch moved"
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// Rotation, replay and reuse
// ---------------------------------------------------------------------------

/// The rotation algorithm end to end on one row: the secret advances, the
/// superseded one is served exactly one replay, and the second presentation of
/// the same superseded secret is refused.
#[compio::test]
async fn a_superseded_secret_replays_once_and_then_is_refused() {
    Database::run(async |database| {
        let db = database.connect_as_auth().await;
        let tag = tag();
        let keys = keys();
        let (person_id, _client_id, grant_id) = seed(&db, &tag).await;
        let scopes = vec!["openid".to_string(), "offline_access".to_string()];
        let amr = vec!["pwd".to_string()];

        let created = session_store::create(
            &db,
            &keys,
            &new_session(&person_id, &grant_id, &format!("pws_{tag}"), &scopes, &amr),
        )
        .await
        .expect("create session")
        .expect("session created");
        let first = created.secret.clone().expect("secret");

        let presented = session_store::peek(&db, &keys, &first)
            .await
            .expect("peek")
            .expect("resolves");
        let Some(RotatedSession { secret: second, .. }) =
            session_store::rotate(&db, &keys, &presented, &scopes, IDLE_DAYS, IDEM_WINDOW_SECS)
                .await
                .expect("rotate")
        else {
            panic!("rotation refused");
        };
        assert_ne!(first, second);

        // The superseded secret resolves to the same row, in the other slot.
        let superseded = session_store::peek(&db, &keys, &first)
            .await
            .expect("peek")
            .expect("superseded secret resolves");
        assert_eq!(superseded.slot(), SecretSlot::Superseded);
        assert_eq!(superseded.session_id, created.row.id);

        let locked = session_store::lock_and_read(&db, &created.row.id)
            .await
            .expect("lock")
            .expect("row");
        let replayed = session_store::replay(&db, &keys, &superseded, &locked)
            .await
            .expect("replay")
            .expect("the lost response replays once");
        assert_eq!(
            replayed.0.refresh_token, second,
            "the replay handed back a different successor than the rotation did"
        );

        // Single-use: the second presentation of the same superseded secret is not
        // served, which is what the caller turns into the reuse kill.
        let locked = session_store::lock_and_read(&db, &created.row.id)
            .await
            .expect("lock")
            .expect("row");
        assert!(
            session_store::replay(&db, &keys, &superseded, &locked)
                .await
                .expect("replay")
                .is_none(),
            "the idempotent record was served twice"
        );
    })
    .await;
}

/// The current secret must not be servable as a replay. Presenting the live
/// secret is a rotation, and a `replay` that accepted it would hand back a
/// cached response instead of advancing the row.
#[compio::test]
async fn the_live_secret_is_not_replayable() {
    Database::run(async |database| {
        let db = database.connect_as_auth().await;
        let tag = tag();
        let keys = keys();
        let (person_id, _client_id, grant_id) = seed(&db, &tag).await;
        let scopes = vec!["openid".to_string()];
        let amr = vec!["pwd".to_string()];

        let created = session_store::create(
            &db,
            &keys,
            &new_session(&person_id, &grant_id, &format!("pws_{tag}"), &scopes, &amr),
        )
        .await
        .expect("create session")
        .expect("session created");
        let first = created.secret.clone().expect("secret");
        let presented = session_store::peek(&db, &keys, &first)
            .await
            .expect("peek")
            .expect("resolves");
        let Some(RotatedSession { secret: second, .. }) =
            session_store::rotate(&db, &keys, &presented, &scopes, IDLE_DAYS, IDEM_WINDOW_SECS)
                .await
                .expect("rotate")
        else {
            panic!("rotation refused");
        };

        let live = session_store::peek(&db, &keys, &second)
            .await
            .expect("peek")
            .expect("resolves");
        assert_eq!(live.slot(), SecretSlot::Current);
        let locked = session_store::lock_and_read(&db, &created.row.id)
            .await
            .expect("lock")
            .expect("row");
        assert!(
            session_store::replay(&db, &keys, &live, &locked)
                .await
                .expect("replay")
                .is_none(),
            "the live secret was served as a replay"
        );
    })
    .await;
}

/// A session created without a secret can never be presented again. Its only
/// mint is the one its creating statement authorised, which is what makes an
/// exchange that was granted no `offline_access` a MINT-READS-ROW case rather
/// than an exception to it.
#[compio::test]
async fn a_session_with_no_secret_can_never_be_presented() {
    Database::run(async |database| {
        let db = database.connect_as_auth().await;
        let tag = tag();
        let keys = keys();
        let (person_id, _client_id, grant_id) = seed(&db, &tag).await;
        let scopes = vec!["openid".to_string()];
        let amr = vec!["pwd".to_string()];

        let subject = format!("pws_{tag}");
        let mut params = new_session(&person_id, &grant_id, &subject, &scopes, &amr);
        params.with_secret = false;
        let created = session_store::create(&db, &keys, &params)
            .await
            .expect("create session")
            .expect("session created");
        assert!(
            created.secret.is_none(),
            "a secretless session handed one back"
        );
        assert_eq!(created.proof.session_id(), created.row.id);

        let row = session_store::lock_and_read(&db, &created.row.id)
            .await
            .expect("lock")
            .expect("row");
        assert!(
            row.secret_key_version.is_none(),
            "a secretless session stored a key version"
        );
    })
    .await;
}

/// Revoking a person ends every live session they hold, in one statement, and
/// leaves an already-revoked one alone.
#[compio::test]
async fn revoking_a_person_ends_every_live_session() {
    Database::run(async |database| {
        let db = database.connect_as_auth().await;
        let tag = tag();
        let keys = keys();
        let (person_id, _client_id, grant_id) = seed(&db, &tag).await;
        let scopes = vec!["openid".to_string()];
        let amr = vec!["pwd".to_string()];
        let subject = format!("pws_{tag}");

        let first = session_store::create(
            &db,
            &keys,
            &new_session(&person_id, &grant_id, &subject, &scopes, &amr),
        )
        .await
        .expect("create")
        .expect("created");
        let second = session_store::create(
            &db,
            &keys,
            &new_session(&person_id, &grant_id, &subject, &scopes, &amr),
        )
        .await
        .expect("create")
        .expect("created");

        let revoked = session_store::revoke_person_sessions(&db, &person_id, "test")
            .await
            .expect("revoke person");
        assert_eq!(revoked, 2, "the person's live sessions were not all ended");

        for id in [&first.row.id, &second.row.id] {
            let row = session_store::lock_and_read(&db, id)
                .await
                .expect("lock")
                .expect("row");
            assert!(row.revoked_at.is_some(), "session {id} survived the revoke");
        }

        // A second pass ends nothing, because nothing is live.
        assert_eq!(
            session_store::revoke_person_sessions(&db, &person_id, "test")
                .await
                .expect("revoke person"),
            0
        );
    })
    .await;
}

/// The grant is one row per (person, audience) and its subject is written once.
/// A second consent advances the scopes and leaves the subject alone, which is
/// what stops a re-derivation silently re-identifying a returning person.
#[compio::test]
async fn a_second_consent_advances_scopes_and_never_rewrites_the_subject() {
    Database::run(async |database| {
        let db = database.connect_as_auth().await;
        let tag = tag();
        let (person_id, client_id, grant_id) = seed(&db, &tag).await;
        let audience = Audience::App {
            client_id: client_id.clone(),
        };

        let widened = vec![
            "openid".to_string(),
            "offline_access".to_string(),
            "email".to_string(),
        ];
        let again = session_store::upsert_grant(
            &db,
            &person_id,
            &audience,
            "pws_a_different_subject_entirely",
            &widened,
            Some("relay@zeroship.test"),
        )
        .await
        .expect("second consent");
        assert_eq!(
            again, grant_id,
            "a second consent minted a second grant row"
        );

        let rows = db
            .query(
                "SELECT subject, scopes, relay_email FROM zeroship.grants WHERE id = $1",
                &[&grant_id],
            )
            .await
            .expect("read grant");
        let row = rows.first().expect("grant row");
        let subject: String = row.get("subject");
        let scopes: Vec<String> = row.get("scopes");
        let relay: Option<String> = row.try_get("relay_email").ok().flatten();
        assert_eq!(subject, format!("pws_{tag}"), "the subject was rewritten");
        assert_eq!(scopes, widened, "the consent did not advance");
        assert_eq!(relay.as_deref(), Some("relay@zeroship.test"));

        // The platform audience is a DIFFERENT row for the same person, which is
        // what makes a suspension of deploy authority a separate act from a
        // suspension inside one app.
        let platform = session_store::upsert_grant(
            &db,
            &person_id,
            &Audience::Platform,
            person_id.as_str(),
            &["openid".to_string()],
            None,
        )
        .await
        .expect("platform grant");
        assert_ne!(platform, grant_id);
    })
    .await;
}
