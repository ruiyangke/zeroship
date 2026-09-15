//! Control's startup import of the operator's worker enroller file.
//!
//! What `worker_enrolment::import_enrollers` may do is only ever ADD: insert an
//! enroller Control has not recorded, leave a recorded one exactly as it is -
//! a REVOKED one included - and refuse, writing nothing, a file that disagrees
//! with what is recorded. Every arm here runs the import as the production
//! `zeroship_control` login rather than the fixture's superuser, so the grants
//! the import depends on are part of what is measured.
//!
//! ISOLATION: `live_db.rs` shares one database across every module, so each
//! arm mints its own enroller ids and keys and removes only the rows it
//! created.

use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};

use zeroship_control::worker_enrolment::{import_enrollers, EnrollerImportReport};
use zeroship_control::Registry;
use zeroship_core::typed_id::new_worker_enroller_id;

use crate::common;

/// The deployment's single execution zone, seeded by
/// `db/migrations-ts/20260914000450_execution_zones_default_zone.ts`.
const DEFAULT_ZONE_ID: &str = "ezn_default000000000000000000";
const DEFAULT_ZONE: &str = "default";

/// The fixture database, reached as the production control-plane login.
fn control_login_url() -> String {
    let mut url = url::Url::parse(&common::require_control_db()).expect("fixture URL parses");
    url.set_username("zeroship_control").expect("username");
    url.set_password(Some("zeroship_control")).expect("password");
    url.into()
}

/// The fixture database as its superuser, for the operator's side of each arm
/// (revocation, row inspection and cleanup).
async fn admin() -> compio_postgres::Client {
    let (client, connection) =
        compio_postgres::connect(&common::require_control_db(), compio_postgres::NoTls)
            .await
            .expect("admin connect");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

/// A fresh, usable Ed25519 public key, base64url without padding.
fn fresh_public_key() -> String {
    use rand::RngCore as _;
    let mut seed = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
    URL_SAFE_NO_PAD.encode(signing.verifying_key().to_bytes())
}

fn entry(id: &str, zone: &str, public_key: &str) -> Value {
    json!({"id": id, "zone": zone, "public_key": public_key})
}

struct ImportFile {
    dir: tempfile::TempDir,
}

impl ImportFile {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("import file directory"),
        }
    }

    fn write(&self, document: &Value) -> PathBuf {
        self.write_raw(&serde_json::to_vec(document).expect("document serializes"))
    }

    fn write_raw(&self, bytes: &[u8]) -> PathBuf {
        let path = self.dir.path().join("worker-enrollers.json");
        std::fs::write(&path, bytes).expect("write import file");
        path
    }
}

async fn import(path: &Path) -> Result<Option<EnrollerImportReport>, String> {
    let registry = Registry::new(&control_login_url())
        .await
        .expect("registry as zeroship_control");
    import_enrollers(&registry, path).await
}

/// `(public_key, execution_zone_id, status)` of one recorded enroller.
async fn recorded(pg: &compio_postgres::Client, id: &str) -> Option<(Vec<u8>, String, String)> {
    pg.query_opt(
        "SELECT public_key, execution_zone_id, status FROM zeroship.worker_enrollers WHERE id = $1",
        &[&id],
    )
    .await
    .expect("read enroller")
    .map(|row| (row.get(0), row.get(1), row.get(2)))
}

async fn forget(pg: &compio_postgres::Client, ids: &[&str]) {
    for id in ids {
        pg.execute("DELETE FROM zeroship.worker_enrollers WHERE id = $1", &[id])
            .await
            .expect("remove probe enroller");
    }
}

#[compio::test]
async fn an_unset_file_imports_nothing() {
    assert_eq!(import(Path::new("")).await, Ok(None));
}

