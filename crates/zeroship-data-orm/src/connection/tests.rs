use super::*;
use futures::{future::LocalBoxFuture, FutureExt};
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct SingleReadFactory {
    calls: Arc<AtomicUsize>,
    registration: crate::sql::registration::SqlRegistration,
}

impl BackendFactory for SingleReadFactory {
    fn sql_registration(&self) -> crate::sql::registration::SqlRegistration {
        assert_eq!(self.calls.fetch_add(1, Ordering::SeqCst), 0);
        self.registration.clone()
    }

    fn connect(&self, _: ProjectKeySource) -> LocalBoxFuture<'_, Result<BackendHandle, DbError>> {
        async { Err(DbError::config("fixture", "connection is not expected")) }.boxed_local()
    }
}

#[test]
fn connection_factory_captures_the_sql_registration_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let _factory = ConnectionFactory::new(
        "single-read",
        SingleReadFactory {
            calls: calls.clone(),
            registration: crate::sql::registration::SqlRegistration::sqlite(),
        },
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[compio::test]
async fn direct_backend_handles_have_stable_distinct_connection_identities() {
    let first_directory = tempfile::tempdir().unwrap();
    let second_directory = tempfile::tempdir().unwrap();
    let open = |directory: &tempfile::TempDir| {
        BackendHandle::new(Rc::new(
            crate::backend_selection::new_sqlite_backend(
                directory.path().to_owned(),
                ProjectKeySource::unavailable(),
            )
            .unwrap(),
        ))
    };
    let first = open(&first_directory);
    let first_clone = first.clone();
    let second = BackendHandle::with_sql(
        Rc::new(
            crate::backend_selection::new_sqlite_backend(
                second_directory.path().to_owned(),
                ProjectKeySource::unavailable(),
            )
            .unwrap(),
        ),
        crate::sql::registration::SqlRegistration::sqlite(),
    )
    .unwrap();

    assert_eq!(
        first.connection_identity(),
        first_clone.connection_identity()
    );
    assert_ne!(first.connection_identity(), second.connection_identity());

    let captured = crate::tx_route::CapturedRoute::capture(
        None,
        "app_direct_backend_route",
        crate::sql::SchemaName::new("app_direct_backend_route").unwrap(),
        first.sql_registration().clone(),
        first.connection_identity(),
    );
    assert!(matches!(
        captured.bind(second),
        Err(DbError::Configuration {
            code: "backend_connection_mismatch",
            ..
        })
    ));
}

struct ControlledFactory {
    inner: ConnectionFactory,
    calls: Arc<AtomicUsize>,
    release: flume::Receiver<()>,
    fail_first: bool,
}
impl BackendFactory for ControlledFactory {
    fn sql_registration(&self) -> crate::sql::registration::SqlRegistration {
        self.inner.sql_registration().clone()
    }
    fn connect(
        &self,
        keys: ProjectKeySource,
    ) -> LocalBoxFuture<'_, Result<BackendHandle, DbError>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            self.release
                .recv_async()
                .await
                .expect("release fixture open");
            if self.fail_first && call == 0 {
                return Err(DbError::config("fixture_open_failed", "retry fixture"));
            }
            self.inner.connect(keys).await
        })
    }
}
fn controlled(
    fail_first: bool,
) -> (
    LocalConnection,
    Arc<AtomicUsize>,
    flume::Sender<()>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let inner = ConnectionFactory::for_url(&format!(
        "sqlite:{}",
        dir.path().join("connection.sqlite").display()
    ))
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let (release, receiver) = flume::unbounded();
    let factory = ConnectionFactory::new(
        "controlled",
        ControlledFactory {
            inner,
            calls: calls.clone(),
            release: receiver,
            fail_first,
        },
    );
    (LocalConnection::new(factory), calls, release, dir)
}

#[compio::test]
async fn a_captured_route_refuses_a_replacement_connection_with_the_same_sql_bundle() {
    let directory = tempfile::tempdir().unwrap();
    let first = ConnectionFactory::for_url(&format!(
        "sqlite:{}",
        directory.path().join("first.sqlite").display()
    ))
    .unwrap();
    let second = ConnectionFactory::for_url(&format!(
        "sqlite:{}",
        directory.path().join("second.sqlite").display()
    ))
    .unwrap();
    assert_eq!(
        first.sql_registration().identity(),
        second.sql_registration().identity()
    );
    let captured = crate::tx_route::CapturedRoute::capture(
        None,
        "app_route_connection",
        crate::sql::SchemaName::new("app_route_connection").unwrap(),
        first.sql_registration().clone(),
        first.identity(),
    );
    let replacement = second
        .connect(ProjectKeySource::unavailable())
        .await
        .unwrap();
    assert!(captured.bind(replacement).is_err());
}
#[compio::test]
async fn concurrent_callers_share_an_open_and_reuse_the_backend() {
    let (connection, calls, release, _dir) = controlled(false);
    let first = connection
        .ensure(ProjectKeySource::unavailable())
        .boxed_local();
    let second = connection
        .ensure(ProjectKeySource::unavailable())
        .boxed_local();
    futures::pin_mut!(first, second);
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut second).is_pending());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();
    let (first, second) = futures::join!(first, second);
    let first = first.unwrap();
    let second = second.unwrap();
    let again = connection
        .ensure(ProjectKeySource::unavailable())
        .await
        .unwrap();
    let concrete =
        |handle: &BackendHandle| handle.get::<crate::backend::SqliteBackend>().unwrap() as *const _;
    assert_eq!(concrete(&first), concrete(&second));
    assert_eq!(concrete(&first), concrete(&again));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
