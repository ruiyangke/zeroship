//! Dev-only fixtures for the data plane.
//!
//! The data fixtures need the migration engine, so they cannot live in
//! `zeroship-testkit`, whose normal dependencies stay limited to container and
//! driver utilities. They are one crate rather than a private copy per
//! consuming crate: the ORM, the V8 adapter and the CDC relay all build the
//! same harness bindings and provision the same role ladder, and three copies
//! would drift. Nothing here is shipped; every consumer reaches it through
//! `[dev-dependencies]`.
//!
//! The surface speaks plain data and leaf crates only, never a domain crate's
//! types, so a crate whose own unit tests exercise the data plane can dev-depend
//! on it without a duplicate-type cycle. Each consumer converts in a thin
//! adapter beside its tests.
//!
//! [`data`] carries the shared surface: the harness binding, the role ladder,
//! the migration-engine schema helpers, the SQLite fixture tables and the CDC
//! platform corpus.

pub mod data;

/// Expand to a per-test app id derived from the enclosing function's name.
///
/// `test_app_id!()` for a test that needs one app, `test_app_id!("b")` for the
/// second app of a test that needs two. The discriminator is folded into the
/// digest, so the two ids differ in the part PostgreSQL cannot truncate away.
///
/// It is a macro because the name has to come from the COMPILER, not from
/// `std::thread::current().name()`. The thread name does equal the test name,
/// but only on the test's own thread; several fixtures read their app id from
/// helpers running elsewhere, where a thread-name read would be silently wrong
/// rather than absent.
#[macro_export]
macro_rules! test_app_id {
    () => {
        $crate::test_app_id!("")
    };
    ($discriminator:expr) => {{
        fn f() {}
        fn type_name_of<T>(_: T) -> &'static str {
            std::any::type_name::<T>()
        }
        $crate::data::test_app_id_from(type_name_of(f), $discriminator)
    }};
}
