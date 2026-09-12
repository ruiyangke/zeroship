use super::*;

fn fixture() -> (tempfile::TempDir, LocalConfig, AppId, StatePaths) {
    let root = tempfile::tempdir().unwrap();
    let app = super::super::project_identity(root.path()).unwrap();
    let config = LocalConfig::default().resolve(root.path()).unwrap();
    let paths = StatePaths::new(root.path(), &config, &app).unwrap();
    zeroship_workflow::service::schema::initialize_sqlite(&paths.journal).unwrap();
    (root, config, app, paths)
}

fn write(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"preserved").unwrap();
}

#[compio::test]
async fn reset_clears_objects_written_through_native_storage() {
    use std::sync::Arc;
    use zeroship_storage::{LocalFs, Namespace, StorageStore};
    let (root, config, app, paths) = fixture();
    let foreign = AppId::mint();
    let store = StorageStore::from_backend(Arc::new(LocalFs::new(&paths.objects)));
    let uploads = store.namespace(Namespace::app(app.as_str()).unwrap());
    uploads
        .put("uploads", "keep", b"customer object", None)
        .await
        .unwrap();
    for name in ["workflow", "workflow-snapshots"] {
        let scope = store.namespace(Namespace::platform(name).unwrap());
        scope
            .put(app.as_str(), "payload", b"workflow data", None)
            .await
            .unwrap();
        scope
            .put(foreign.as_str(), "payload", b"other app", None)
            .await
            .unwrap();
    }
    reset(root.path(), config).unwrap();
    assert_eq!(
        uploads.get("uploads", "keep").await.unwrap().unwrap().0,
        b"customer object"
    );
    for name in ["workflow", "workflow-snapshots"] {
        let scope = store.namespace(Namespace::platform(name).unwrap());
        assert!(scope.get(app.as_str(), "payload").await.unwrap().is_none());
        assert_eq!(
            scope
                .get(foreign.as_str(), "payload")
                .await
                .unwrap()
                .unwrap()
                .0,
            b"other app"
        );
    }
}

#[test]
fn reset_preserves_project_identity_and_unrelated_storage() {
    let (root, config, app, paths) = fixture();
    let foreign = AppId::mint();
    let preserved = [
        root.path().join(".zeroship/app-id"),
        root.path().join("app.zship"),
        root.path().join("business.sqlite"),
        root.path().join(".zeroship/kv.redb"),
        paths
            .objects
            .join("app:uploads")
            .join(app.as_str())
            .join("data"),
        paths
            .objects
            .join("platform:workflow")
            .join(foreign.as_str())
            .join("data"),
    ];
    for path in &preserved[1..] {
        write(path);
    }
    let before: Vec<_> = preserved
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    for bucket in &paths.buckets {
        write(&bucket.join("payload"));
    }
    reset(root.path(), config).unwrap();
    for (path, bytes) in preserved.iter().zip(before) {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert!(paths.buckets.iter().all(|path| !path.exists()));
    assert!(paths.markers.iter().all(|path| !path.exists()));
    zeroship_workflow::service::schema::initialize_sqlite(&paths.journal).unwrap();
}

#[test]
fn reset_refuses_business_tables_and_foreign_or_unverifiable_ownership() {
    for ddl in [
        "CREATE TABLE business (value TEXT)",
        "CREATE TABLE __zeroship_workflow_probe (app_id TEXT); INSERT INTO __zeroship_workflow_probe VALUES ('foreign')",
        "CREATE TABLE __zeroship_workflow_probe (app_id TEXT); INSERT INTO __zeroship_workflow_probe VALUES (NULL)",
        "CREATE TABLE __zeroship_workflow_probe (value TEXT)",
    ] {
        let (root, config, _, paths) = fixture();
        Connection::open(&paths.journal).unwrap().execute_batch(ddl).unwrap();
        let before = std::fs::read(&paths.journal).unwrap();
        assert!(reset(root.path(), config).is_err());
        assert_eq!(std::fs::read(&paths.journal).unwrap(), before);
        assert!(paths.markers.iter().all(|path| !path.exists()));
    }
}

#[test]
fn interrupted_reset_blocks_startup_and_resumes_under_its_original_scope() {
    let (root, config, _, paths) = fixture();
    for bucket in &paths.buckets {
        write(&bucket.join("payload"));
    }
    drop(Reset::prepare(root.path(), config.clone()).unwrap());
    assert!(paths.ensure_ready().unwrap_err().contains("unfinished"));
    std::fs::remove_file(&paths.journal).unwrap();
    let changed = LocalConfig {
        journal: root.path().join("other.sqlite"),
        ..config.clone()
    };
    assert!(reset(root.path(), changed)
        .unwrap_err()
        .contains("another app or configuration"));
    assert!(paths
        .buckets
        .iter()
        .all(|bucket| bucket.join("payload").exists()));
    reset(root.path(), config).unwrap();
    paths.ensure_ready().unwrap();
    assert!(paths.buckets.iter().all(|bucket| !bucket.exists()));
}

#[test]
fn reset_requires_existing_identity_and_refuses_orphan_sqlite_sidecars() {
    let root = tempfile::tempdir().unwrap();
    assert!(reset(root.path(), LocalConfig::default()).is_err());
    assert!(!root.path().join(".zeroship/app-id").exists());
    let (root, config, _, paths) = fixture();
    std::fs::remove_file(&paths.journal).unwrap();
    let sidecar = super::super::state::suffix(&paths.journal, "-wal");
    write(&sidecar);
    assert!(reset(root.path(), config)
        .unwrap_err()
        .contains("sidecars remain"));
    assert_eq!(std::fs::read(sidecar).unwrap(), b"preserved");
}

#[cfg(unix)]
#[test]
fn reset_rejects_linked_targets_and_does_not_follow_nested_links() {
    use std::os::unix::fs::symlink;
    let (root, config, _, paths) = fixture();
    let outside = root.path().join("outside");
    write(&outside.join("data"));
    for bucket in &paths.buckets {
        std::fs::create_dir_all(bucket).unwrap();
        symlink(&outside, bucket.join("linked")).unwrap();
    }
    reset(root.path(), config.clone()).unwrap();
    assert_eq!(std::fs::read(outside.join("data")).unwrap(), b"preserved");
    let alias = root.path().join("journal-alias");
    std::fs::hard_link(&paths.journal, &alias).unwrap();
    assert!(reset(root.path(), config.clone())
        .unwrap_err()
        .contains("hard links"));
    std::fs::remove_file(alias).unwrap();
    symlink(&outside, &paths.buckets[0]).unwrap();
    assert!(reset(root.path(), config)
        .unwrap_err()
        .contains("symbolic links"));
    assert_eq!(std::fs::read(outside.join("data")).unwrap(), b"preserved");
}
