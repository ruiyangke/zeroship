use super::*;
use serde_json::json;
use std::time::Duration;
use zeroship_workflow::{
    operations::{RunState, SignalOptions, StartOptions},
    service::{AppWorkflows, RequestId},
};

#[test]
fn local_configuration_rejects_unknown_and_invalid_limits() {
    assert!(toml::from_str::<LocalConfig>("unknown = true").is_err());
    assert!(toml::from_str::<LocalConfig>("bundle = 'built.zship'").is_err());
    let valid: LocalConfig = toml::from_str("[worker]\ntask_slots = 2").unwrap();
    assert!(toml::from_str::<LocalConfig>("journal = 'custom.sqlite'").is_err());
    assert!(toml::from_str::<LocalConfig>("objects = 'custom-objects'").is_err());
    let valid = valid.validate().unwrap();
    assert_eq!(valid.worker.task_slots, 2);
    let invalid = LocalConfig {
        max_source_bytes: 0,
        ..LocalConfig::default()
    };
    assert!(invalid.validate().is_err());
}

fn publish(path: &Path, version: &str) {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().unwrap().parent().unwrap();
    let output = std::process::Command::new("pnpm")
        .current_dir(workspace.join("sdks/vite-plugin"))
        .args(["exec", "tsx"])
        .arg(manifest.join("tests/fixtures/app-bundle.ts"))
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

async fn client(root: &Path, app: &AppId) -> (WorkflowService, AppWorkflows) {
    let service = WorkflowService::open(
        Rc::new(test_storage(root, app).open().await.unwrap()),
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
async fn local_worker_retains_code_across_app_rebuild_and_restart_without_http() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join("app.zship");
    publish(&bundle, "original");
    let config = LocalConfig::default();
    let app = zeroship_core::app_id::local_dev_app_id();
    let env = [("APP_ID".into(), "untrusted-variable".into())].into();
    let host = LocalHost::start(
        root.path(),
        app.clone(),
        config.clone(),
        Some(bundle.clone()),
        test_storage(root.path(), &app),
        env,
        vec![],
        RuntimeLimits::default(),
    )
    .unwrap();
    assert_eq!(host.app, app);
    assert!(!root.path().join(".zeroship/app-id").exists());
    let (_service, api) = client(root.path(), &app).await;
    let old = api
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    await_state(&api, &old.id, RunState::Waiting).await;

    publish(&bundle, "replacement");
    // Startup repairs/reselects the same immutable deploy without replacing old runs.
    drop(host);
    std::fs::remove_dir_all(root.path().join("src")).unwrap();
    let host = LocalHost::start(
        root.path(),
        app.clone(),
        config.clone(),
        Some(bundle.clone()),
        test_storage(root.path(), &app),
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
    await_state(&api, &new.id, RunState::Waiting).await;
    drop(host);
    std::fs::remove_file(&bundle).unwrap();
    let host = LocalHost::start(
        root.path(),
        app.clone(),
        config,
        None,
        test_storage(root.path(), &app),
        HashMap::new(),
        vec![],
        RuntimeLimits::default(),
    )
    .unwrap();
    assert_eq!(host.app, app);
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

fn test_storage(root: &Path, app: &AppId) -> HostStorage {
    HostStorage {
        connection: zeroship_data_orm::connection::ConnectionFactory::for_url(&format!(
            "sqlite:{}",
            root.join(".zeroship/dev.sqlite").display()
        ))
        .unwrap(),
        keys: zeroship_data_orm::encryption::ProjectKeySource::unavailable(),
        binding: zeroship_data_orm::binding::DbBinding::new(
            app.as_str(),
            "test-deployment",
            zeroship_core::schema_name::SchemaName::new(
                &zeroship_core::app_derivation::schema_name(app),
            )
            .unwrap(),
        ),
        objects: zeroship_storage::StorageStore::from_backend(Arc::new(
            zeroship_storage::LocalFs::new(root.join(".zeroship/storage")),
        )),
    }
}
