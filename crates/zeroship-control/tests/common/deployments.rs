//! Real deployment manifests and catalog deploy commands for tests.
//!
//! A catalog deploy accepts only a manifest whose content hash verifies, so
//! fixtures seal manifests exactly as ingest does rather than inventing hashes.
#![allow(dead_code)]

use sha2::{Digest, Sha256};
use zeroship_bundle::Manifest;
use zeroship_control::publication::{
    Acceptance, CatalogError, CommandBinding, DeployCommand, VerifiedDeployment, ZSHIP_CONTENT_TYPE,
};
use zeroship_control::Registry;
use zeroship_core::{AppId, DeployCommandId, UserId};

/// Hash `manifest` as ingest does, embed that hash, and return the hash with
/// the stored manifest bytes.
pub fn sealed(mut manifest: Manifest) -> (String, String) {
    manifest.deploy_hash = None;
    let hash = zeroship_bundle::deployment_manifest_hash(
        &serde_json::to_vec(&manifest).expect("serialize manifest"),
    )
    .expect("hash manifest");
    manifest.deploy_hash = Some(hash.clone());
    (
        hash,
        serde_json::to_string(&manifest).expect("serialize sealed manifest"),
    )
}

/// A passthrough manifest told apart from others by its compiler label.
pub fn labelled(label: &str) -> Manifest {
    let mut manifest = Manifest::passthrough();
    manifest.metadata.compiler = Some(label.to_owned());
    manifest
}

/// The verified deployment for `manifest`.
pub fn verified(manifest: Manifest) -> VerifiedDeployment {
    let (hash, json) = sealed(manifest);
    VerifiedDeployment::verify(json, hash).expect("verified deployment")
}

/// A fresh deploy command for `deployment`. The manifest bytes stand in for
/// the archive whose digest a real upload binds.
pub fn command(app: &AppId, actor: &UserId, deployment: VerifiedDeployment) -> DeployCommand {
    let archive_sha256 = hex::encode(Sha256::digest(deployment.manifest_json().as_bytes()));
    DeployCommand {
        binding: CommandBinding {
            id: DeployCommandId::mint(),
            app: app.clone(),
            actor: actor.clone(),
            content_type: ZSHIP_CONTENT_TYPE,
            archive_sha256,
        },
        deployment,
        blobs_uploaded: 0,
        blobs_deduped: 0,
    }
}

/// Deploy `manifest` to `app` as `actor` through a fresh catalog command.
pub async fn deploy(
    registry: &Registry,
    app: &AppId,
    actor: &UserId,
    manifest: Manifest,
) -> Result<Acceptance, CatalogError> {
    registry
        .deploy(command(app, actor, verified(manifest)))
        .await
}