#[compio::test]
async fn cancelling_the_first_waiter_does_not_strand_initialization() {
    let (connection, calls, release, _dir) = controlled(false);
    let mut cancelled = connection
        .ensure(ProjectKeySource::unavailable())
        .boxed_local();
    assert!(futures::poll!(&mut cancelled).is_pending());
    drop(cancelled);
    release.send(()).unwrap();
    connection
        .ensure(ProjectKeySource::unavailable())
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
#[compio::test]
async fn a_failed_open_preserves_its_error_and_allows_retry() {
    let (connection, calls, release, _dir) = controlled(true);
    release.send(()).unwrap();
    let error = connection
        .ensure(ProjectKeySource::unavailable())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        DbError::Configuration {
            code: "fixture_open_failed",
            ..
        }
    ));
    assert!(connection.backend().is_none());
    release.send(()).unwrap();
    connection
        .ensure(ProjectKeySource::unavailable())
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
#[test]
fn configuration_identity_and_debug_follow_the_connection_contract() {
    let first = ConnectionFactory::for_url("postgres://user:secret@host/db").unwrap();
    let same = ConnectionFactory::for_url("postgres://user:secret@host/db").unwrap();
    let other = ConnectionFactory::for_url("postgres://user:different@host/db").unwrap();
    assert_eq!(first.identity(), same.identity());
    assert_ne!(first.identity(), other.identity());
    assert!(!format!("{first:?}").contains("secret"));
    let limit = ConnectionFactory::for_url_with_limit(
        first.url().unwrap(),
        NonZeroUsize::new(1),
        SessionAuthority::PerAppRole,
    )
    .unwrap();
    assert_ne!(first.identity(), limit.identity());
    let service = ConnectionFactory::for_url_with_limit(
        first.url().unwrap(),
        None,
        SessionAuthority::Connection,
    )
    .unwrap();
    assert_ne!(first.identity(), service.identity());
    fn thread_safe<T: Send + Sync>() {}
    thread_safe::<ConnectionFactory>();
}

mod url_selection {

    use super::{backend_for_url, BackendUrl};
    use std::path::PathBuf;

    #[test]
    fn postgres_urls_dispatch_to_postgres() {
        assert!(matches!(
            backend_for_url("postgres://localhost/dev").unwrap(),
            BackendUrl::Postgres
        ));
        assert!(matches!(
            backend_for_url("postgresql://localhost/dev").unwrap(),
            BackendUrl::Postgres
        ));
    }

    #[test]
    fn sqlite_urls_dispatch_to_sqlite() {
        assert_eq!(
            backend_for_url("sqlite:/tmp/dev.sqlite").unwrap(),
            BackendUrl::Sqlite {
                path: PathBuf::from("/tmp/dev.sqlite"),
            }
        );
        assert_eq!(
            backend_for_url("sqlite:///tmp/dev.sqlite").unwrap(),
            BackendUrl::Sqlite {
                path: PathBuf::from("/tmp/dev.sqlite"),
            }
        );
        assert_eq!(
            backend_for_url("file:./dev.sqlite").unwrap(),
            BackendUrl::Sqlite {
                path: PathBuf::from("./dev.sqlite"),
            }
        );

        assert_eq!(
            backend_for_url("./dev.sqlite").unwrap(),
            BackendUrl::Sqlite {
                path: PathBuf::from("./dev.sqlite"),
            }
        );
        assert_eq!(
            backend_for_url("sqlite://host/db").unwrap(),
            BackendUrl::Sqlite {
                path: PathBuf::from("host/db"),
            }
        );
    }

    #[test]
    fn memory_urls_cannot_select_a_backend() {
        for url in [
            ":memory:",
            "sqlite::memory:",
            "file::memory:",
            "sqlite:",
            "sqlite://",
            "file:",
            "sqlite:db?mode=memory&cache=shared",
        ] {
            assert!(!zeroship_core::db_url::is_sqlite_url(url), "{url}");
            assert!(
                matches!(
                    backend_for_url(url),
                    Err(zeroship_data_orm::error::DbError::Configuration {
                        code: "sqlite_file_required",
                        ..
                    })
                ),
                "{url}",
            );
        }
    }

    #[test]
    fn unknown_scheme_is_rejected() {
        let err = backend_for_url("mysql://localhost/dev").unwrap_err();
        assert!(matches!(
            err,
            zeroship_data_orm::error::DbError::Configuration {
                code: "unsupported_database_url_scheme",
                ..
            }
        ));
    }
}

#[compio::test]
async fn a_late_failed_waiter_cannot_clear_a_new_attempt() {
    let (connection, calls, release, _dir) = controlled(true);
    let mut first = connection
        .ensure(ProjectKeySource::unavailable())
        .boxed_local();
    let mut late = connection
        .ensure(ProjectKeySource::unavailable())
        .boxed_local();
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut late).is_pending());
    release.send(()).unwrap();
    assert!(first.await.is_err());
    let mut retry = connection
        .ensure(ProjectKeySource::unavailable())
        .boxed_local();
    assert!(futures::poll!(&mut retry).is_pending());
    assert!(late.await.is_err());
    let mut joined = connection
        .ensure(ProjectKeySource::unavailable())
        .boxed_local();
    assert!(futures::poll!(&mut joined).is_pending());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    release.send(()).unwrap();
    let (retry, joined) = futures::join!(retry, joined);
    assert!(retry.is_ok() && joined.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
