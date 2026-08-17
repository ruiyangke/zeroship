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
//! Skips when `AUTH_DB_URL` is unset. Under `ZEROSHIP_REQUIRE_LIVE_BACKENDS=1`
//! a skip is a failure instead.

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
const FIXTURE_DDL: &str = "CREATE SCHEMA IF NOT EXISTS zeroship; \
     CREATE TABLE IF NOT EXISTS zeroship.service_assertion_replay ( \
         replay_key text PRIMARY KEY, \
         expires_at timestamptz NOT NULL \
     ); \
     CREATE INDEX IF NOT EXISTS service_assertion_replay_expiry_idx \
         ON zeroship.service_assertion_replay (expires_at)";

/// Serialises the fixture DDL across concurrent test threads and processes.
///
/// MEASURED: without it, the first run against a database that does not yet
/// have the table fails 3 of 4 tests, because cargo runs the four tests on four
/// threads and `CREATE TABLE IF NOT EXISTS` is not concurrency-safe - the
/// existence check and the create are not atomic, so the losers raise a
/// duplicate-key error on the catalogue. The second run passed 4 of 4 and every
/// run after it did too, which is exactly the shape that gets a fixture bug
/// mistaken for a flake and then ignored: it only ever fails on a fresh
/// database.
const FIXTURE_LOCK: i64 = 7_523_000_001;

fn db_url() -> Option<String> {
    zeroship_core::test_env!("AUTH_DB_URL")
}

/// Establish the replay table exactly once, whoever gets there first.
async fn ensure_fixture(client: &Client) {
    client
        .execute("SELECT pg_advisory_lock($1)", &[&FIXTURE_LOCK])
        .await
        .expect("take the fixture lock");
    let established = client.batch_execute(FIXTURE_DDL).await;
    client
        .execute("SELECT pg_advisory_unlock($1)", &[&FIXTURE_LOCK])
        .await
        .expect("release the fixture lock");
    established.expect("establish the replay table fixture");
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
                    "SELECT 1 FROM zeroship.service_assertion_replay \
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
                    "INSERT INTO zeroship.service_assertion_replay (replay_key, expires_at) \
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
    store: impl Fn(Client) -> Arc<dyn ReplayStore>,
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
    let Some(url) = db_url() else {
        zeroship_test_support::skip("AUTH_DB_URL unset (Postgres jti store)");
        return;
    };
    let replicas = two_replicas(&url, |client| {
        Arc::new(PostgresReplayStore::new(client)) as Arc<dyn ReplayStore>
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
    let Some(url) = db_url() else {
        zeroship_test_support::skip("AUTH_DB_URL unset (Postgres jti store)");
        return;
    };
    // The negative control. Same two connections, same one assertion, the only
    // difference being that the claim is a SELECT followed by an INSERT. If
    // this admitted one, the harness would not be racing and the test above
    // would prove nothing.
    let replicas = two_replicas(&url, |client| {
        Arc::new(ReadThenWriteStore { client }) as Arc<dyn ReplayStore>
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
    let Some(url) = db_url() else {
        zeroship_test_support::skip("AUTH_DB_URL unset (Postgres jti store)");
        return;
    };
    let client = connect(&url).await;
    ensure_fixture(&client).await;
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

    let purged = store.purge_expired().await.expect("sweep");
    assert!(purged < u64::MAX, "the sweep runs and reports a count: {purged}");
}

#[compio::test]
async fn a_verified_assertion_cannot_be_replayed_at_another_replica() {
    let Some(url) = db_url() else {
        zeroship_test_support::skip("AUTH_DB_URL unset (Postgres jti store)");
        return;
    };
    // Sequential, and across replicas: the second verifier has never seen this
    // assertion and has no process-local memory of it. Only the shared store
    // can refuse it.
    let replicas = two_replicas(&url, |client| {
        Arc::new(PostgresReplayStore::new(client)) as Arc<dyn ReplayStore>
    })
    .await;

    let observed = PeerCredentials::new(Some(replicas.assertion.as_str()), None, CALLEE);
    assert!(verify_identity(replicas.first.as_ref(), &observed).await.is_ok());
    assert_eq!(
        verify_identity(replicas.second.as_ref(), &observed).await,
        Err(AuthError::CredentialRejected)
    );
}
