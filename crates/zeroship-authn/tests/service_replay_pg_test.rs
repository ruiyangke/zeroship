//! The Postgres `jti` store, raced across two connections.
//!
//! An in-process race cannot prove what this mechanism needs. The claim has to
//! be settled by something every REPLICA of the callee shares, so the test that
//! matters runs two verifiers over two separate database connections - the
//! closest thing to two replicas a single test process can be - and races them
//! on ONE assertion.
//!
//! Every race case here is paired with a negative control that differs in one
//! variable: the same two connections, the same single assertion, a store that
//! reads and only then writes. It admits BOTH. Without that pairing a green
//! "exactly one succeeded" is equally consistent with a harness that never
//! interleaved, which would make the whole test vacuous.
//!
//! Announces a skip when there is no test database, and that skip is a FAILURE in
//! the gate that provisions the database: `tests/run_auth_suite.sh` exports the
//! DSN and then fails on any announcement not named in its allowlist. This is
//! not one of them. There is no environment variable that changes the verdict -
//! `ZEROSHIP_REQUIRE_LIVE_BACKENDS=1` used to, and being opt-in it was set by
//! everyone except the person whose run it would have saved.
//!
//! # What makes this hermetic, and what it costs
//!
//! The table is SHARED - `service_authn.service_assertion_replay`, unqualified,
//! established below and reused by every run against a given database. Nothing
//! truncates it between runs. That is deliberate and it is safe, because every
//! key this file writes is fresh per run rather than per fixture: the race and
//! replay tests key on `iss|jti` where `jti` is 128 random bits from
//! `new_jti()`, and the rest carry [`unique_suffix`]. No run can see another
//! run's key, so no neighbouring row can decide any verdict here.
//!
//! MEASURED, 2026-08-21, on the shared 5440 server: 74 databases carry this
//! table and the largest holds 9 rows, which is exactly one run's residue - one
//! per race, one per sequential replay, `live` and `stale`, and one per granted
//! role. Back-to-back runs against ONE database do stack (9, 18, 27, 36), for
//! the retention window only: rows outlive their run by at most 120 seconds and
//! the next run past that window sweeps them. The residue is bounded by the
//! window, not by the number of runs, which is why every long-lived database on
//! that server sits at 9 or 0 rather than in the thousands.
//!
//! The one number here that a neighbour DOES contaminate is the count
//! `purge_expired` returns, and nothing asserts on it; see the sweep arm of
//! [`a_live_claim_blocks_a_replay_and_an_expired_one_does_not`] for why it
//! cannot be asserted on and what is checked instead.

#![allow(clippy::future_not_send)]

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use compio_postgres::{Client, NoTls};
use zeroship_authn::service_replay::PostgresReplayStore;
use zeroship_core::service_assertion::{
    ClaimFuture, ReplayClaim, ReplayStore, ReplayStoreError, ServiceAssertionMinter,
    ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey, ServiceTrustBundle,
};
use zeroship_core::service_identity::{verify_identity, AuthError, PeerCredentials};

const CALLER: &str = "spiffe://zeroship.ai/svc/gateway";
const CALLEE: &str = "spiffe://zeroship.ai/svc/control";
const CALLER_KID: &str = "gateway-replay-test";

/// The table this store reads, spelled the way the migration spells it.
///
/// Production establishes it from
/// `db/migrations-ts/20260816000100_service_assertion_replay.ts`. This is a
/// TEST FIXTURE and runs under the privileged test DSN; a service role can
/// neither create it nor needs to. The two spellings can drift, which is a real
/// cost of testing against a database the suite provisions itself rather than
/// one built by migrations alone.
const FIXTURE_DDL: &str = "CREATE SCHEMA IF NOT EXISTS service_authn; \
     CREATE TABLE IF NOT EXISTS service_authn.service_assertion_replay ( \
         replay_key text PRIMARY KEY, \
         expires_at timestamptz NOT NULL \
     ); \
     CREATE INDEX IF NOT EXISTS service_assertion_replay_expiry_idx \
         ON service_authn.service_assertion_replay (expires_at)";

