use super::*;
use crate::scheduling::{Options as SchedulingOptions, Scheduler};
use zeroship_core::{
    app_id::AppId,
    workflow_deployments::HoldScope,
    workflow_jobs::{DeploymentId, JobOperation},
    workflow_schedules::{ActivateSchedules, RegisterSchedules},
};

fn tables(path: &Path) -> Vec<String> {
    let connection = rusqlite::Connection::open(path).unwrap();
    let mut query = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT GLOB 'sqlite_*' ORDER BY name")
        .unwrap();
    let names = query
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    names
}

fn manifest() -> (String, String) {
    let mut manifest = zeroship_bundle::Manifest::default();
    let hash =
        zeroship_bundle::deployment_manifest_hash(&serde_json::to_vec(&manifest).unwrap()).unwrap();
    manifest.deploy_hash = Some(hash.clone());
    (hash, serde_json::to_string(&manifest).unwrap())
}

#[test]
fn combined_schema_contains_the_catalog_and_the_manager() {
    let combined = rusqlite::Connection::open_in_memory().unwrap();
    combined.execute_batch(SQLITE_SCHEMA).unwrap();
    let catalog = rusqlite::Connection::open_in_memory().unwrap();
    catalog.execute_batch(deployments::SQLITE_SCHEMA).unwrap();
    let combined = objects(&combined).unwrap();
    let catalog = objects(&catalog).unwrap();
    assert!(!catalog.is_empty());
    assert!(catalog.iter().all(|object| combined.contains(object)));
    for table in ["app_deploys", "jobs", "schedule_activations", "recovery_duties"] {
        assert!(
            combined.iter().any(|(name, _)| name == table),
            "combined schema lacks {table}"
        );
    }
    assert!(combined.len() > catalog.len());
}

#[compio::test]
async fn a_new_file_holds_the_catalog_and_manager_schema_and_reopens() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("nested/platform.sqlite");
    let platform = LocalPlatform::open(&path).await.unwrap();
    let names = tables(&path);
    for table in [
        "app_deploys",
        "app_deploy_holds",
        "schema_version",
        "queue_scopes",
        "jobs",
        "workers",
        "assignments",
        "schedule_activations",
        "recovery_duties",
    ] {
        assert!(names.iter().any(|name| name == table), "missing {table}");
    }
    let fingerprint: String = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT fingerprint FROM schema_version WHERE id = 'manager'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(fingerprint, include_str!("../../schema/fingerprint.txt").trim());
    let app = AppId::mint();
    let (hash, encoded) = manifest();
    let deployment = platform
        .deployments()
        .record_deployment(&app, &hash, &encoded)
        .await
        .unwrap();
    platform
        .queue(Options::default())
        .await
        .unwrap()
        .register_scope(&app)
        .await
        .unwrap();
    drop(platform);
    let reopened = LocalPlatform::open(&path).await.unwrap();
    assert_eq!(
        reopened
            .deployments()
            .record_deployment(&app, &hash, &encoded)
            .await
            .unwrap(),
        deployment
    );
    assert_eq!(tables(&path), names);
}

#[compio::test]
async fn partial_or_changed_files_are_refused_without_rewriting_them() {
    let (hash, _) = manifest();
    for (label, schema) in [
        ("deployment catalog only", deployments::SQLITE_SCHEMA.to_owned()),
        (
            "manager only",
            include_str!("../../schema/sqlite.sql").to_owned(),
        ),
        (
            "an additional table",
            format!("{SQLITE_SCHEMA}\nCREATE TABLE workflow_extra (id TEXT PRIMARY KEY);"),
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("platform.sqlite");
        let admin = rusqlite::Connection::open(&path).unwrap();
        admin.execute_batch(&schema).unwrap();
        let before = tables(&path);
        if before.iter().any(|name| name == "app_deploys") {
            admin
                .execute(
                    "INSERT INTO app_deploys (id, app_id, deploy_hash, manifest_json) VALUES ('dep_keep', 'app_keep', ?1, '{}')",
                    [&hash],
                )
                .unwrap();
        }
        assert!(
            matches!(
                LocalPlatform::open(&path).await,
                Err(deployments::Error::Unavailable(_))
            ),
            "{label} must be refused"
        );
        assert_eq!(tables(&path), before, "{label} was rewritten");
        if before.iter().any(|name| name == "app_deploys") {
            let kept: String = admin
                .query_row("SELECT id FROM app_deploys", [], |row| row.get(0))
                .unwrap();
            assert_eq!(kept, "dep_keep");
        }
    }
    // The exact combined compiler output remains acceptable.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("platform.sqlite");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(SQLITE_SCHEMA)
        .unwrap();
    LocalPlatform::open(&path).await.unwrap();
}

#[compio::test]
async fn queue_holds_its_deployment_through_the_same_catalog() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("platform.sqlite");
    let platform = LocalPlatform::open(&path).await.unwrap();
    let app = AppId::mint();
    let (hash, encoded) = manifest();
    let deployment = DeploymentId::parse(
        &platform
            .deployments()
            .record_deployment(&app, &hash, &encoded)
            .await
            .unwrap(),
    )
    .unwrap();
    let scheduler = Scheduler::new(
        platform.queue(Options::default()).await.unwrap(),
        SchedulingOptions::default(),
    )
    .unwrap();
    assert!(scheduler.selection(&app).await.unwrap().is_none());
    scheduler
        .prepare(&RegisterSchedules {
            app_id: app.clone(),
            deployment_id: deployment.clone(),
            schedules: vec![],
        })
        .await
        .unwrap();
    let job = scheduler
        .activate(&ActivateSchedules {
            app_id: app.clone(),
            deployment_id: deployment.clone(),
            revision: 1.try_into().unwrap(),
        })
        .await
        .unwrap();
    assert!(matches!(
        &job.operation,
        JobOperation::Activate { deployment_id, .. } if deployment_id == &deployment
    ));
    let selection = scheduler.selection(&app).await.unwrap().unwrap();
    let selected = selection.activation.unwrap();
    assert!(selection.enabled);
    assert_eq!(selected.job, job);
    assert_eq!(selected.deployment_id, deployment);
    assert!(!selected.ready);
    let holder = HoldScope::for_queue(app.clone());
    let state: String = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT state FROM app_deploy_holds WHERE app_id = ?1 AND deploy_id = ?2 AND holder_id = ?3",
            rusqlite::params![app.as_str(), deployment.as_str(), holder.holder()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "held");
}
