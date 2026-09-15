use super::*;
use futures::channel::oneshot;
use std::{path::PathBuf, sync::Mutex};
use zeroship_bundle::{BlobError, PutOutcome};
use zeroship_control::{
    cron::deploy_retention::{Collector, DeployRetentionConfig},
    publication::{Acceptance, CatalogError, VerifiedDeployment},
};
use zeroship_workflow_manager::deployments::{self, DeploymentHolds};

struct Versions {
    app: AppId,
    old: String,
    old_hash: String,
    old_manifest: String,
    current: String,
    current_hash: String,
}

async fn seed_versions(fixture: &Fixture, label: &str) -> Versions {
    let (app, old, old_hash) = fixture.deployment(label).await;
    let old_manifest: String = fixture
        .platform
        .admin
        .query_one(
            "SELECT manifest_json FROM zeroship.app_deploys WHERE id=$1",
            &[&old],
        )
        .await
        .unwrap()
        .get(0);
    fixture
        .state
        .blob_store
        .put_manifest(&app, &old_hash, old_manifest.as_bytes())
        .await
        .unwrap();
    let current = typed_id::generate("dep");
    let mut manifest = zeroship_bundle::Manifest {
        asset_version: 1,
        ..Default::default()
    };
    let current_hash =
        zeroship_bundle::deployment_manifest_hash(&serde_json::to_vec(&manifest).unwrap()).unwrap();
    manifest.deploy_hash = Some(current_hash.clone());
    let encoded = serde_json::to_string(&manifest).unwrap();
    fixture
        .state
        .blob_store
        .put_manifest(&app, &current_hash, encoded.as_bytes())
        .await
        .unwrap();
    assert_eq!(fixture.platform.admin.execute(
        "UPDATE zeroship.app_deploys SET activated_at=now()-interval '1 day', created_at=now()-interval '1 day' WHERE id=$1",
        &[&old],
    ).await.unwrap(), 1);
    assert_eq!(fixture.platform.admin.execute(
        "INSERT INTO zeroship.app_deploys(id,app_id,deploy_hash,manifest_json,activated_at) VALUES($1,$2,$3,$4,now()-interval '1 hour')",
        &[&current, &app.as_str(), &current_hash, &encoded],
    ).await.unwrap(), 1);
    assert_eq!(
        fixture
            .platform
            .admin
            .execute(
                "UPDATE zeroship.apps SET deploy_hash=$2,manifest_json=$3 WHERE id=$1",
                &[&app.as_str(), &current_hash, &encoded],
            )
            .await
            .unwrap(),
        1
    );
    Versions {
        app,
        old,
        old_hash,
        old_manifest,
        current,
        current_hash,
    }
}

/// Redeploy the old version through the catalog command, as a seeded actor.
async fn redeploy_old(fixture: &Fixture, versions: &Versions) -> Result<Acceptance, CatalogError> {
    let actor = fixture.actor().await;
    let deployment =
        VerifiedDeployment::verify(versions.old_manifest.clone(), versions.old_hash.clone())
            .expect("the seeded manifest verifies");
    fixture
        .state
        .registry
        .deploy(super::deployment_commands::command(
            &versions.app,
            &actor,
            deployment,
        ))
        .await
}

async fn state(fixture: &Fixture, id: &str) -> String {
    fixture
        .platform
        .admin
        .query_one(
            "SELECT retention_state FROM zeroship.app_deploys WHERE id=$1",
            &[&id],
        )
        .await
        .unwrap()
        .get(0)
}

