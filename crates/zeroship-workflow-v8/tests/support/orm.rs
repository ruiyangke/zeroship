use std::path::Path;
use zeroship_data_orm::{binding::DbBinding, encryption::ProjectKeySource, ConnectOptions};
use zeroship_workflow::service::{
    schema,
    store::{OrmStore, SchemaName},
};

pub async fn store(directory: &Path) -> OrmStore {
    let store = OrmStore::connect(
        DbBinding::new(
            "workflow",
            "test-deployment",
            SchemaName::new("workflow").unwrap(),
        ),
        ConnectOptions::new(
            format!("sqlite:{}", directory.join("app.sqlite").display()),
            ProjectKeySource::unavailable(),
        ),
    )
    .await
    .unwrap();
    schema::initialize_local(&store).await.unwrap();
    store
}