/// The migration, read at compile time so its GRANT cannot drift from here.
///
/// The DDL above is hand-written and CAN drift; that is stated at
/// [`FIXTURE_DDL`] and remains true. The GRANT does not have to, and it is the
/// half that went wrong: the shipped migration granted insert, update and
/// delete and argued SELECT away, and every statement this store issues reads
/// `expires_at` in a condition, so all four roles were denied both of them.
/// Nothing here could see that, because the fixture connects as the superuser.
///
/// So the privilege list and the role list are parsed out of the migration and
/// applied verbatim, and [`every_granted_role_can_run_the_stores_own_statements`]
/// runs the store as each of those roles. Take `select` back out of the
/// migration and that test goes red with the Postgres permission error.
const MIGRATION_SOURCE: &str =
    include_str!("../../../db/migrations-ts/20260816000100_service_assertion_replay.ts");

/// A role that is deliberately NOT in the migration's grant list.
///
/// [`fixture_grant_sql`] gives it `usage` on the schema and nothing at all on
/// this table, so a statement it issues fails on the TABLE privilege rather
/// than on reaching the schema - which is what makes it a driver-error fixture
/// and not a differently-shaped one. In production it is the creator-app login,
/// which runs no verifier and so is granted nothing here.
const UNGRANTED_ROLE: &str = "zeroship_app";

/// The one `grant(...)` call in the migration whose target is the replay TABLE.
///
/// The migration also carries a schema-level `usage` grant, and both calls spell
/// `privileges: [` and `to: [`. Taking the first match in the file would read
/// `["usage"]` as the table privileges and still produce runnable SQL, so the
/// call is selected by its `kind: "table"` target rather than by position.
fn table_grant_call() -> &'static str {
    MIGRATION_SOURCE
        .split("grant({")
        .find(|call| call.contains("kind: \"table\""))
        .expect("the migration grants privileges on a table")
}

/// Pull the quoted strings out of the first `<marker>...]` list in `source`.
fn quoted_list_after_in(source: &str, marker: &str) -> Vec<String> {
    let (_, tail) = source
        .split_once(marker)
        .unwrap_or_else(|| panic!("the source contains {marker:?}"));
    let (list, _) = tail
        .split_once(']')
        .unwrap_or_else(|| panic!("the list after {marker:?} is closed"));
    let items: Vec<String> = list
        .split('"')
        .skip(1)
        .step_by(2)
        .map(ToOwned::to_owned)
        .collect();
    assert!(!items.is_empty(), "the list after {marker:?} is not empty");
    items
}

/// The roles the migration grants on the replay table.
fn granted_roles() -> Vec<String> {
    quoted_list_after_in(table_grant_call(), "to: [")
}

/// The migration's own GRANT, re-expressed as SQL against the fixture table.
///
/// The schema-level `usage` is a fixture concern rather than a parsed one: a
/// database this suite provisions itself has no
/// `db/migrations-ts/20260702000900_grants.ts` to have run, so without it every
/// role below fails on reaching the schema and the table privileges are never
/// exercised at all. [`UNGRANTED_ROLE`] is included for exactly that reason -
/// its failure has to be about the TABLE.
fn fixture_grant_sql() -> String {
    let granted = granted_roles();
    let mut reach_the_schema = granted.clone();
    reach_the_schema.push(UNGRANTED_ROLE.to_owned());
    format!(
        "GRANT usage ON SCHEMA service_authn TO {}; \
         GRANT {} ON service_authn.service_assertion_replay TO {}",
        reach_the_schema.join(", "),
        quoted_list_after_in(table_grant_call(), "privileges: [").join(", "),
        granted.join(", "),
    )
}