/// The inserting arm and its idempotent re-run: an unknown enroller is inserted
/// active in the zone its NAME resolves to, and a second import of the same
/// file changes nothing - which is what every Control restart does.
#[compio::test]
async fn unknown_enrollers_are_inserted_active_and_a_rerun_changes_nothing() {
    let pg = admin().await;
    let file = ImportFile::new();
    let (first, second) = (new_worker_enroller_id(), new_worker_enroller_id());
    let (first_key, second_key) = (fresh_public_key(), fresh_public_key());
    let path = file.write(&json!({"enrollers": [
        entry(&first, DEFAULT_ZONE, &first_key),
        entry(&second, DEFAULT_ZONE, &second_key),
    ]}));

    let inserted = import(&path).await;
    let rerun = import(&path).await;
    let first_row = recorded(&pg, &first).await;
    let second_row = recorded(&pg, &second).await;
    forget(&pg, &[&first, &second]).await;

    assert_eq!(
        inserted,
        Ok(Some(EnrollerImportReport {
            inserted: 2,
            unchanged: 0,
            revoked: 0,
        }))
    );
    assert_eq!(
        rerun,
        Ok(Some(EnrollerImportReport {
            inserted: 0,
            unchanged: 2,
            revoked: 0,
        })),
        "a restart re-reading the same file must be a no-op"
    );
    let decoded = |key: &str| URL_SAFE_NO_PAD.decode(key).expect("key decodes");
    assert_eq!(
        first_row,
        Some((
            decoded(&first_key),
            DEFAULT_ZONE_ID.to_owned(),
            "active".to_owned()
        ))
    );
    assert_eq!(
        second_row,
        Some((
            decoded(&second_key),
            DEFAULT_ZONE_ID.to_owned(),
            "active".to_owned()
        ))
    );
}

/// THE RULE THAT MAKES REVOCATION SURVIVE A FILE NOBODY EDITED. The operator
/// revokes an enroller in the database and the next Control boot re-reads a
/// file that still names it: it stays revoked.
///
/// The paired control is the sibling entry in the SAME file and the SAME
/// import, which stays active - so "stays revoked" is about the one row the
/// operator revoked, not an import that never writes anything.
#[compio::test]
async fn a_revoked_enroller_left_in_the_file_stays_revoked() {
    let pg = admin().await;
    let file = ImportFile::new();
    let (revoked, sibling) = (new_worker_enroller_id(), new_worker_enroller_id());
    let path = file.write(&json!({"enrollers": [
        entry(&revoked, DEFAULT_ZONE, &fresh_public_key()),
        entry(&sibling, DEFAULT_ZONE, &fresh_public_key()),
    ]}));
    import(&path).await.expect("first import");
    pg.execute("SELECT zeroship.revoke_worker_enroller($1)", &[&revoked])
        .await
        .expect("the operator revokes one unit");

    let after_restart = import(&path).await;
    let revoked_status = recorded(&pg, &revoked).await.map(|row| row.2);
    let sibling_status = recorded(&pg, &sibling).await.map(|row| row.2);
    forget(&pg, &[&revoked, &sibling]).await;

    assert_eq!(
        after_restart,
        Ok(Some(EnrollerImportReport {
            inserted: 0,
            unchanged: 1,
            revoked: 1,
        }))
    );
    assert_eq!(
        revoked_status.as_deref(),
        Some("revoked"),
        "an import must never write `active` over a revocation"
    );
    assert_eq!(sibling_status.as_deref(), Some("active"));
}

