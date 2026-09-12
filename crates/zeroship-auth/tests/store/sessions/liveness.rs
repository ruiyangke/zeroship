//! Session eligibility is checked again after an earlier successful mint.

use super::{keys, new_session, seed, tag, IDEM_WINDOW_SECS, IDLE_DAYS};
use crate::common::database::Database;
use compio_postgres::Client;
use uuid::Uuid;
use zeroship_auth::session_store::{self, SecretSlot};

#[derive(Clone, Copy, Debug)]
enum Change {
    Revoked,
    IdleExpired,
    AbsoluteExpired,
    CredentialChanged,
    GrantSuspended,
    Disabled,
    Anonymized,
    DeletionRequested,
    DeletionScheduled,
}

impl Change {
    const fn blocks_creation(self) -> bool {
        !matches!(
            self,
            Self::Revoked | Self::IdleExpired | Self::AbsoluteExpired
        )
    }

    async fn apply(self, db: &Client, person: Uuid, grant: &str, session: &str) {
        let changed = match self {
            Self::Revoked | Self::IdleExpired | Self::AbsoluteExpired => {
                let sql = match self {
                    Self::Revoked => "UPDATE zeroship.sessions SET revoked_at = NOW() WHERE id = $1",
                    Self::IdleExpired => "UPDATE zeroship.sessions SET idle_expires_at = NOW() - INTERVAL '1 minute' WHERE id = $1",
                    Self::AbsoluteExpired => "UPDATE zeroship.sessions SET absolute_expires_at = NOW() - INTERVAL '1 minute' WHERE id = $1",
                    _ => unreachable!(),
                };
                db.execute(sql, &[&session]).await
            }
            Self::GrantSuspended => db.execute(
                "UPDATE zeroship.grants SET subject_status = 'suspended' WHERE id = $1",
                &[&grant],
            ).await,
            _ => {
                let sql = match self {
                    Self::CredentialChanged => "UPDATE zeroship.users SET credential_version = credential_version + 1 WHERE id = $1",
                    Self::Disabled => "UPDATE zeroship.users SET disabled_at = NOW() WHERE id = $1",
                    Self::Anonymized => "UPDATE zeroship.users SET anonymized_at = NOW() WHERE id = $1",
                    Self::DeletionRequested => "UPDATE zeroship.users SET deletion_requested_at = NOW() WHERE id = $1",
                    Self::DeletionScheduled => "UPDATE zeroship.users SET deletion_scheduled_for = NOW() + INTERVAL '1 day' WHERE id = $1",
                    _ => unreachable!(),
                };
                db.execute(sql, &[&person]).await
            }
        }.expect("apply the lifecycle change");
        assert_eq!(changed, 1, "{self:?} must change the fixture row");
    }
}

#[allow(
    clippy::future_not_send,
    reason = "the fixture belongs to this compio runtime"
)]
#[allow(
    clippy::too_many_lines,
    reason = "keep each lifecycle change beside its successful controls and refusals"
)]
async fn check(change: Change, database: &Database) {
    let mut db = database.connect_as_auth().await;
    let keys = keys();
    let tag = tag();
    let (person, _, grant) = seed(&db, &tag).await;
    let subject = format!("pws_{tag}");
    let scopes = vec!["openid".into(), "offline_access".into()];
    let amr = vec!["pwd".into()];
    let mut params = new_session(person, &grant, &subject, &scopes, &amr);
    params.expected_credential_epoch = Some(
        db.query_one(
            "SELECT credential_version FROM zeroship.users WHERE id = $1",
            &[&person],
        )
        .await
        .unwrap()
        .get(0),
    );
    let created = session_store::create(&db, &keys, &params)
        .await
        .unwrap()
        .expect("an eligible account creates a session");
    let original = session_store::peek(&db, &keys, created.secret.as_ref().unwrap())
        .await
        .unwrap()
        .unwrap();
    let rotated =
        session_store::rotate(&db, &keys, &original, &scopes, IDLE_DAYS, IDEM_WINDOW_SECS)
            .await
            .unwrap()
            .expect("an eligible session rotates");
    let current = session_store::peek(&db, &keys, &rotated.secret)
        .await
        .unwrap()
        .unwrap();
    let superseded = session_store::peek(&db, &keys, created.secret.as_ref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.slot(), SecretSlot::Current);
    assert_eq!(superseded.slot(), SecretSlot::Superseded);

    // Successful controls are rolled back so the refusal sees the same secrets
    // and an unconsumed replay record.
    let control = db.transaction().await.unwrap();
    assert!(
        session_store::rotate(
            &control,
            &keys,
            &current,
            &scopes,
            IDLE_DAYS,
            IDEM_WINDOW_SECS
        )
        .await
        .unwrap()
        .is_some(),
        "{change:?}: rotation control"
    );
    control.rollback().await.unwrap();
    let control = db.transaction().await.unwrap();
    let locked = session_store::lock_and_read(&control, &created.row.id)
        .await
        .unwrap()
        .unwrap();
    let replay = session_store::replay(&control, &keys, &superseded, &locked)
        .await
        .unwrap()
        .expect("an eligible session replays");
    assert_eq!(replay.0.refresh_token, rotated.secret);
    control.rollback().await.unwrap();

    change.apply(&db, person, &grant, &created.row.id).await;
    if change.blocks_creation() {
        assert!(
            session_store::create(&db, &keys, &params)
                .await
                .unwrap()
                .is_none(),
            "{change:?}: ineligible account created a session"
        );
    }
    assert!(
        session_store::rotate(&db, &keys, &current, &scopes, IDLE_DAYS, IDEM_WINDOW_SECS)
            .await
            .unwrap()
            .is_none(),
        "{change:?}: ineligible session rotated"
    );
    let refusal = db.transaction().await.unwrap();
    let locked = session_store::lock_and_read(&refusal, &created.row.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        session_store::replay(&refusal, &keys, &superseded, &locked)
            .await
            .unwrap()
            .is_none(),
        "{change:?}: ineligible session replayed"
    );
    refusal.commit().await.unwrap();
}

#[compio::test]
async fn ended_sessions_refuse_rotation_and_replay() {
    Database::run(async |database| {
        for change in [
            Change::Revoked,
            Change::IdleExpired,
            Change::AbsoluteExpired,
        ] {
            check(change, database).await;
        }
    })
    .await;
}

#[compio::test]
async fn account_and_grant_changes_refuse_every_mint_path() {
    Database::run(async |database| {
        for change in [
            Change::CredentialChanged,
            Change::GrantSuspended,
            Change::Disabled,
            Change::Anonymized,
            Change::DeletionRequested,
            Change::DeletionScheduled,
        ] {
            check(change, database).await;
        }
    })
    .await;
}
