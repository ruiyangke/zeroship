use std::path::Path;
use zeroship_data_orm::{
    binding::DbBinding, connection::ConnectionFactory, encryption::ProjectKeySource,
};
use zeroship_workflow::service::{
    schema,
    store::{OrmStore, SchemaName},
};

pub async fn store(directory: &Path) -> OrmStore {
    let store = OrmStore::connect(
        DbBinding::platform(
            "workflow",
            "test-deployment",
            SchemaName::new("workflow").unwrap(),
        ),
        &ConnectionFactory::for_url(&format!(
            "sqlite:{}",
            directory.join("app.sqlite").display()
        ))
        .unwrap(),
        ProjectKeySource::unavailable(),
    )
    .await
    .unwrap();
    schema::initialize_local(&store).await.unwrap();
    store
}