async fn assert_current(fixture: &Fixture, app: &AppId, hash: &str) {
    let value: String = fixture
        .platform
        .admin
        .query_one(
            "SELECT deploy_hash FROM zeroship.apps WHERE id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(value, hash);
}

async fn collector(fixture: &Fixture, blobs: Arc<dyn BlobStore>, batch: i64) -> Collector {
    Collector::connect(
        &fixture.control_url,
        blobs,
        DeployRetentionConfig {
            batch_size: batch,
            ..DeployRetentionConfig::default()
        },
    )
    .await
    .unwrap()
}

#[ntex::test]
async fn collection_obeys_independent_holds_without_creator_schema_access() {
    let fixture = Fixture::new().await;
    let versions = seed_versions(&fixture, "collector-private").await;
    let foreign = seed_versions(&fixture, "collector-foreign").await;
    let journal_schema = zeroship_core::app_derivation::schema_name(&versions.app);
    fixture
        .platform
        .admin
        .batch_execute(&format!(
            "CREATE SCHEMA \"{journal_schema}\"; \
         CREATE TABLE \"{journal_schema}\".__zeroship_workflow_runs(id text PRIMARY KEY); \
         REVOKE ALL ON SCHEMA \"{journal_schema}\" FROM PUBLIC, zeroship_control; \
         REVOKE ALL ON zeroship.workflow_runs FROM zeroship_control;"
        ))
        .await
        .unwrap();
    let readable: bool = fixture
        .platform
        .admin
        .query_one(
            "SELECT has_schema_privilege('zeroship_control',$1,'USAGE')",
            &[&journal_schema],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!readable);
    assert_eq!(
        fixture
            .platform
            .admin
            .execute(
                "UPDATE zeroship.apps SET archived_at=now() WHERE id=$1",
                &[&versions.app.as_str()],
            )
            .await
            .unwrap(),
        1
    );

    let ledger = DeploymentHolds::new(database(&fixture.control_url).await).unwrap();
    let queue = HoldScope::for_queue(versions.app.clone());
    let journal = HoldScope::for_app(versions.app.clone());
    ledger
        .acquire(&queue, &versions.old, generation(1))
        .await
        .unwrap();
    ledger
        .acquire(&journal, &versions.old, generation(1))
        .await
        .unwrap();
    assert!(matches!(
        ledger
            .acquire(
                &HoldScope::for_queue(foreign.app.clone()),
                &versions.old,
                generation(1)
            )
            .await,
        Err(deployments::Error::PermissionDenied)
    ));
    let mut scan = collector(&fixture, fixture.state.blob_store.clone(), 128).await;
    let first = scan.tick().await.unwrap();
    assert_eq!(first.failed, 0);
    assert_eq!(first.finished, 1);
    assert_eq!(state(&fixture, &foreign.old).await, "deleted");
    assert_eq!(state(&fixture, &versions.old).await, "available");
    ledger
        .release(&queue, &versions.old, generation(1))
        .await
        .unwrap();
    let held = scan.tick().await.unwrap();
    assert_eq!(held.finished, 0);
    fixture
        .state
        .blob_store
        .get_manifest(&versions.app, &versions.old_hash)
        .await
        .unwrap();
    ledger
        .release(&journal, &versions.old, generation(1))
        .await
        .unwrap();
    let released = scan.tick().await.unwrap();
    assert_eq!(released.finished, 1);
    assert_eq!(state(&fixture, &versions.old).await, "deleted");
    assert!(matches!(
        fixture
            .state
            .blob_store
            .get_manifest(&versions.app, &versions.old_hash)
            .await,
        Err(BlobError::NotFound(_))
    ));
    for active in [&versions, &foreign] {
        assert_eq!(state(&fixture, &active.current).await, "available");
        fixture
            .state
            .blob_store
            .get_manifest(&active.app, &active.current_hash)
            .await
            .unwrap();
        assert_current(&fixture, &active.app, &active.current_hash).await;
    }
    assert_eq!(fixture.rows().await.len(), 2);
}

#[derive(Debug)]
struct Gate {
    entered: oneshot::Sender<()>,
    released: oneshot::Receiver<()>,
}
#[derive(Debug)]
struct FaultStore {
    inner: Arc<dyn BlobStore>,
    fail: AtomicBool,
    deletes: AtomicUsize,
    gate: Mutex<Option<Gate>>,
}
impl FaultStore {
    fn new(inner: Arc<dyn BlobStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            fail: false.into(),
            deletes: 0.into(),
            gate: Mutex::new(None),
        })
    }
    fn gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, received) = oneshot::channel();
        let (release, released) = oneshot::channel();
        *self.gate.lock().unwrap() = Some(Gate { entered, released });
        (received, release)
    }
}
#[async_trait::async_trait(?Send)]
impl BlobStore for FaultStore {
    async fn get_blob(&self, hash: &str) -> Result<bytes::Bytes, BlobError> {
        self.inner.get_blob(hash).await
    }
    fn local_path(&self, hash: &str) -> Option<PathBuf> {
        self.inner.local_path(hash)
    }
    async fn put_blob_stream(
        &self,
        hash: &str,
        size: u64,
        reader: &mut dyn std::io::Read,
    ) -> Result<PutOutcome, BlobError> {
        self.inner.put_blob_stream(hash, size, reader).await
    }
    async fn has_blob(&self, hash: &str) -> Result<bool, BlobError> {
        self.inner.has_blob(hash).await
    }
    async fn probe(&self) -> Result<(), BlobError> {
        self.inner.probe().await
    }
    async fn get_blob_to_file(
        &self,
        hash: &str,
        out: &compio::fs::File,
        expected: Option<u64>,
        maximum: u64,
    ) -> Result<u64, BlobError> {
        self.inner
            .get_blob_to_file(hash, out, expected, maximum)
            .await
    }
    async fn put_manifest(&self, app: &AppId, hash: &str, bytes: &[u8]) -> Result<(), BlobError> {
        self.inner.put_manifest(app, hash, bytes).await
    }
    async fn get_manifest(&self, app: &AppId, hash: &str) -> Result<bytes::Bytes, BlobError> {
        self.inner.get_manifest(app, hash).await
    }
    async fn delete_manifest(&self, app: &AppId, hash: &str) -> Result<bool, BlobError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.entered.send(()).unwrap();
            gate.released
                .await
                .map_err(|_| BlobError::Backend("abandoned delete".into()))?;
        }
        if self.fail.swap(false, Ordering::SeqCst) {
            return Err(BlobError::Backend("injected delete failure".into()));
        }
        self.inner.delete_manifest(app, hash).await
    }
    async fn delete_app_manifests(&self, app: &AppId) -> Result<(), BlobError> {
        self.inner.delete_app_manifests(app).await
    }
}

