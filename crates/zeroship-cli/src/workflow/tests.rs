use super::*;
use serde_json::json;
use zeroship_workflow::{
    operations::{RunState, SignalOptions, StartOptions},
    service::{AppWorkflows, RequestId},
};

#[test]
fn project_identity_survives_restart_and_concurrent_initialization() {
    let root = tempfile::tempdir().unwrap();
    let peers: Vec<_> = (0..8)
        .map(|_| {
            let path = root.path().to_owned();
            std::thread::spawn(move || project_identity(&path).unwrap())
        })
        .collect();
    let app = project_identity(root.path()).unwrap();
    for peer in peers {
        assert_eq!(peer.join().unwrap(), app);
    }
    assert_eq!(project_identity(root.path()).unwrap(), app);
    let other = tempfile::tempdir().unwrap();
    assert_ne!(project_identity(other.path()).unwrap(), app);
    std::fs::write(root.path().join(".zeroship/app-id"), "malformed").unwrap();
    assert!(project_identity(root.path()).is_err());
}

#[test]
fn local_configuration_rejects_unknown_and_invalid_limits() {
    assert!(toml::from_str::<LocalConfig>("unknown = true").is_err());
    let valid: LocalConfig =
        toml::from_str("bundle = 'built.zship'\n[worker]\ntask_slots = 2").unwrap();
    let root = tempfile::tempdir().unwrap();
    let valid = valid.resolve(root.path()).unwrap();
    assert_eq!(valid.bundle, Some(root.path().join("built.zship")));
    assert_eq!(valid.worker.task_slots, 2);
    let invalid = LocalConfig {
        max_source_bytes: 0,
        ..LocalConfig::default()
    };
    assert!(invalid.resolve(root.path()).is_err());
}

fn publish(path: &Path, version: &str) {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().unwrap().parent().unwrap();
    let output = std::process::Command::new("pnpm")
        .current_dir(workspace.join("sdks/vite-plugin"))
        .args(["exec", "tsx"])
        .arg(manifest.join("tests/fixtures/workflow-bundle.ts"))
        .arg(path.parent().unwrap())
        .arg(version)
        .output()
        .expect("run the workflow deploy compiler with pnpm");
    assert!(
        output.status.success(),
        "workflow fixture build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn client(root: &Path, config: &LocalConfig, app: &AppId) -> (WorkflowService, AppWorkflows) {
    let config = config.clone().resolve(root).unwrap();
    let service = WorkflowService::open(
        Arc::new(SqliteStore::new(config.journal)),
        Arc::new(HostPolicies::default()),
    )
    .await
    .unwrap();
    service
        .register_app(
            app,
            PolicySnapshot::configuration(1.try_into().unwrap(), AppPolicy::default()).unwrap(),
        )
        .await
        .unwrap();
    let api = service.for_app(app.clone());
    (service, api)
}

async fn await_state(
    api: &AppWorkflows,
    id: &str,
    state: RunState,
) -> zeroship_workflow::operations::RunStatus {
    compio::time::timeout(Duration::from_secs(15), async {
        loop {
            let status = api.status(id).await.unwrap();
            if status.state == state {
                return status;
            }
            assert_ne!(status.state, RunState::Failed, "{status:?}");
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background workflow progress")
}

#[compio::test]
async fn local_worker_retains_code_through_hot_reload_and_restart_without_http() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join("workflows.zship");
    publish(&bundle, "original");
    let config = LocalConfig {
        bundle: Some(bundle.clone()),
        bundle_poll_ms: 10,
        ..LocalConfig::default()
    };
    let env = [("APP_ID".into(), "untrusted-variable".into())].into();
    let host = LocalHost::start(
        root.path(),
        config.clone(),
        env,
        vec![],
        RuntimeLimits::default(),
    )
    .unwrap();
    let app = host.app.clone();
    let (service, api) = client(root.path(), &config, &app).await;
    let old = api
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    await_state(&api, &old.id, RunState::Waiting).await;

    publish(&bundle, "replacement");
    let path = tempfile::tempdir().unwrap();
    let blobs: Arc<dyn BlobStore> = Arc::new(LocalDiskBlobStore::new(path.path().into()).unwrap());
    let archive = std::fs::read(&bundle).unwrap();
    let ingested = zeroship_bundle::ingest(&blobs, &app.uuid(), &archive)
        .await
        .unwrap();
    let manifest = serde_json::from_str(&ingested.manifest_json).unwrap();
    let executable = BundleExecutable::load(&manifest, blobs.as_ref(), config.max_source_bytes)
        .await
        .unwrap();
    compio::time::timeout(Duration::from_secs(15), async {
        while service
            .deployment_by_hash(&app, executable.content_hash())
            .await
            .unwrap()
            .is_none()
        {
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Startup repairs/reselects the same immutable deploy without replacing old runs.
    drop(host);
    std::fs::remove_dir_all(root.path().join("src")).unwrap();
    let host = LocalHost::start(
        root.path(),
        config,
        HashMap::new(),
        vec![],
        RuntimeLimits::default(),
    )
    .unwrap();
    assert_eq!(host.app, app);
    let new = api
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    for (run, expected) in [
        (&old.id, "original:original:lazy"),
        (&new.id, "replacement:replacement:lazy"),
    ] {
        api.signal(
            &RequestId::mint(),
            run,
            SignalOptions {
                signal_type: "resume".into(),
                payload: json!(null),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            await_state(&api, run, RunState::Completed).await.output,
            Some(json!(expected))
        );
    }
    drop(host);
}
