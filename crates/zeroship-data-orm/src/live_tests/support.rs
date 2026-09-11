#[path = "../../../../tests/fixtures/data/mod.rs"]
mod shared;
pub use shared::*;

/// Install a fixture-owned PostgreSQL pool through the adapter's neutral seam.
pub fn install_postgres_pool(pool: std::rc::Rc<compio_postgres::Pool>, url: &str) {
    let backend = zeroship_data_orm::backend::PostgresBackend::new(
        pool,
        url.to_owned(),
        super::host::isolate_key_source(),
    );
    super::host::set_backend_for_tests(
        zeroship_data_orm::backend::BackendHandle::new(std::rc::Rc::new(backend)),
        url,
    );
}

pub async fn begin_transaction(app_id: &str, url: &str) {
    let pool = std::rc::Rc::new(
        compio_postgres::Pool::connect(url, 2)
            .await
            .expect("fixture pool"),
    );
    crate::support::roles::ensure_per_app_role(&pool, app_id)
        .await
        .expect("fixture role");
    if super::host::current_backend_for_tests().is_none() {
        install_postgres_pool(pool, url);
    }
    super::host::begin_transaction_for_tests(app_id).await;
}