#[ntex::test]
async fn collection_rotates_past_failures_and_recovers_manifest_deletion() {
    let fixture = Fixture::new().await;
    let first = seed_versions(&fixture, "collector-retry").await;
    let second = seed_versions(&fixture, "collector-later").await;
    let faults = FaultStore::new(fixture.state.blob_store.clone());
    faults.fail.store(true, Ordering::SeqCst);
    let mut scan = collector(&fixture, faults.clone(), 1).await;
    // Visit in actual native-id order. A failed first deletion must not pin
    // the page cursor and prevent the other eligible app from being collected.
    let ids: Vec<String> = fixture
        .platform
        .admin
        .query("SELECT id FROM zeroship.app_deploys ORDER BY id", &[])
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(ids.len(), 4);
    let mut failed = 0;
    let mut finished = 0;
    let mut newcomer = None;
    for (index, _) in ids.iter().enumerate() {
        let stats = scan.tick().await.unwrap();
        assert_eq!(stats.candidates, 1);
        failed += stats.failed;
        finished += stats.finished;
        if index == 0 {
            let fresh = seed_versions(&fixture, "collector-next-sweep").await;
            assert!(fresh.old > *ids.last().unwrap());
            newcomer = Some(fresh);
        }
    }
    let newcomer = newcomer.unwrap();
    assert_eq!(scan.tick().await.unwrap().candidates, 0);
    assert_eq!(state(&fixture, &newcomer.old).await, "available");
    assert_eq!(failed, 1);
    assert_eq!(finished, 1);
    let interrupted = if state(&fixture, &first.old).await == "reclaiming" {
        &first
    } else {
        &second
    };
    assert_eq!(state(&fixture, &interrupted.old).await, "reclaiming");
    fixture
        .state
        .blob_store
        .get_manifest(&interrupted.app, &interrupted.old_hash)
        .await
        .unwrap();
    // A fresh collector resumes fences, even when a new grace window excludes
    // every ordinarily available deployment.
    let mut restart = Collector::connect(
        &fixture.control_url,
        faults.clone(),
        DeployRetentionConfig {
            grace_window_ms: 3 * 24 * 60 * 60 * 1_000,
            ..DeployRetentionConfig::default()
        },
    )
    .await
    .unwrap();
    let recovered = restart.tick().await.unwrap();
    assert_eq!(recovered.finished, 1);
    assert_eq!(state(&fixture, &interrupted.old).await, "deleted");
    let mut next_sweep = collector(&fixture, faults.clone(), 128).await;
    assert_eq!(next_sweep.tick().await.unwrap().finished, 1);
    assert_eq!(state(&fixture, &newcomer.old).await, "deleted");

    let lost_finish = seed_versions(&fixture, "collector-finish").await;
    fixture.platform.admin.batch_execute(
        "CREATE FUNCTION zeroship.fail_collector_finish() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.retention_state='deleted' THEN RAISE EXCEPTION 'injected finish failure'; END IF; RETURN NEW; END $$; \
         CREATE TRIGGER fail_collector_finish BEFORE UPDATE ON zeroship.app_deploys \
         FOR EACH ROW EXECUTE FUNCTION zeroship.fail_collector_finish();"
    ).await.unwrap();
    let mut scan = collector(&fixture, faults.clone(), 128).await;
    let failed = scan.tick().await.unwrap();
    assert_eq!(failed.failed, 1);
    assert_eq!(state(&fixture, &lost_finish.old).await, "reclaiming");
    assert!(matches!(
        fixture
            .state
            .blob_store
            .get_manifest(&lost_finish.app, &lost_finish.old_hash)
            .await,
        Err(BlobError::NotFound(_))
    ));
    fixture
        .platform
        .admin
        .batch_execute("DROP TRIGGER fail_collector_finish ON zeroship.app_deploys")
        .await
        .unwrap();
    let mut restart = collector(&fixture, faults.clone(), 128).await;
    let finished = restart.tick().await.unwrap();
    assert_eq!(finished.finished, 1);
    assert_eq!(finished.manifests_deleted, 0);
    assert_eq!(state(&fixture, &lost_finish.old).await, "deleted");
}

