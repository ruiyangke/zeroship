//! User identity constraints retain their structured database errors.

use crate::common::database::{eventually, Database};
use futures::channel::oneshot;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use zeroship_auth::store::users;
use zeroship_core::UserId;
use zeroship_data_orm::orm::{Entity, Insertable, UtcInstant};

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
        let locked_until = UtcInstant::from_unix_millis(
            chrono::Utc::now().timestamp_millis() + 60_000,
        )
        .unwrap();
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
            updated.unwrap().locked_until.unwrap().timestamp_micros(),
            locked_until.unix_micros()
        );
    })
    .await;
}

/// An instant written with a microsecond fraction reads back through the
/// model's own conversion as exactly that instant.
///
/// The raw connection is the oracle: it writes the value and reports the
/// microsecond count the server holds, so the assertion never restates what the
/// conversion is supposed to produce. A decode that floored to the containing
/// millisecond would disagree with the oracle on every row here.
#[compio::test]
async fn native_user_rows_keep_the_microseconds_the_server_stored() {
    Database::run(async |database| {
        let pg = database.connect_as_auth().await;
        let orm = database.orm().await;
        use zeroship_auth::store::native::models::users as model;
        let created = users::create(&orm, "micros@example.test", "Micros", None)
            .await
            .unwrap();
        for fraction in ["000001", "999999", "500000"] {
            let stamp = format!("2026-05-07 01:02:03.{fraction}+00");
            let oracle: i64 = pg
                .query_one(
                    "UPDATE zeroship.users SET locked_until = $2::timestamptz WHERE id = $1 \
                     RETURNING (extract(epoch FROM locked_until) * 1000000)::bigint",
                    &[&created.id.as_str(), &stamp],
                )
                .await
                .unwrap()
                .get(0);
            let row = users::find_by_id(&orm, &created.id).await.unwrap().unwrap();
            assert_eq!(
                row.locked_until.unwrap().timestamp_micros(),
                oracle,
                "{stamp}"
            );
            // The value read back re-binds to its own row, which is what a
            // reservation token compared for equality depends on.
            let matched = orm
                .entity::<model::Entity>()
                .unwrap()
                .query()
                .filter(
                    model::locked_until
                        .eq(Some(
                            UtcInstant::from_unix_micros(
                                row.locked_until.unwrap().timestamp_micros(),
                            )
                            .unwrap(),
                        ))
                        .unwrap(),
                )
                .count()
                .await
                .unwrap();
            assert_eq!(matched, 1, "{stamp}");
        }
        // Control: an instant one microsecond away matches no row, so the
        // equality above is the stored value and not a coarse comparison.
        let row = users::find_by_id(&orm, &created.id).await.unwrap().unwrap();
        let skewed =
            UtcInstant::from_unix_micros(row.locked_until.unwrap().timestamp_micros() + 1).unwrap();
        assert_eq!(
            orm.entity::<model::Entity>()
                .unwrap()
                .query()
                .filter(model::locked_until.eq(Some(skewed)).unwrap())
                .count()
                .await
                .unwrap(),
            0
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
async fn native_dummy_login_failure_issues_the_update_of_a_real_failure() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let admin = database.connect().await;
        let user = users::create(&orm, "dummy-failure@example.test", "Dummy failure", None)
            .await
            .unwrap();
        // Statement triggers fire even when an UPDATE matches no row. The
        // counter is fixture state, so the definer owns every write to it.
        admin
            .batch_execute(
                "CREATE SCHEMA fixture; \
                 CREATE TABLE fixture.user_updates ( \
                   statement bigserial PRIMARY KEY, changed_rows bigint NOT NULL); \
                 CREATE FUNCTION fixture.record_user_update() RETURNS trigger \
                 LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$ \
                 BEGIN \
                   INSERT INTO fixture.user_updates (changed_rows) SELECT count(*) FROM changed; \
                   RETURN NULL; \
                 END $$; \
                 CREATE TRIGGER record_user_update AFTER UPDATE ON zeroship.users \
                 REFERENCING NEW TABLE AS changed FOR EACH STATEMENT \
                 EXECUTE FUNCTION fixture.record_user_update()",
            )
            .await
            .unwrap();

        users::find_by_id(&orm, &user.id).await.unwrap().unwrap();
        users::find_by_email(&orm, "dummy-failure@example.test")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            take_user_updates(&admin).await,
            Vec::<i64>::new(),
            "reads must not register as user updates"
        );

        assert_eq!(
            users::record_login_failure(&orm, &user.id).await.unwrap(),
            1
        );
        let real = take_user_updates(&admin).await;
        assert_eq!(
            real,
            [1],
            "a failure below the threshold updates only its counter"
        );

        let baseline = login_state(&admin, &user.id).await;
        users::record_login_failure_dummy(&orm).await.unwrap();
        let dummy = take_user_updates(&admin).await;
        assert_eq!(
            dummy.len(),
            real.len(),
            "the dummy must issue the user updates of a real failure: {dummy:?}"
        );
        assert_eq!(dummy, [0], "the dummy update must match no stored user");
        assert_eq!(login_state(&admin, &user.id).await, baseline);
    })
    .await;
}

