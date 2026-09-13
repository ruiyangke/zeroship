//! Control owns persistent project data keys. Workers receive the key for an
//! app's database-bound project through the authenticated internal API.

use crate::{registry::Registry, secret_cipher::SecretCipher};
use zeroship_core::{app_id::AppId, project_data_key::ProjectDataKey, project_id::ProjectId};

#[derive(Debug)]
pub(crate) enum KeyError {
    AppNotFound,
    Storage(String),
}

impl From<compio_postgres::Error> for KeyError {
    fn from(error: compio_postgres::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

pub(crate) async fn for_app(
    registry: &Registry,
    cipher: &SecretCipher,
    app: &AppId,
) -> Result<ProjectDataKey, KeyError> {
    let mut connection = registry
        .conn()
        .await
        .map_err(|error| KeyError::Storage(error.to_string()))?;
    let transaction = connection.transaction().await?;
    // Lock the authoritative project row before checking for a key. Apps in
    // the same project and concurrent control instances must choose one key.
    let projects = transaction
        .query(
            "SELECT p.id FROM zeroship.projects p JOIN zeroship.apps a ON a.project_id = p.id \
         WHERE a.id = $1 FOR UPDATE OF p FOR KEY SHARE OF a",
            &[&app.as_str()],
        )
        .await?;
    let project = projects
        .first()
        .ok_or(KeyError::AppNotFound)?
        .get::<_, String>("id");
    let project_id = ProjectId::parse(&project)
        .map_err(|_| KeyError::Storage("invalid project identity".into()))?;
    let aad = [
        b"zeroship:project-data-key\0".as_slice(),
        project.as_bytes(),
    ]
    .concat();
    let rows = transaction
        .query(
            "SELECT ciphertext FROM zeroship.project_data_keys WHERE project_id = $1",
            &[&project],
        )
        .await?;
    let key = if let Some(row) = rows.first() {
        let ciphertext: Vec<u8> = row.get("ciphertext");
        let (plaintext, rewrap) = cipher
            .open(&aad, &ciphertext)
            .map_err(|error| KeyError::Storage(error.to_string()))?;
        let bytes = plaintext
            .as_slice()
            .try_into()
            .map_err(|_| KeyError::Storage("invalid stored project key".into()))?;
        let key = ProjectDataKey::new(project_id, bytes);
        if rewrap {
            let ciphertext = cipher
                .seal(&aad, key.key())
                .map_err(|error| KeyError::Storage(error.to_string()))?;
            transaction
                .execute(
                    "UPDATE zeroship.project_data_keys SET ciphertext = $1 WHERE project_id = $2",
                    &[&ciphertext, &project],
                )
                .await?;
        }
        key
    } else {
        let key = ProjectDataKey::generate(project_id);
        let ciphertext = cipher
            .seal(&aad, key.key())
            .map_err(|error| KeyError::Storage(error.to_string()))?;
        transaction
            .execute(
                "INSERT INTO zeroship.project_data_keys (project_id, ciphertext) VALUES ($1, $2)",
                &[&project, &ciphertext],
            )
            .await?;
        key
    };
    transaction.commit().await?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use testcontainers::{
        core::{IntoContainerPort, WaitFor},
        runners::SyncRunner,
        GenericImage, ImageExt,
    };

    #[test]
    fn project_keys_survive_concurrent_provisioning_restarts_and_wrapping_key_rotation() {
        let postgres = GenericImage::new("postgres", "18")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "PostgreSQL init process complete; ready for start up.",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_PASSWORD", "project-key-fixture")
            .start()
            .expect("project-key tests require Docker and PostgreSQL");
        let url = format!(
            "postgres://postgres:project-key-fixture@{}:{}/postgres",
            postgres.get_host().unwrap(),
            postgres.get_host_port_ipv4(5432).unwrap()
        );
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let registry = Registry::new(&url).await.unwrap();
            let connection = registry.conn().await.unwrap();
            connection.batch_execute(
                "CREATE SCHEMA zeroship; \
                 CREATE TABLE zeroship.projects (id text COLLATE \"C\" PRIMARY KEY); \
                 CREATE TABLE zeroship.apps (id text COLLATE \"C\" PRIMARY KEY, project_id text REFERENCES zeroship.projects); \
                 CREATE TABLE zeroship.project_data_keys (project_id text COLLATE \"C\" PRIMARY KEY \
                    REFERENCES zeroship.projects ON DELETE CASCADE, ciphertext bytea NOT NULL); \
                 CREATE ROLE key_fixture_worker; GRANT USAGE ON SCHEMA zeroship TO key_fixture_worker;"
            ).await.unwrap();
            let first = ProjectId::mint();
            let second = ProjectId::mint();
            for project in [&first, &second] {
                connection.execute("INSERT INTO zeroship.projects VALUES ($1)", &[&project.as_str()]).await.unwrap();
            }
            let apps = [AppId::mint(), AppId::mint(), AppId::mint()];
            for (app, project) in [(&apps[0], &first), (&apps[1], &first), (&apps[2], &second)] {
                connection.execute("INSERT INTO zeroship.apps VALUES ($1, $2)", &[&app.as_str(), &project.as_str()]).await.unwrap();
            }
            let original = SecretCipher::new("original-master", &[]);
            let deliveries = futures::future::join_all((0..8).map(|i| for_app(&registry, &original, &apps[i % 2]))).await;
            let key = deliveries[0].as_ref().unwrap();
            for delivered in &deliveries {
                let delivered = delivered.as_ref().unwrap();
                assert_eq!(delivered.project_id, first);
                assert_eq!(delivered.key(), key.key());
            }
            let other = for_app(&registry, &original, &apps[2]).await.unwrap();
            assert_ne!(other.key(), key.key());
            let read_ciphertext = || async {
                connection.query("SELECT ciphertext FROM zeroship.project_data_keys WHERE project_id = $1", &[&first.as_str()])
                    .await.unwrap()[0].get::<_, Vec<u8>>("ciphertext")
            };
            let ciphertext = read_ciphertext().await;
            assert!(!ciphertext.windows(key.key().len()).any(|window| window == key.key()));
            let restarted = Registry::new(&url).await.unwrap();
            assert_eq!(for_app(&restarted, &original, &apps[1]).await.unwrap().key(), key.key());
            let rotated = SecretCipher::new("replacement-master", &["original-master"]);
            assert_eq!(for_app(&registry, &rotated, &apps[0]).await.unwrap().key(), key.key());
            assert_ne!(read_ciphertext().await, ciphertext);
            let current = SecretCipher::new("replacement-master", &[]);
            assert_eq!(for_app(&registry, &current, &apps[1]).await.unwrap().key(), key.key());
            let missing_app = AppId::mint();
            assert!(matches!(
                for_app(&registry, &current, &missing_app).await,
                Err(KeyError::AppNotFound)
            ));

            connection.batch_execute("SET ROLE key_fixture_worker").await.unwrap();
            let denied = connection.query("SELECT * FROM zeroship.project_data_keys", &[]).await.unwrap_err();
            assert_eq!(denied.code().map(|code| code.code()), Some("42501"));
            connection.batch_execute("RESET ROLE").await.unwrap();
            connection.execute("UPDATE zeroship.project_data_keys SET ciphertext = $1 WHERE project_id = $2", &[&ciphertext, &second.as_str()])
                .await.unwrap();
            assert!(for_app(&registry, &original, &apps[2]).await.is_err(), "another project's wrapped key must not authenticate");
            let invalid = b"corrupt key".to_vec();
            connection.execute("UPDATE zeroship.project_data_keys SET ciphertext = $1 WHERE project_id = $2", &[&invalid, &first.as_str()])
                .await.unwrap();
            assert!(for_app(&registry, &current, &apps[0]).await.is_err());
            assert_eq!(read_ciphertext().await, invalid, "corruption must never trigger key replacement");
            drop(connection);
            assert!(compio_postgres::drain_connections(std::time::Duration::from_secs(2)).await);
        });
    }
}
