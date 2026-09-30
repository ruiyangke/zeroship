use std::path::Path;
use zeroship_data_orm::connection::ConnectionFactory;
use zeroship_workflow::service::{
    schema,
    store::{HostStorage, OrmStore},
};

/// A journal opened the way a host opens it, over a session file in
/// `directory`.
pub async fn store(directory: &Path) -> OrmStore {
    let store = HostStorage::new(
        ConnectionFactory::for_platform_url(&format!(
            "sqlite:{}",
            directory.join("app.sqlite").display()
        ))
        .unwrap(),
    )
    .open()
    .await
    .unwrap();
    schema::initialize_local(&store).await.unwrap();
    store
}