/// Changed-row counts of the user UPDATE statements recorded since the last call.
async fn take_user_updates(admin: &compio_postgres::Client) -> Vec<i64> {
    admin
        .query(
            "WITH taken AS (DELETE FROM fixture.user_updates RETURNING statement, changed_rows) \
             SELECT changed_rows FROM taken ORDER BY statement",
            &[],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect()
}

/// Session names that the fixture's trigger and activity probes use to tell
/// concurrent login failures apart.
const FIRST_CALLER: &str = "lockout-first";
const LATER_CALLER: &str = "lockout-later";
/// Advisory lock the fixture holds to pause the first caller's deadline write.
const DEADLINE_GATE: i64 = 41_001;

/// One login failure on its own thread, compio runtime and connection pool,
/// connected before it is started.
struct Caller {
    start: Option<oneshot::Sender<()>>,
    finished: Arc<AtomicBool>,
    count: oneshot::Receiver<i32>,
}

impl Caller {
    #[allow(
        clippy::future_not_send,
        reason = "fixture channels are awaited on this compio runtime"
    )]
    async fn connect(database: &Database, name: &str, id: &UserId) -> Self {
        let url = database.auth_url_named(name).to_string();
        let id = id.clone();
        let (connected, ready) = oneshot::channel();
        let (start, started) = oneshot::channel::<()>();
        let (recorded, count) = oneshot::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        std::thread::spawn(move || {
            let runtime = compio::runtime::Runtime::new().unwrap();
            let outcome = runtime.block_on(async move {
                let orm = zeroship_auth::store::native::connect(&url).await.unwrap();
                connected.send(()).unwrap();
                started.await.ok()?;
                let count = users::record_login_failure(&orm, &id).await.unwrap();
                done.store(true, Ordering::SeqCst);
                Some(count)
            });
            drop(runtime);
            if let Some(count) = outcome {
                let _ = recorded.send(count);
            }
        });
        ready.await.expect("the caller connects its repository");
        Self {
            start: Some(start),
            finished,
            count,
        }
    }

    fn start(&mut self) {
        self.start
            .take()
            .expect("each caller starts once")
            .send(())
            .unwrap();
    }

    fn finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
}

/// Backends of callers under `name` that wait on a lock.
async fn lock_waiters(observer: &compio_postgres::Client, name: &str) -> i64 {
    observer
        .query_one(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND application_name = $1 \
               AND wait_event_type = 'Lock'",
            &[&name],
        )
        .await
        .unwrap()
        .get(0)
}

/// Whether the first caller waits at the deadline gate, and how many later
/// callers' backends the lock manager reports as blocked.
async fn deadline_pause(observer: &compio_postgres::Client) -> (bool, usize) {
    let row = observer
        .query_one(
            "SELECT count(*) FILTER (WHERE application_name = $1 \
                      AND wait_event_type = 'Lock' AND wait_event = 'advisory') = 1, \
                    count(*) FILTER (WHERE application_name = $2 \
                      AND cardinality(pg_blocking_pids(pid)) > 0) \
             FROM pg_stat_activity WHERE datname = current_database()",
            &[&FIRST_CALLER, &LATER_CALLER],
        )
        .await
        .unwrap();
    (row.get(0), usize::try_from(row.get::<_, i64>(1)).unwrap())
}

