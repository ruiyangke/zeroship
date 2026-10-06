//! Private setup for database and implementation tests in this crate.
//!
//! The shared data fixtures are the single source in `zeroship-data-testkit`,
//! reached as an ordinary `[dev-dependencies]` entry. [`adapter`] converts the
//! testkit's plain identities and field maps into this crate's types, so the
//! unit tests use the same role ladder, binding minting and schema helpers the
//! adapter and relay binaries link.
mod adapter;
pub(crate) use adapter::*;
pub(crate) use zeroship_testkit::postgres::server as postgres;

/// Expand to a per-test app id derived from the enclosing function's name.
///
/// The testkit's own `test_app_id!` cannot be re-exported here: this is a
/// unit-test module inside `zeroship-data-orm`, so `$crate` in that macro would
/// name the ORM rather than the testkit. The body is the same expansion against
/// this module's `test_app_id_from`.
macro_rules! test_app_id {
    () => {
        $crate::tests::fixtures::test_app_id!("")
    };
    ($discriminator:expr) => {{
        fn f() {}
        fn type_name_of<T>(_: T) -> &'static str {
            std::any::type_name::<T>()
        }
        $crate::tests::fixtures::test_app_id_from(type_name_of(f), $discriminator)
    }};
}
pub(crate) use test_app_id;

mod host;
pub(crate) use host::Host;
mod database;
pub(crate) use database::DatabaseFixture;
pub(crate) mod events;
mod state;
pub(crate) use state::{
    cache_schema, cache_schema_for_deploy, generated_schema, native_fields, reset_engine,
};
mod unit;
pub(crate) use unit::{unit_backend, unit_route};

/// A masked cell as the read pipeline produces it: `users.ssn` of row `usr_1`,
/// classified `spi`, displaying `***-**-6789`.
pub(crate) fn masked_cell() -> crate::value::Value {
    let schema = crate::schema::CollectionSchema::from_fields(&crate::value!({
        "ssn": {"type": "string", "mask": {"kind": "last4", "classification": "spi"}}
    }))
    .unwrap()
    .into_fields();
    let mut row = crate::value!({"id": "usr_1", "ssn": "123-45-6789"});
    crate::protection::mask_pass::wrap_row_on_read(&schema, "users", &mut row).unwrap();
    let crate::value::Value::Object(mut fields) = row else {
        panic!("wrap_row_on_read keeps a record a record");
    };
    let cell = fields.swap_remove("ssn").unwrap();
    assert!(cell.as_masked().is_some(), "{cell:?}");
    cell
}