/// Serialises the fixture DDL across concurrent test threads and processes.
///
/// MEASURED, when this file held four tests: without it, the first run against
/// a database that does not yet have the table fails 3 of 4, because cargo runs
/// the tests on a thread each and `CREATE TABLE IF NOT EXISTS` is not
/// concurrency-safe - the existence check and the create are not atomic, so the
/// losers raise a duplicate-key error on the catalogue. The second run passed 4
/// of 4 and every run after it did too, which is exactly the shape that gets a
/// fixture bug mistaken for a flake and then ignored: it only ever fails on a
/// fresh database.
///
/// The file holds six tests now, so the lock serialises six threads rather than
/// four and matters MORE, not less. RE-MEASURED 2026-08-21, both arms, against a
/// database whose `service_authn` schema had just been dropped: with the lock, 6
/// of 6 on the first run; with only this one statement replaced by a `SELECT 1`
/// and nothing else changed, 0 of 6. It has gone from losing 3 of 4 to losing
/// every test, because a sixth racer is a sixth chance for the catalogue insert
/// to collide.
const FIXTURE_LOCK: i64 = 7_523_000_001;

/// The live `PostgreSQL` the jti store under test lives in.
///
/// # Panics
///
/// When neither `PG_TEST_URL` nor the test overlay names one, with the
/// provisioning command. Every arm here used to announce a skip instead - and
/// this file's whole point is that two replicas racing one assertion admit
/// exactly one, which nothing but a real server can settle.
fn db_url() -> String {
    zeroship_core::config::test_database_url()
}

/// Establish the replay table exactly once, whoever gets there first.
async fn ensure_fixture(client: &Client) {
    client
        .execute("SELECT pg_advisory_lock($1)", &[&FIXTURE_LOCK])
        .await
        .expect("take the fixture lock");
    let established = client
        .batch_execute(FIXTURE_DDL)
        .await
        .and(client.batch_execute(&fixture_grant_sql()).await);
    client
        .execute("SELECT pg_advisory_unlock($1)", &[&FIXTURE_LOCK])
        .await
        .expect("release the fixture lock");
    established.expect("establish the replay table fixture");
}

/// A connection whose privileges are exactly one role's.
///
/// MEASURED, on a scratch database built by `zeroship-platform-migrate` from
/// `db/migrations-ts`: a superuser session that has `SET ROLE` to a
/// non-superuser really is privilege-checked as that role - before the grant
/// was fixed, this exact probe returned `permission denied for table
/// service_assertion_replay` for all four granted roles, and after it all of
/// them succeeded. So this is a genuine least-privilege session and not a
/// superuser wearing a label.
async fn client_as_role(url: &str, role: &str) -> Client {
    let client = connect(url).await;
    ensure_fixture(&client).await;
    client
        .execute(&format!("SET ROLE {role}"), &[])
        .await
        .unwrap_or_else(|error| panic!("assume the role {role}: {error}"));
    client
}

async fn connect(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .expect("connect to the test database");
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("[service_replay_pg_test] connection driver: {error}");
        }
    })
    .detach();
    client
}

