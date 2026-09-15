//! Control's startup import of the operator's trusted-signer file.
//!
//! What `worker_join::import_join_signers` may do is only ever ADD: insert a
//! signer Control has not recorded, leave a recorded one exactly as it is - a
//! REVOKED one included - and refuse, writing nothing, a file that disagrees
//! with what is recorded. Every arm here runs the import as the production
//! `zeroship_control` login rather than the fixture's superuser, so the grants
//! the import depends on are part of what is measured.
//!
//! ISOLATION: `live_db.rs` shares one database across every module, so each
//! arm mints its own signer ids and keys and removes only the rows it created.

use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};

use zeroship_control::worker_join::{import_join_signers, JoinSignerImportReport};
use zeroship_control::Registry;
use zeroship_core::typed_id::new_join_signer_id;

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

fn entry(id: &str, zones: &[&str], public_key: &str) -> Value {
    json!({"id": id, "zones": zones, "public_key": public_key})
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
        let path = self.dir.path().join("join-signers.json");
        std::fs::write(&path, bytes).expect("write import file");
        path
    }
}

async fn import(path: &Path) -> Result<Option<JoinSignerImportReport>, String> {
    let registry = Registry::new(&control_login_url())
        .await
        .expect("registry as zeroship_control");
    import_join_signers(&registry, path).await
}

/// `(public_key, permitted zone ids, status)` of one recorded signer.
///
/// The zones come back with the row because they are HALF the recorded fact: a
/// signer's zones are its authority, so an arm that checked only the key would
/// not notice an import that recorded a signer for nothing at all.
async fn recorded(pg: &compio_postgres::Client, id: &str) -> Option<(Vec<u8>, Vec<String>, String)> {
    let row = pg
        .query_opt(
            "SELECT public_key, status FROM zeroship.worker_join_signers WHERE id = $1",
            &[&id],
        )
        .await
        .expect("read signer")?;
    let zones = pg
        .query(
            "SELECT execution_zone_id FROM zeroship.worker_join_signer_zones \
              WHERE signer_id = $1 ORDER BY execution_zone_id",
            &[&id],
        )
        .await
        .expect("read permitted zones")
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect();
    Some((row.get(0), zones, row.get(1)))
}

async fn forget(pg: &compio_postgres::Client, ids: &[&str]) {
    for id in ids {
        pg.execute(
            "DELETE FROM zeroship.worker_join_signer_zones WHERE signer_id = $1",
            &[id],
        )
        .await
        .expect("remove probe zone grants");
        pg.execute(
            "DELETE FROM zeroship.worker_join_signers WHERE id = $1",
            &[id],
        )
        .await
        .expect("remove probe signer");
    }
}

#[compio::test]
async fn an_unset_file_imports_nothing() {
    assert_eq!(import(Path::new("")).await, Ok(None));
}

