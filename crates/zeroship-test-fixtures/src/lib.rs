//! Infrastructure owned by tests, consumed through development dependencies.
//! Fixture guards must outlive the clients and services that use them.
pub mod postgres;
