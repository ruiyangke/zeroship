//! Immutable identity of one `env.db` binding.
//!
//! A worker thread can keep multiple V8 isolates for the same app alive at
//! different deploys. The app id alone therefore does not identify the schema
//! metadata a CRUD receiver was minted to use. `DbBinding` captures both parts
//! from the active isolate once and travels with every `Db` / `Collection`
//! wrapper and asynchronous CRUD continuation.
//!
//! # Two identities, carried separately
//!
//! `app_id` is the TENANT: the transaction-lane key, the broker routing key,
//! the metering subject, the CDC event stamp, the encryption salt. [`schema`]
//! is the PHYSICAL DATABASE SCHEMA: what query building qualifies tables with,
//! what DDL creates, and what the per-app PostgreSQL role is derived from.
//!
//! They hold the same characters today and are still two fields, because every
//! consumer has to say which one it means. A consumer that reads `app_id()`
//! where it wanted `schema()` is a bug that no longer waits for the two values
//! to diverge in order to be visible - it is visible in the source.
//!
//! [`schema`]: DbBinding::schema

use zeroship_data_sql::SchemaName;

/// Deploy token used when a host does not inject `ZEROSHIP_DEPLOY_ID` (local
/// dev, raw-JS deploys, and narrow test harnesses).
pub const COLD_START_DEPLOY_TOKEN: &str = "cold_start";

/// Identity of one app-at-deploy database binding.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DbBinding {
    app_id: String,
    deploy_token: String,
    schema: SchemaName,
}

impl DbBinding {
    /// Build a binding from the tenant identity, the deploy token, and the
    /// physical schema.
    ///
    /// The schema is handed in rather than derived here on purpose: deriving it
    /// would put the app-id-is-the-schema assumption inside the type that is
    /// meant to separate them. The one production derivation lives in
    /// `zeroship_data_v8::v8_classes::db::binding_for_isolate`.
    pub fn new(
        app_id: impl Into<String>,
        deploy_token: impl Into<String>,
        schema: SchemaName,
    ) -> Self {
        Self {
            app_id: app_id.into(),
            deploy_token: deploy_token.into(),
            schema,
        }
    }

    /// The binding a harness with no live isolate uses.
    ///
    /// Test-gated on purpose. Every PRODUCTION binding is minted by
    /// `v8_classes::db::binding_for_isolate` - called by
    /// `v8_classes::db::mint_db` and, for rehydrated `MaskedValue`s, by
    /// `v8_classes::masked_value::rehydrate_masked_values` - off the isolate's
    /// own `ZEROSHIP_DEPLOY_ID`; it falls back to [`COLD_START_DEPLOY_TOKEN`]
    /// only when the host injected none. Nothing in a shipped binary should be
    /// constructing a deploy identity from an app id alone, and this gate is
    /// what makes that checkable.
    ///
    /// # Panics
    ///
    /// Panics if `app_id` is not a legal schema name. This helper is the one
    /// place that still assumes the tenant id doubles as the schema, which is
    /// exactly why it is test-gated: production mints the schema through
    /// `SchemaName::new` and REFUSES the binding instead of panicking.
    #[cfg(feature = "test-helpers")]
    pub fn cold_start(app_id: impl Into<String>) -> Self {
        let app_id = app_id.into();
        let schema = SchemaName::new(&app_id)
            .expect("cold-start fixtures use app ids that are legal schema names");
        Self::new(app_id, COLD_START_DEPLOY_TOKEN, schema)
    }

    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    pub fn deploy_token(&self) -> &str {
        &self.deploy_token
    }

    /// The physical schema this binding's statements are qualified with.
    pub fn schema(&self) -> &SchemaName {
        &self.schema
    }
}