/// The inserting arm and its idempotent re-run: an unknown signer is inserted
/// active for the zones its NAMES resolve to, and a second import of the same
/// file changes nothing - which is what every Control restart does.
#[compio::test]
async fn unknown_signers_are_inserted_active_and_a_rerun_changes_nothing() {
    let pg = admin().await;
    let file = ImportFile::new();
    let (first, second) = (new_join_signer_id(), new_join_signer_id());
    let (first_key, second_key) = (fresh_public_key(), fresh_public_key());
    let path = file.write(&json!({"signers": [
        entry(&first, &[DEFAULT_ZONE], &first_key),
        entry(&second, &[DEFAULT_ZONE], &second_key),
    ]}));

    let inserted = import(&path).await;
    let rerun = import(&path).await;
    let first_row = recorded(&pg, &first).await;
    let second_row = recorded(&pg, &second).await;
    forget(&pg, &[&first, &second]).await;

    assert_eq!(
        inserted,
        Ok(Some(JoinSignerImportReport {
            inserted: 2,
            unchanged: 0,
            revoked: 0,
        }))
    );
    assert_eq!(
        rerun,
        Ok(Some(JoinSignerImportReport {
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
            vec![DEFAULT_ZONE_ID.to_owned()],
            "active".to_owned()
        ))
    );
    assert_eq!(
        second_row,
        Some((
            decoded(&second_key),
            vec![DEFAULT_ZONE_ID.to_owned()],
            "active".to_owned()
        ))
    );
}

/// THE RULE THAT MAKES REVOCATION SURVIVE A FILE NOBODY EDITED. The operator
/// rotates a signer in the database and the next Control boot re-reads a file
/// that still names it: it stays revoked.
///
/// The paired control is the sibling entry in the SAME file and the SAME
/// import, which stays active - so "stays revoked" is about the one row the
/// operator rotated, not an import that never writes anything.
#[compio::test]
async fn a_revoked_signer_left_in_the_file_stays_revoked() {
    let pg = admin().await;
    let file = ImportFile::new();
    let (revoked, sibling) = (new_join_signer_id(), new_join_signer_id());
    let path = file.write(&json!({"signers": [
        entry(&revoked, &[DEFAULT_ZONE], &fresh_public_key()),
        entry(&sibling, &[DEFAULT_ZONE], &fresh_public_key()),
    ]}));
    import(&path).await.expect("first import");
    pg.execute(
        "SELECT zeroship.rotate_worker_join_signer($1)",
        &[&revoked],
    )
    .await
    .expect("the operator rotates one signer");

    let after_restart = import(&path).await;
    let revoked_status = recorded(&pg, &revoked).await.map(|row| row.2);
    let sibling_status = recorded(&pg, &sibling).await.map(|row| row.2);
    forget(&pg, &[&revoked, &sibling]).await;

    assert_eq!(
        after_restart,
        Ok(Some(JoinSignerImportReport {
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

/// A file that disagrees with a recorded signer refuses the boot, and NOTHING
/// in it is written - not even the entries that were admissible.
///
/// Four disagreements, each against the same recorded signer: its id under
/// another key, its key under another id, its id with a WIDER set of declared
/// zones, and its id naming a zone the deployment does not declare. Every
/// refusal carries an admissible new entry beside it, and that entry's absence
/// afterwards is what shows the import wrote nothing.
#[compio::test]
async fn a_file_conflicting_with_a_recorded_signer_writes_nothing() {
    let pg = admin().await;
    let file = ImportFile::new();
    let recorded_id = new_join_signer_id();
    let recorded_key = fresh_public_key();
    import(&file.write(&json!({"signers": [
        entry(&recorded_id, &[DEFAULT_ZONE], &recorded_key)
    ]})))
    .await
    .expect("record the signer the conflicts are measured against");

    // A second declared zone, so a recorded signer can be claimed for a wider
    // set. Zones are declared by migrations, never by Control, so the
    // operator's side of the fixture declares this one.
    let other_zone = format!("probe-zone-{}", uuid::Uuid::new_v4().simple());
    let other_zone_id = zeroship_core::typed_id::generate("ezn");
    pg.execute(
        "INSERT INTO zeroship.execution_zones (id, name, status) VALUES ($1, $2, 'active')",
        &[&other_zone_id, &other_zone],
    )
    .await
    .expect("declare a second zone");

    let bystander = new_join_signer_id();
    let impostor = new_join_signer_id();
    let cases = [
        (
            "the recorded id under another key",
            json!({"signers": [
                entry(&bystander, &[DEFAULT_ZONE], &fresh_public_key()),
                entry(&recorded_id, &[DEFAULT_ZONE], &fresh_public_key()),
            ]}),
            "different public key",
        ),
        (
            "the recorded key under another id",
            json!({"signers": [
                entry(&bystander, &[DEFAULT_ZONE], &fresh_public_key()),
                entry(&impostor, &[DEFAULT_ZONE], &recorded_key),
            ]}),
            "already recorded for signer",
        ),
        (
            "the recorded id with a wider zone set",
            json!({"signers": [
                entry(&bystander, &[DEFAULT_ZONE], &fresh_public_key()),
                entry(&recorded_id, &[DEFAULT_ZONE, &other_zone], &recorded_key),
            ]}),
            "zones are its authority",
        ),
        (
            "a zone the deployment does not declare",
            json!({"signers": [
                entry(&bystander, &[DEFAULT_ZONE], &fresh_public_key()),
                entry(&recorded_id, &["elsewhere"], &recorded_key),
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
            vec![DEFAULT_ZONE_ID.to_owned()],
            "active".to_owned()
        )),
        "a refused file must leave the recorded signer exactly as it was"
    );
}

/// A malformed file is refused before any database work, whatever is wrong
/// with it. The control is the well-formed file at the end, which imports:
/// without it every refusal above could be a function that refuses everything.
///
/// "Writes nothing" is measured on the ids the refused files name, never on a
/// table-wide count: sibling modules join concurrently in the same database.
#[compio::test]
async fn a_malformed_file_is_refused_and_writes_nothing() {
    let pg = admin().await;
    let file = ImportFile::new();
    let id = new_join_signer_id();
    let key = fresh_public_key();
    let other = new_join_signer_id();
    let small_order = URL_SAFE_NO_PAD.encode([0_u8; 32]);

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("not JSON", b"not json".to_vec()),
        ("no signers", br#"{"signers": []}"#.to_vec()),
        (
            "an unknown member",
            serde_json::to_vec(&json!({"signers": [
                {"id": id, "zones": [DEFAULT_ZONE], "public_key": key, "status": "active"}
            ]}))
            .unwrap(),
        ),
        (
            "an id that is not a wjs_ typed id",
            serde_json::to_vec(&json!({"signers": [
                entry("wkr_0000000000000000000000001", &[DEFAULT_ZONE], &key)
            ]}))
            .unwrap(),
        ),
        (
            "a key that is not 32 bytes",
            serde_json::to_vec(&json!({"signers": [
                entry(&id, &[DEFAULT_ZONE], &URL_SAFE_NO_PAD.encode([7_u8; 31]))
            ]}))
            .unwrap(),
        ),
        (
            "a small-order key",
            serde_json::to_vec(&json!({"signers": [entry(&id, &[DEFAULT_ZONE], &small_order)]}))
                .unwrap(),
        ),
        (
            "no zones at all",
            serde_json::to_vec(&json!({"signers": [entry(&id, &[], &key)]})).unwrap(),
        ),
        (
            "an empty zone name",
            serde_json::to_vec(&json!({"signers": [entry(&id, &[""], &key)]})).unwrap(),
        ),
        (
            "one id twice",
            serde_json::to_vec(&json!({"signers": [
                entry(&id, &[DEFAULT_ZONE], &key),
                entry(&id, &[DEFAULT_ZONE], &fresh_public_key()),
            ]}))
            .unwrap(),
        ),
        (
            "one key twice",
            serde_json::to_vec(&json!({"signers": [
                entry(&id, &[DEFAULT_ZONE], &key),
                entry(&other, &[DEFAULT_ZONE], &key),
            ]}))
            .unwrap(),
        ),
    ];
    let mut refused = Vec::new();
    for (label, bytes) in &cases {
        refused.push((*label, import(&file.write_raw(bytes)).await));
    }
    let written_by_refusals = [recorded(&pg, &id).await, recorded(&pg, &other).await];
    let control =
        import(&file.write(&json!({"signers": [entry(&id, &[DEFAULT_ZONE], &key)]}))).await;
    forget(&pg, &[&id]).await;

    for (label, outcome) in refused {
        assert!(outcome.is_err(), "{label} must be refused, got {outcome:?}");
    }
    assert_eq!(written_by_refusals, [None, None], "a refused file writes no row");
    assert_eq!(
        control,
        Ok(Some(JoinSignerImportReport {
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
    let ids = [new_join_signer_id(), new_join_signer_id()];
    let path = file.write(&json!({"signers": [
        entry(&ids[0], &[DEFAULT_ZONE], &fresh_public_key()),
        entry(&ids[1], &[DEFAULT_ZONE], &fresh_public_key()),
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