#[ntex::test]
async fn committed_reclamation_refuses_activation_and_survives_cancellation() {
    let fixture = Fixture::new().await;
    let versions = seed_versions(&fixture, "collector-cancel").await;
    let faults = FaultStore::new(fixture.state.blob_store.clone());
    let (entered, release) = faults.gate();
    let mut scan = collector(&fixture, faults.clone(), 128).await;
    compio::time::timeout(Duration::from_secs(10), async {
        let pending = Box::pin(scan.tick());
        let remaining = match futures::future::select(pending, entered).await {
            futures::future::Either::Left(_) => panic!("collector bypassed delete gate"),
            futures::future::Either::Right((entered, pending)) => {
                entered.unwrap();
                pending
            }
        };
        assert_eq!(state(&fixture, &versions.old).await, "reclaiming");
        assert!(matches!(
            redeploy_old(&fixture, &versions).await,
            Err(CatalogError::DeploymentReclaimed)
        ));
        assert_current(&fixture, &versions.app, &versions.current_hash).await;
        fixture
            .state
            .blob_store
            .get_manifest(&versions.app, &versions.old_hash)
            .await
            .unwrap();
        drop(remaining);
        drop(release);
    })
    .await
    .expect("cancelled collector fixture stalled");
    let mut restart = collector(&fixture, faults.clone(), 128).await;
    assert_eq!(restart.tick().await.unwrap().finished, 1);
    assert_eq!(state(&fixture, &versions.old).await, "deleted");
    assert!(matches!(
        redeploy_old(&fixture, &versions).await,
        Err(CatalogError::DeploymentReclaimed)
    ));
    assert_current(&fixture, &versions.app, &versions.current_hash).await;
}

async fn waiter(observer: &compio_postgres::Client, blocker: i32) -> i32 {
    compio::time::timeout(Duration::from_secs(10), async {
        loop {
            let rows = observer
                .query(
                    "SELECT pid FROM pg_stat_activity WHERE usename='zeroship_control' \
                 AND wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid))",
                    &[&blocker],
                )
                .await
                .unwrap();
            if let Some(row) = rows.first() {
                assert_eq!(rows.len(), 1);
                return row.get(0);
            }
            compio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("expected database lock was not observed")
}

#[ntex::test]
async fn activation_holds_app_lock_before_collector_rechecks_current_deployment() {
    let fixture = Fixture::new().await;
    let versions = seed_versions(&fixture, "collector-activation").await;
    fixture.platform.admin.batch_execute(
        "CREATE FUNCTION zeroship.gate_collector_activation() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.activated_at IS DISTINCT FROM OLD.activated_at THEN \
         PERFORM pg_advisory_xact_lock(73921867); END IF; RETURN NEW; END $$; \
         CREATE TRIGGER gate_collector_activation BEFORE UPDATE ON zeroship.app_deploys \
         FOR EACH ROW EXECUTE FUNCTION zeroship.gate_collector_activation();"
    ).await.unwrap();
    let admin_url = fixture
        .control_url
        .replacen("zeroship_control@", "postgres@", 1);
    let blocker = platform::connect(&admin_url).await;
    let blocker_pid: i32 = blocker
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    blocker
        .query_one("SELECT pg_advisory_lock(73921867)", &[])
        .await
        .unwrap();
    let mut scan = collector(&fixture, fixture.state.blob_store.clone(), 128).await;
    compio::time::timeout(Duration::from_secs(20), async {
        let activation = redeploy_old(&fixture, &versions);
        let observer = async {
            let activation_pid = waiter(&fixture.platform.admin, blocker_pid).await;
            let collect = scan.tick();
            let release = async {
                let collector_pid = waiter(&fixture.platform.admin, activation_pid).await;
                assert_ne!(collector_pid, activation_pid);
                let unlocked: bool = blocker
                    .query_one("SELECT pg_advisory_unlock(73921867)", &[])
                    .await
                    .unwrap()
                    .get(0);
                assert!(unlocked);
            };
            let (stats, ()) = futures::join!(collect, release);
            assert_eq!(stats.unwrap().failed, 0);
        };
        let (activated, ()) = futures::join!(activation, observer);
        assert!(!activated.unwrap().replayed());
    })
    .await
    .expect("activation/collection race stalled");
    assert_current(&fixture, &versions.app, &versions.old_hash).await;
    assert_eq!(state(&fixture, &versions.old).await, "available");
    fixture
        .state
        .blob_store
        .get_manifest(&versions.app, &versions.old_hash)
        .await
        .unwrap();
}