/// A distinct key per run, so repeated runs against one database never collide.
fn unique_suffix() -> String {
    use rand::RngCore as _;
    let mut bytes = [0_u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn issuer(uri: &str) -> ServiceIssuer {
    ServiceIssuer::parse(uri).expect("a well-formed issuer identifier")
}

/// Whether the table holds a row for `key`, read on a connection of its own.
///
/// The store cannot answer this and should not be able to: `claim` reports
/// `Accepted` for an absent key and for an expired one alike, which is the
/// whole point of the reclaim arm. Asking the table directly is the only way to
/// tell "the sweep deleted it" from "the sweep left it and the next claim
/// reclaimed it".
async fn row_exists(client: &Client, key: &str) -> bool {
    !client
        .query(
            "SELECT 1 FROM service_authn.service_assertion_replay WHERE replay_key = $1",
            &[&key],
        )
        .await
        .expect("read the replay table")
        .is_empty()
}

/// The WRONG shape, over real connections: SELECT, then INSERT.
///
/// The negative control for every race below. It is not an implementation
/// anything ships.
struct ReadThenWriteStore {
    client: Client,
}

impl ReplayStore for ReadThenWriteStore {
    fn claim<'a>(&'a self, key: &'a str, expires_at: SystemTime) -> ClaimFuture<'a> {
        Box::pin(async move {
            let seen = self
                .client
                .query(
                    "SELECT 1 FROM service_authn.service_assertion_replay \
                     WHERE replay_key = $1 AND expires_at > now()",
                    &[&key],
                )
                .await
                .map_err(|error| ReplayStoreError(error.to_string()))?;
            if !seen.is_empty() {
                return Ok(ReplayClaim::AlreadyUsed);
            }
            let seconds = expires_at
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("an instant after the epoch")
                .as_secs_f64();
            self.client
                .execute(
                    "INSERT INTO service_authn.service_assertion_replay (replay_key, expires_at) \
                     VALUES ($1, to_timestamp($2::double precision)) \
                     ON CONFLICT (replay_key) DO UPDATE SET expires_at = excluded.expires_at",
                    &[&key, &seconds],
                )
                .await
                .map_err(|error| ReplayStoreError(error.to_string()))?;
            Ok(ReplayClaim::Accepted)
        })
    }
}

/// One assertion, and two verifiers that share only the database.
struct TwoReplicas {
    assertion: Rc<String>,
    first: Rc<ServiceAssertionVerifier>,
    second: Rc<ServiceAssertionVerifier>,
}

async fn two_replicas(
    url: &str,
    store: impl Fn(Client) -> Arc<dyn ReplayStore + Send + Sync>,
) -> TwoReplicas {
    let signing = ServiceSigningKey::generate();
    let public = signing.verifying_key_bytes();
    let kid = format!("{CALLER_KID}-{}", unique_suffix());
    let assertion = ServiceAssertionMinter::new(issuer(CALLER), kid.clone(), &signing)
        .expect("build the caller's minter")
        .mint(&issuer(CALLEE))
        .expect("mint one assertion");

    let mut verifiers = Vec::new();
    for _ in 0..2 {
        let client = connect(url).await;
        ensure_fixture(&client).await;
        let mut bundle = ServiceTrustBundle::new();
        bundle
            .trust(&issuer(CALLER), kid.clone(), public)
            .expect("trust the caller's key");
        verifiers.push(ServiceAssertionVerifier::new(bundle, store(client)));
    }
    let second = Rc::new(verifiers.pop().expect("two verifiers"));
    let first = Rc::new(verifiers.pop().expect("two verifiers"));
    TwoReplicas {
        assertion: Rc::new(assertion),
        first,
        second,
    }
}

/// Present the one assertion to both replicas at once, and count acceptances.
///
/// The two verifications run as separate tasks rather than as two branches of
/// one future, so each is driven independently by the runtime and neither can
/// be starved by the other's inline execution.
async fn race(replicas: &TwoReplicas) -> usize {
    let spawn_one = |verifier: &Rc<ServiceAssertionVerifier>| {
        let verifier = Rc::clone(verifier);
        let assertion = Rc::clone(&replicas.assertion);
        compio::runtime::spawn(async move {
            let observed = PeerCredentials::new(Some(assertion.as_str()), None, CALLEE);
            verify_identity(verifier.as_ref(), &observed).await
        })
    };
    let first = spawn_one(&replicas.first);
    let second = spawn_one(&replicas.second);
    let outcomes = [
        first.await.expect("first verification task"),
        second.await.expect("second verification task"),
    ];
    outcomes.iter().filter(|outcome| outcome.is_ok()).count()
}

#[compio::test]
async fn two_replicas_racing_one_assertion_admit_exactly_one() {
    let url = db_url();
    let replicas = two_replicas(&url, |client| {
        Arc::new(PostgresReplayStore::new(client)) as Arc<dyn ReplayStore + Send + Sync>
    })
    .await;

    assert_eq!(
        race(&replicas).await,
        1,
        "the shared put-if-absent must admit exactly one replica"
    );
}

#[compio::test]
async fn a_read_then_write_store_loses_the_same_race() {
    let url = db_url();
    // The negative control. Same two connections, same one assertion, the only
    // difference being that the claim is a SELECT followed by an INSERT. If
    // this admitted one, the harness would not be racing and the test above
    // would prove nothing.
    let replicas = two_replicas(&url, |client| {
        Arc::new(ReadThenWriteStore { client }) as Arc<dyn ReplayStore + Send + Sync>
    })
    .await;

    assert_eq!(
        race(&replicas).await,
        2,
        "a read-then-write claim must lose this race, or the harness is not racing"
    );
}

#[compio::test]
async fn a_live_claim_blocks_a_replay_and_an_expired_one_does_not() {
    let url = db_url();
    let client = connect(&url).await;
    ensure_fixture(&client).await;
    // A second connection: the store takes ownership of the first and exposes
    // no read of its own.
    let probe = connect(&url).await;
    let store = PostgresReplayStore::new(client);

    let live = format!("spiffe://zeroship.ai/svc/gateway|live-{}", unique_suffix());
    let future = SystemTime::now() + Duration::from_secs(120);
    assert_eq!(
        store.claim(&live, future).await.expect("claim"),
        ReplayClaim::Accepted
    );
    assert_eq!(
        store.claim(&live, future).await.expect("re-claim"),
        ReplayClaim::AlreadyUsed,
        "a live claim is single use"
    );

    // A row past its retention window is reclaimable in place, so a lagging
    // sweeper cannot lock a key out forever.
    let stale = format!("spiffe://zeroship.ai/svc/gateway|stale-{}", unique_suffix());
    let past = SystemTime::now() - Duration::from_secs(120);
    assert_eq!(
        store.claim(&stale, past).await.expect("claim"),
        ReplayClaim::Accepted
    );
    assert_eq!(
        store.claim(&stale, future).await.expect("re-claim"),
        ReplayClaim::Accepted,
        "an expired claim must not block the key forever"
    );

    // The sweep, judged on two rows this run owns rather than on the count it
    // returns.
    //
    // The count is not assertable here. `purge_expired` reports rows deleted
    // TABLE-WIDE, and every row this run still holds at this point is live -
    // MEASURED: a completed run leaves nine rows and all nine are inside their
    // retention window. So on a fresh database the count is 0, and on a shared
    // one it is composed entirely of OTHER runs' rows. The assertion that stood
    // here was `assert!(purged < u64::MAX)`, which no return value of `execute`
    // can fail: it read as a check on the sweep while being satisfied by a
    // sweep that deleted nothing.
    //
    // The pair below differs in one variable, the retention window, so it
    // discriminates in both directions: a purge that deletes nothing fails the
    // second, and a purge that dropped its `WHERE` and deleted everything fails
    // the first.
    let swept = format!("spiffe://zeroship.ai/svc/gateway|swept-{}", unique_suffix());
    assert_eq!(
        store
            .claim(&swept, SystemTime::now() - Duration::from_secs(1))
            .await
            .expect("claim"),
        ReplayClaim::Accepted,
        "the row the sweep is about to be judged on was written"
    );
    store.purge_expired().await.expect("sweep");
    assert!(
        row_exists(&probe, &live).await,
        "the sweep must not touch a claim still inside its retention window"
    );
    assert!(
        !row_exists(&probe, &swept).await,
        "the sweep must delete a claim whose retention window has closed"
    );
    // WHAT THIS DOES NOT CATCH: a run racing this one against the same database
    // sweeps `swept` too, so under concurrency the absence above can be that
    // run's work rather than this store's. That direction is a false GREEN and
    // never a false red, which is the right way round for a shared fixture -
    // the failure this file's advisory lock exists to prevent is a fixture bug
    // that reads as a flake.
}

#[compio::test]
async fn a_verified_assertion_cannot_be_replayed_at_another_replica() {
    let url = db_url();
    // Sequential, and across replicas: the second verifier has never seen this
    // assertion and has no process-local memory of it. Only the shared store
    // can refuse it.
    let replicas = two_replicas(&url, |client| {
        Arc::new(PostgresReplayStore::new(client)) as Arc<dyn ReplayStore + Send + Sync>
    })
    .await;

    let observed = PeerCredentials::new(Some(replicas.assertion.as_str()), None, CALLEE);
    assert!(verify_identity(replicas.first.as_ref(), &observed).await.is_ok());
    assert_eq!(
        verify_identity(replicas.second.as_ref(), &observed).await,
        Err(AuthError::CredentialRejected)
    );
}

#[compio::test]
async fn every_granted_role_can_run_the_stores_own_statements() {
    let url = db_url();
    // The rest of this file connects as the privileged test DSN, so it can
    // prove the SQL is correct and cannot prove a service is allowed to issue
    // it. Those are different questions and only the second one shipped wrong.
    for role in granted_roles() {
        let store = PostgresReplayStore::new(client_as_role(&url, &role).await);
        let key = format!("spiffe://zeroship.ai/svc/gateway|{role}-{}", unique_suffix());
        let future = SystemTime::now() + Duration::from_secs(120);

        assert_eq!(
            store
                .claim(&key, future)
                .await
                .unwrap_or_else(|error| panic!("{role} must be able to claim: {error}")),
            ReplayClaim::Accepted,
            "{role}: the first claim of a fresh key wins"
        );
        assert_eq!(
            store
                .claim(&key, future)
                .await
                .unwrap_or_else(|error| panic!("{role} must be able to re-claim: {error}")),
            ReplayClaim::AlreadyUsed,
            "{role}: a live claim is single use"
        );
        store
            .purge_expired()
            .await
            .unwrap_or_else(|error| panic!("{role} must be able to sweep: {error}"));
    }
}

#[compio::test]
async fn a_role_without_the_grant_fails_closed_rather_than_admitting_the_assertion() {
    let url = db_url();
    // The fail-closed arm was covered only in `zeroship-core`, against a store
    // that returns an error by construction. Mutating `service_replay.rs` to
    // map a driver error to Accepted left this file 4 of 4 green, because
    // nothing here ever made a statement fail. A role without the grant does,
    // and it is the same failure the missing SELECT would have produced in
    // production.
    let store = PostgresReplayStore::new(client_as_role(&url, UNGRANTED_ROLE).await);
    let key = format!("spiffe://zeroship.ai/svc/gateway|denied-{}", unique_suffix());
    let error = store
        .claim(&key, SystemTime::now() + Duration::from_secs(120))
        .await
        .expect_err("a role without the grant cannot claim");
    // `compio_postgres::Error` displays a server-side failure as the bare
    // string `db error`, so a store that forwarded `error.to_string()` would
    // log a total inbound-auth outage as three uninformative words. Both the
    // server's message and its SQLSTATE have to survive.
    let rendered = error.to_string();
    for expected in [
        "permission denied for table service_assertion_replay",
        "42501",
    ] {
        assert!(
            rendered.contains(expected),
            "the store must surface the driver's own {expected:?}: {rendered}"
        );
    }

    // And the verifier turns that into a refusal rather than a free pass.
    let signing = ServiceSigningKey::generate();
    let kid = format!("{CALLER_KID}-{}", unique_suffix());
    let assertion = ServiceAssertionMinter::new(issuer(CALLER), kid.clone(), &signing)
        .expect("build the caller's minter")
        .mint(&issuer(CALLEE))
        .expect("mint one assertion");
    let mut bundle = ServiceTrustBundle::new();
    bundle
        .trust(&issuer(CALLER), kid, signing.verifying_key_bytes())
        .expect("trust the caller's key");
    let verifier = ServiceAssertionVerifier::new(bundle, Arc::new(store));
    let observed = PeerCredentials::new(Some(assertion.as_str()), None, CALLEE);
    let outcome = verify_identity(&verifier, &observed).await;
    assert!(
        outcome.is_err(),
        "a store that could not answer has not said the assertion is fresh"
    );
    assert_eq!(
        outcome,
        Err(AuthError::StoreUnavailable),
        "and the outage is reported as one, not as a rejected credential"
    );
}
