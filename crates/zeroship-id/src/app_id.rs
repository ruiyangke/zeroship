//! [`AppId`] is the tenant identity of a creator app.
//!
//! App-scoped database, storage, metering, routing, and deployment code share
//! this type. Physical-name derivations live in `zeroship-core::app_derivation`.
//! The type stores only its canonical printed form and exposes it explicitly
//! through [`AppId::as_str`].

use crate::entity_id::declare_entity_id;
use crate::typed_id::APP_PREFIX;

declare_entity_id! {
    /// The typed id of one creator app: `app_<base36(uuidv7)>`.
    AppId,
    APP_PREFIX,
    app_id_tests,
}

/// Fixed app identity used by the isolated local development database.
pub const LOCAL_DEV_APP_ID: &str = "app_0000000002e4nenowz3qmamtd";

/// Return the local development identity as a validated [`AppId`].
#[must_use]
pub fn local_dev_app_id() -> AppId {
    AppId::parse(LOCAL_DEV_APP_ID).expect("LOCAL_DEV_APP_ID is canonical")
}

#[cfg(test)]
mod tests {
    use super::{local_dev_app_id, AppId, LOCAL_DEV_APP_ID};
    use crate::typed_id::{self, APP_PREFIX, ParseError};

    /// An OAuth client id derived from an app has the same body under `oac_`,
    /// so parsing it as an app must fail at the prefix boundary.
    #[test]
    fn parse_refuses_the_oauth_client_id_derived_from_the_same_app() {
        let app = AppId::mint();
        let client_id = typed_id::app_oauth_client_id(&app);
        let err = AppId::parse(&client_id).expect_err("an oauth client id is not an app id");
        match err {
            ParseError::WrongPrefix { expected, got } => {
                assert_eq!(expected, APP_PREFIX);
                assert_eq!(got, typed_id::APP_OAUTH_CLIENT_PREFIX);
            }
            ParseError::Malformed(msg) => panic!("should be a boundary rejection, got {msg}"),
        }

        let canonical = app.as_str().to_string();
        assert_eq!(
            canonical.strip_prefix(APP_PREFIX),
            client_id.strip_prefix(typed_id::APP_OAUTH_CLIENT_PREFIX),
            "the two ids must share a body, or this is not a prefix test"
        );
        assert!(AppId::parse(&canonical).is_ok());
    }

    #[test]
    fn local_dev_id_matches_the_shared_contract() {
        let contract: serde_json::Value = serde_json::from_str(include_str!(
            "../local-dev-app-id.json"
        ))
        .expect("local dev id contract parses");
        assert_eq!(contract["app_id"], LOCAL_DEV_APP_ID);
        assert_eq!(local_dev_app_id().as_str(), LOCAL_DEV_APP_ID);
    }
}
