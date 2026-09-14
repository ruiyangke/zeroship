//! User identity constraints retain their structured database errors.

use crate::common::database::Database;
use zeroship_auth::store::users;
use zeroship_core::UserId;
use zeroship_data_orm::orm::{Entity, Insertable};

#[compio::test]
async fn user_repository_uses_native_ids_and_case_insensitive_email() {
    Database::run(async |database| {
        let orm = zeroship_auth::store::native::connect(database.auth_url().as_str())
            .await
            .unwrap();
        assert!(users::find_by_id(&orm, &UserId::mint())
            .await
            .unwrap()
            .is_none());
        assert!(users::find_by_email(&orm, "absent@example.test")
            .await
            .unwrap()
            .is_none());
        let created = users::create(&orm, "Creator@Example.test", "Creator", Some("phc"))
            .await
            .unwrap();
        let by_email = users::find_by_email(&orm, "CREATOR@EXAMPLE.TEST")
            .await
            .unwrap()
            .unwrap();
        let by_id = users::find_by_id(&orm, &created.id).await.unwrap().unwrap();
        assert_eq!(by_email.id, created.id);
        assert_eq!(by_id.email, created.email);
        assert_eq!(by_id.password_hash.as_deref(), Some("phc"));
        assert_eq!(by_id.credential_version, 0);
    })
    .await;
}

#[test]
fn native_user_insert_preserves_the_owned_id_buffer() {
    let id = UserId::mint();
    let buffer = id.as_str().as_ptr();
    let record = users::NewUser {
        id,
        email: "native@example.test",
        name: "Native caller",
        password_hash: None,
    }
    .into_record()
    .unwrap();
    assert_eq!(record["id"].as_str().unwrap().as_ptr(), buffer);
}

#[compio::test]
async fn native_user_rows_apply_the_callers_identity_conversion() {
    Database::run(async |database| {
        let native = zeroship_auth::store::native::connect(database.auth_url().as_str())
            .await
            .unwrap();
        use zeroship_auth::store::native::models::users as model;
        let id = UserId::mint();
        let created: users::UserRow = native
            .entity::<model::Entity>()
            .unwrap()
            .insert(users::NewUser {
                id: id.clone(),
                email: "native@example.test",
                name: "Native caller",
                password_hash: None,
            })
            .await
            .unwrap();
        assert_eq!(created.id, id);
        let user = native
            .entity::<model::Entity>()
            .unwrap()
            .query()
            .filter(model::id.eq(created.id.as_str()).unwrap())
            .first::<users::UserRow>()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.id, created.id);
        assert_eq!(user.email, created.email);
        assert_eq!(user.name, created.name);
        assert_eq!(model::Entity::COLLECTION, "users");
        let locked_until = chrono::Utc::now().timestamp_millis() + 60_000;
        let updated: Option<users::UserRow> = native
            .entity::<model::Entity>()
            .unwrap()
            .update(
                model::id.eq(id.as_str()).unwrap(),
                model::locked_until.set(Some(locked_until)).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            updated.unwrap().locked_until,
            chrono::DateTime::from_timestamp_millis(locked_until)
        );
    })
    .await;
}

#[compio::test]
async fn duplicate_email_preserves_the_existing_user_and_reports_unique_violation() {
    Database::run(async |database| {
        let pg = database.connect_as_auth().await;
        let orm = database.orm().await;
        let first = users::create(&orm, "creator@example.test", "Original creator", None)
            .await
            .unwrap();
        let error = users::create(&orm, "CREATOR@EXAMPLE.TEST", "Duplicate creator", None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            zeroship_data_orm::orm::DbError::UniqueViolation { .. }
        ));
        let retained = users::find_by_email(&orm, "creator@example.test")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.id, first.id);
        assert_eq!(retained.name, first.name);
        let count: i64 = pg
            .query_one("SELECT COUNT(*) FROM zeroship.users", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, 1);
    })
    .await;
}

#[derive(Debug, PartialEq)]
struct LoginState {
    failures: i32,
    locked_until: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: chrono::DateTime<chrono::Utc>,
    last_login_at: Option<chrono::DateTime<chrono::Utc>>,
    revision: String,
}

async fn login_state(pg: &compio_postgres::Client, id: &UserId) -> LoginState {
    let row = pg
        .query_one(
            "SELECT failed_login_count, locked_until, updated_at, last_login_at, xmin::text \
             FROM zeroship.users WHERE id = $1",
            &[&id.as_str()],
        )
        .await
        .unwrap();
    LoginState {
        failures: row.get(0),
        locked_until: row.get(1),
        updated_at: row.get(2),
        last_login_at: row.get(3),
        revision: row.get(4),
    }
}

async fn database_now(pg: &compio_postgres::Client) -> chrono::DateTime<chrono::Utc> {
    pg.query_one("SELECT clock_timestamp()", &[])
        .await
        .unwrap()
        .get(0)
}