#[compio::test]
async fn concurrent_native_login_failures_preserve_increments_and_the_longest_lock() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let admin = database.connect().await;
        let user = users::create(
            &orm,
            "concurrent-lock@example.test",
            "Concurrent lock",
            None,
        )
        .await
        .unwrap();
        let initial = users::lockout::THRESHOLD - 1;
        let callers = 4;
        let final_count = initial + callers;
        let longest = users::lockout::backoff_secs(final_count).unwrap();
        let previous = users::lockout::backoff_secs(final_count - 1).unwrap();
        assert!(
            longest > previous && longest < users::lockout::MAX_BACKOFF_SECS,
            "the final count must lock below the cap and longer than the count before it"
        );
        admin
            .execute(
                "UPDATE zeroship.users SET failed_login_count = $2 WHERE id = $1",
                &[&user.id.as_str(), &initial],
            )
            .await
            .unwrap();
        // Pause the deadline write of the caller that records the first
        // failure. It is a statement trigger, so it runs before that write
        // touches the row.
        admin
            .batch_execute(&format!(
                "CREATE SCHEMA fixture; \
                 CREATE FUNCTION fixture.pause_first_deadline() RETURNS trigger \
                 LANGUAGE plpgsql AS $$ \
                 BEGIN \
                   IF current_setting('application_name') = '{FIRST_CALLER}' THEN \
                     PERFORM pg_advisory_xact_lock({DEADLINE_GATE}); \
                   END IF; \
                   RETURN NULL; \
                 END $$; \
                 CREATE TRIGGER pause_first_deadline \
                 BEFORE UPDATE OF locked_until ON zeroship.users \
                 FOR EACH STATEMENT EXECUTE FUNCTION fixture.pause_first_deadline()"
            ))
            .await
            .unwrap();
        let gate = database.connect().await;
        gate.query_one("SELECT pg_advisory_lock($1)", &[&DEADLINE_GATE])
            .await
            .unwrap();
        let mut holder = database.connect().await;
        let hold = holder.transaction().await.unwrap();
        hold.query_one(
            "SELECT id FROM zeroship.users WHERE id = $1 FOR UPDATE",
            &[&user.id.as_str()],
        )
        .await
        .unwrap();

        let mut first = Caller::connect(database, FIRST_CALLER, &user.id).await;
        let mut later = Vec::new();
        for _ in 1..callers {
            later.push(Caller::connect(database, LATER_CALLER, &user.id).await);
        }
        // The caller that queues first on the held row is the first to update
        // it once the hold commits.
        first.start();
        let first_queued = eventually(async || lock_waiters(&admin, FIRST_CALLER).await == 1).await;
        for caller in &mut later {
            caller.start();
        }
        let all_queued = eventually(async || {
            lock_waiters(&admin, FIRST_CALLER).await + lock_waiters(&admin, LATER_CALLER).await
                == i64::from(callers)
        })
        .await;
        let before = database_now(&admin).await;
        hold.commit().await.unwrap();
        // While the first deadline write waits, a failure written in one
        // transaction still holds the row, so later callers stay queued. A
        // deadline written outside that transaction lets them finish first.
        let settled = eventually(async || {
            let (paused, blocked) = deadline_pause(&admin).await;
            paused && (blocked == later.len() || later.iter().all(Caller::finished))
        })
        .await;
        gate.query_one("SELECT pg_advisory_unlock($1)", &[&DEADLINE_GATE])
            .await
            .unwrap();
        let first_count = first
            .count
            .await
            .expect("the first caller records its failure");
        let mut counts = vec![first_count];
        for caller in later {
            counts.push(
                caller
                    .count
                    .await
                    .expect("a later caller records its failure"),
            );
        }
        let after = database_now(&admin).await;

        assert!(first_queued, "the first caller must queue on the held row");
        assert!(all_queued, "every caller must queue on the held row");
        assert!(
            settled,
            "the first deadline write must pause until the later callers settle"
        );
        assert_eq!(first_count, initial + 1);
        counts.sort_unstable();
        assert_eq!(counts, ((initial + 1)..=final_count).collect::<Vec<_>>());
        let state = login_state(&admin, &user.id).await;
        assert_eq!(state.failures, final_count);
        let locked_until = state
            .locked_until
            .expect("the final failure locks the account");
        // Inside this window an earlier count's deadline ends before the
        // lower bound, so the bounds identify the count that wrote it.
        assert!(
            after - before < chrono::Duration::seconds(longest - previous),
            "the run must be shorter than the gap between the last two backoffs"
        );
        let backoff = chrono::Duration::seconds(longest);
        assert!(
            locked_until >= before + backoff,
            "the deadline must belong to the final count: {locked_until} < {}",
            before + backoff
        );
        assert!(
            locked_until <= after + backoff,
            "the deadline must belong to the final count: {locked_until} > {}",
            after + backoff
        );
    })
    .await;
}