/// A file that disagrees with a recorded enroller refuses the boot, and
/// NOTHING in it is written - not even the entries that were admissible.
///
/// Four disagreements, each against the same recorded enroller: its id under
/// another key, its key under another id, its id in another declared zone, and
/// its id in a zone the deployment does not declare. Every refusal carries an
/// admissible new entry beside it, and that entry's absence afterwards is what
/// shows the import wrote nothing.
#[compio::test]
async fn a_file_conflicting_with_a_recorded_enroller_writes_nothing() {
    let pg = admin().await;
    let file = ImportFile::new();
    let recorded_id = new_worker_enroller_id();
    let recorded_key = fresh_public_key();
    import(&file.write(&json!({"enrollers": [entry(&recorded_id, DEFAULT_ZONE, &recorded_key)]})))
        .await
        .expect("record the enroller the conflicts are measured against");

    // A second declared zone, so a recorded enroller can be claimed for the
    // wrong one. Zones are declared by migrations, never by Control, so the
    // operator's side of the fixture declares this one.
    let other_zone = format!("probe-zone-{}", uuid::Uuid::new_v4().simple());
    let other_zone_id = zeroship_core::typed_id::generate("ezn");
    pg.execute(
        "INSERT INTO zeroship.execution_zones (id, name, status) VALUES ($1, $2, 'active')",
        &[&other_zone_id, &other_zone],
    )
    .await
    .expect("declare a second zone");

    let bystander = new_worker_enroller_id();
    let impostor = new_worker_enroller_id();
    let cases = [
        (
            "the recorded id under another key",
            json!({"enrollers": [
                entry(&bystander, DEFAULT_ZONE, &fresh_public_key()),
                entry(&recorded_id, DEFAULT_ZONE, &fresh_public_key()),
            ]}),
            "different public key",
        ),
        (
            "the recorded key under another id",
            json!({"enrollers": [
                entry(&bystander, DEFAULT_ZONE, &fresh_public_key()),
                entry(&impostor, DEFAULT_ZONE, &recorded_key),
            ]}),
            "already recorded for enroller",
        ),
        (
            "the recorded id in another declared zone",
            json!({"enrollers": [
                entry(&bystander, DEFAULT_ZONE, &fresh_public_key()),
                entry(&recorded_id, &other_zone, &recorded_key),
            ]}),
            "already recorded in execution zone",
        ),
        (
            "a zone the deployment does not declare",
            json!({"enrollers": [
                entry(&bystander, DEFAULT_ZONE, &fresh_public_key()),
                entry(&recorded_id, "elsewhere", &recorded_key),
            ]}),
            "does not declare",
        ),
    ];
    let mut outcomes = Vec::new();
    for (label, document, _) in &cases {
        outcomes.push((*label, import(&file.write(document)).await));
    }
    let bystander_row = recorded(&pg, &bystander).await;
    let impostor_row = recorded(&pg, &impostor).await;
    let recorded_row = recorded(&pg, &recorded_id).await;
    forget(&pg, &[&recorded_id, &bystander, &impostor]).await;
    pg.execute(
        "DELETE FROM zeroship.execution_zones WHERE id = $1",
        &[&other_zone_id],
    )
    .await
    .expect("remove the probe zone");

    for ((label, outcome), (_, _, reason)) in outcomes.into_iter().zip(&cases) {
        let message = outcome.expect_err(label);
        assert!(message.contains(reason), "{label}: {message}");
    }
    assert_eq!(
        bystander_row, None,
        "an admissible entry in a refused file must not be written"
    );
    assert_eq!(impostor_row, None);
    assert_eq!(
        recorded_row,
        Some((
            URL_SAFE_NO_PAD.decode(&recorded_key).expect("key decodes"),
            DEFAULT_ZONE_ID.to_owned(),
            "active".to_owned()
        )),
        "a refused file must leave the recorded enroller exactly as it was"
    );
}