#[compio::test]
async fn native_profile_initialization_preserves_existing_verification_and_avatar() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let pg = database.connect_as_auth().await;
        let user = users::create(&orm, "profile@example.test", "Profile", None)
            .await
            .unwrap();
        let before = database_now(&pg).await;
        users::mark_email_verified(&orm, &user.id).await.unwrap();
        let after = database_now(&pg).await;
        users::set_avatar_if_missing(&orm, &user.id, "https://example.test/avatar")
            .await
            .unwrap();
        let verified: chrono::DateTime<chrono::Utc> = pg
            .query_one(
                "SELECT email_verified_at FROM zeroship.users WHERE id = $1",
                &[&user.id.as_str()],
            )
            .await
            .unwrap()
            .get(0);
        assert!(verified >= before && verified <= after);
        let initialized = login_state(&pg, &user.id).await;
        users::mark_email_verified(&orm, &user.id).await.unwrap();
        users::set_avatar_if_missing(&orm, &user.id, "https://example.test/replacement")
            .await
            .unwrap();
        assert_eq!(login_state(&pg, &user.id).await, initialized);
        let stored = pg
            .query_one(
                "SELECT email_verified_at, avatar_url FROM zeroship.users WHERE id = $1",
                &[&user.id.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(stored.get::<_, chrono::DateTime<chrono::Utc>>(0), verified);
        assert_eq!(stored.get::<_, String>(1), "https://example.test/avatar");
        let absent = UserId::mint();
        users::mark_email_verified(&orm, &absent).await.unwrap();
        users::set_avatar_if_missing(&orm, &absent, "https://example.test/absent")
            .await
            .unwrap();
        assert!(users::find_by_id(&orm, &absent).await.unwrap().is_none());
    })
    .await;
}

#[compio::test]
async fn native_login_failures_increment_apply_backoff_and_cap_the_lock() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let pg = database.connect_as_auth().await;
        let user = users::create(&orm, "lockout@example.test", "Lockout", None)
            .await
            .unwrap();
        for expected in 1..users::lockout::THRESHOLD {
            assert_eq!(
                users::record_login_failure(&orm, &user.id).await.unwrap(),
                expected
            );
            let state = login_state(&pg, &user.id).await;
            assert_eq!(state.failures, expected);
            assert!(state.locked_until.is_none());
        }
        for (expected, seconds) in [
            (
                users::lockout::THRESHOLD,
                users::lockout::INITIAL_BACKOFF_SECS,
            ),
            (
                users::lockout::THRESHOLD + 1,
                users::lockout::INITIAL_BACKOFF_SECS * 2,
            ),
        ] {
            let before = database_now(&pg).await;
            assert_eq!(
                users::record_login_failure(&orm, &user.id).await.unwrap(),
                expected
            );
            let after = database_now(&pg).await;
            let state = login_state(&pg, &user.id).await;
            assert_eq!(state.failures, expected);
            let locked_until = state.locked_until.expect("threshold must lock the account");
            let backoff = chrono::Duration::seconds(seconds);
            assert!(locked_until >= before + backoff);
            assert!(locked_until <= after + backoff);
        }
        let capped_count = users::lockout::THRESHOLD + i32::try_from(i32::BITS).unwrap();
        pg.execute(
            "UPDATE zeroship.users SET failed_login_count = $2 WHERE id = $1",
            &[&user.id.as_str(), &capped_count],
        )
        .await
        .unwrap();
        let before = database_now(&pg).await;
        assert_eq!(
            users::record_login_failure(&orm, &user.id).await.unwrap(),
            capped_count + 1
        );
        let after = database_now(&pg).await;
        let until = login_state(&pg, &user.id).await.locked_until.unwrap();
        let cap = chrono::Duration::seconds(users::lockout::MAX_BACKOFF_SECS);
        assert!(until >= before + cap);
        assert!(until <= after + cap);
    })
    .await;
}

#[compio::test]
async fn native_login_failure_rolls_back_the_counter_when_the_deadline_write_fails() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let admin = database.connect().await;
        let user = users::create(&orm, "rollback-lock@example.test", "Rollback lock", None)
            .await
            .unwrap();
        let initial = users::lockout::THRESHOLD - 1;
        admin.execute(
            "UPDATE zeroship.users SET failed_login_count = $2 WHERE id = $1",
            &[&user.id.as_str(), &initial],
        ).await.unwrap();
        admin.batch_execute(
            "ALTER TABLE zeroship.users ADD CONSTRAINT reject_lock_deadline CHECK (locked_until IS NULL)",
        ).await.unwrap();
        let before = login_state(&admin, &user.id).await;
        assert!(users::record_login_failure(&orm, &user.id).await.is_err());
        assert_eq!(login_state(&admin, &user.id).await, before);
    }).await;
}

#[compio::test]
async fn native_login_reset_clears_dirty_state_and_clean_or_absent_accounts_are_untouched() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let pg = database.connect_as_auth().await;
        let user = users::create(&orm, "reset-state@example.test", "Reset state", None)
            .await
            .unwrap();
        let clean = login_state(&pg, &user.id).await;
        users::reset_login_failures(&orm, &user.id).await.unwrap();
        assert_eq!(login_state(&pg, &user.id).await, clean);
        for (failures, locked) in [(1, false), (0, true), (users::lockout::THRESHOLD, true)] {
            pg.execute(
                "UPDATE zeroship.users SET failed_login_count = $2, \
                 locked_until = CASE WHEN $3 THEN NOW() + INTERVAL '1 hour' ELSE NULL END, \
                 updated_at = NOW() - INTERVAL '1 day' WHERE id = $1",
                &[&user.id.as_str(), &failures, &locked],
            )
            .await
            .unwrap();
            let dirty = login_state(&pg, &user.id).await;
            users::reset_login_failures(&orm, &user.id).await.unwrap();
            let reset = login_state(&pg, &user.id).await;
            assert_eq!(reset.failures, 0);
            assert!(reset.locked_until.is_none());
            assert!(reset.updated_at > dirty.updated_at);
            assert_ne!(reset.revision, dirty.revision);
            users::reset_login_failures(&orm, &user.id).await.unwrap();
            assert_eq!(login_state(&pg, &user.id).await, reset);
        }
        let baseline = login_state(&pg, &user.id).await;
        let absent = UserId::mint();
        assert_eq!(users::record_login_failure(&orm, &absent).await.unwrap(), 0);
        users::record_login_failure_dummy(&orm).await.unwrap();
        users::reset_login_failures(&orm, &absent).await.unwrap();
        users::touch_last_login(&orm, &absent).await.unwrap();
        assert_eq!(login_state(&pg, &user.id).await, baseline);
        assert!(users::find_by_id(&orm, &absent).await.unwrap().is_none());
    })
    .await;
}

#[compio::test]
async fn native_last_login_uses_the_database_clock_without_changing_lockout_state() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let pg = database.connect_as_auth().await;
        let user = users::create(&orm, "last-login@example.test", "Last login", None)
            .await
            .unwrap();
        pg.execute(
            "UPDATE zeroship.users SET failed_login_count = $2, \
             locked_until = NOW() + INTERVAL '1 hour', \
             last_login_at = NOW() - INTERVAL '1 day' WHERE id = $1",
            &[&user.id.as_str(), &users::lockout::THRESHOLD],
        )
        .await
        .unwrap();
        let original = login_state(&pg, &user.id).await;
        let before = database_now(&pg).await;
        users::touch_last_login(&orm, &user.id).await.unwrap();
        let after = database_now(&pg).await;
        let touched = login_state(&pg, &user.id).await;
        let last_login = touched.last_login_at.unwrap();
        assert!(last_login >= before && last_login <= after);
        assert!(last_login > original.last_login_at.unwrap());
        assert_eq!(touched.failures, original.failures);
        assert_eq!(touched.locked_until, original.locked_until);
    })
    .await;
}

#[compio::test]
async fn concurrent_native_login_failures_preserve_increments_and_the_longest_lock() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let pg = database.connect_as_auth().await;
        let user = users::create(
            &orm,
            "concurrent-lock@example.test",
            "Concurrent lock",
            None,
        )
        .await
        .unwrap();
        let initial = users::lockout::THRESHOLD - 1;
        pg.execute(
            "UPDATE zeroship.users SET failed_login_count = $2 WHERE id = $1",
            &[&user.id.as_str(), &initial],
        )
        .await
        .unwrap();
        let before = database_now(&pg).await;
        let url = database.auth_url().to_string();
        let id = user.id.clone();
        let workers = 4;
        let attempts = 3;
        let mut counts = compio::runtime::spawn_blocking(move || {
            let start = std::sync::Arc::new(std::sync::Barrier::new(workers));
            let threads: Vec<_> = (0..workers)
                .map(|_| {
                    let start = start.clone();
                    let url = url.clone();
                    let id = id.clone();
                    std::thread::spawn(move || {
                        start.wait();
                        compio::runtime::Runtime::new().unwrap().block_on(async {
                            let orm = zeroship_auth::store::native::connect(&url).await.unwrap();
                            let mut counts = Vec::new();
                            for _ in 0..attempts {
                                counts.push(users::record_login_failure(&orm, &id).await.unwrap());
                            }
                            counts
                        })
                    })
                })
                .collect();
            threads
                .into_iter()
                .flat_map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        })
        .await
        .unwrap();
        counts.sort_unstable();
        let final_count = initial + i32::try_from(workers * attempts).unwrap();
        assert_eq!(counts, ((initial + 1)..=final_count).collect::<Vec<_>>());
        let state = login_state(&pg, &user.id).await;
        assert_eq!(state.failures, final_count);
        let longest = users::lockout::backoff_secs(final_count).unwrap();
        assert!(state.locked_until.unwrap() >= before + chrono::Duration::seconds(longest));
    })
    .await;
}