/// A malformed file is refused before any database work, whatever is wrong
/// with it. The control is the well-formed file at the end, which imports:
/// without it every refusal above could be a function that refuses everything.
///
/// "Writes nothing" is measured on the ids the refused files name, never on a
/// table-wide count: sibling modules enrol concurrently in the same database.
#[compio::test]
async fn a_malformed_file_is_refused_and_writes_nothing() {
    let pg = admin().await;
    let file = ImportFile::new();
    let id = new_worker_enroller_id();
    let key = fresh_public_key();
    let other = new_worker_enroller_id();
    let small_order = URL_SAFE_NO_PAD.encode([0_u8; 32]);

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("not JSON", b"not json".to_vec()),
        ("no enrollers", br#"{"enrollers": []}"#.to_vec()),
        (
            "an unknown member",
            serde_json::to_vec(&json!({"enrollers": [
                {"id": id, "zone": DEFAULT_ZONE, "public_key": key, "status": "active"}
            ]}))
            .unwrap(),
        ),
        (
            "an id that is not a wen_ typed id",
            serde_json::to_vec(&json!({"enrollers": [
                entry("wkr_0000000000000000000000001", DEFAULT_ZONE, &key)
            ]}))
            .unwrap(),
        ),
        (
            "a key that is not 32 bytes",
            serde_json::to_vec(&json!({"enrollers": [
                entry(&id, DEFAULT_ZONE, &URL_SAFE_NO_PAD.encode([7_u8; 31]))
            ]}))
            .unwrap(),
        ),
        (
            "a small-order key",
            serde_json::to_vec(&json!({"enrollers": [entry(&id, DEFAULT_ZONE, &small_order)]}))
                .unwrap(),
        ),
        (
            "an empty zone",
            serde_json::to_vec(&json!({"enrollers": [entry(&id, "", &key)]})).unwrap(),
        ),
        (
            "one id twice",
            serde_json::to_vec(&json!({"enrollers": [
                entry(&id, DEFAULT_ZONE, &key),
                entry(&id, DEFAULT_ZONE, &fresh_public_key()),
            ]}))
            .unwrap(),
        ),
        (
            "one key twice",
            serde_json::to_vec(&json!({"enrollers": [
                entry(&id, DEFAULT_ZONE, &key),
                entry(&other, DEFAULT_ZONE, &key),
            ]}))
            .unwrap(),
        ),
    ];
    let mut refused = Vec::new();
    for (label, bytes) in &cases {
        refused.push((*label, import(&file.write_raw(bytes)).await));
    }
    let written_by_refusals = [recorded(&pg, &id).await, recorded(&pg, &other).await];
    let control = import(&file.write(&json!({"enrollers": [entry(&id, DEFAULT_ZONE, &key)]}))).await;
    forget(&pg, &[&id]).await;

    for (label, outcome) in refused {
        assert!(outcome.is_err(), "{label} must be refused, got {outcome:?}");
    }
    assert_eq!(written_by_refusals, [None, None], "a refused file writes no row");
    assert_eq!(
        control,
        Ok(Some(EnrollerImportReport {
            inserted: 1,
            unchanged: 0,
            revoked: 0,
        }))
    );
}

/// Two Control replicas booting at once with the same file converge: one
/// inserts, the other finds the winner's rows and judges them unchanged. The
/// second import's insert waits on the first's uncommitted row rather than
/// failing, which is why the outcome is two successes and not a refused boot.
#[compio::test]
async fn two_replicas_importing_the_same_file_at_once_converge() {
    let pg = admin().await;
    let file = ImportFile::new();
    let ids = [new_worker_enroller_id(), new_worker_enroller_id()];
    let path = file.write(&json!({"enrollers": [
        entry(&ids[0], DEFAULT_ZONE, &fresh_public_key()),
        entry(&ids[1], DEFAULT_ZONE, &fresh_public_key()),
    ]}));

    let (left, right) = futures::join!(import(&path), import(&path));
    let statuses = [
        recorded(&pg, &ids[0]).await.map(|row| row.2),
        recorded(&pg, &ids[1]).await.map(|row| row.2),
    ];
    forget(&pg, &[&ids[0], &ids[1]]).await;

    let left = left.expect("one replica imports").expect("configured");
    let right = right.expect("the other replica imports").expect("configured");
    assert_eq!(left.inserted + right.inserted, 2, "{left:?} {right:?}");
    assert_eq!(left.unchanged + right.unchanged, 2, "{left:?} {right:?}");
    assert_eq!(statuses, [Some("active".to_owned()), Some("active".to_owned())]);
}
