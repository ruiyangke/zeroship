//! [`AppId`] - the tenant identity of one creator app, as a typed id.
//!
//! An app id is not only a key: it SEEDS every physical name the data plane
//! gives one tenant - the `PostgreSQL` schema, both role names, the publication
//! digest, the replication-slot stem, the encryption salt, the KV scope, the
//! object-storage prefix, the bundle path segment, the meter key and the ring
//! position. [`crate::app_derivation`] is the one place that turns an app id
//! into any of them, and it takes this type so those derivations change
//! together or not at all.
//!
//! # It is declared by the macro, like every other entity id
//!
//! [`crate::entity_id::declare_entity_id`] generates the type, `mint`, `parse`,
//! `as_str`, serde and the tests that assert the absences - no `Display`, no
//! `AsRef<str>`, no `From<&str>`, no inherent `as_bytes`. Read that module for
//! what each absence prevents and for the collation contract every column
//! holding one of these ids owes, `zeroship.apps.id` included.
//!
//! `AppId` carried a hand-written body until the id became text, because it
//! also held the uuid the `zeroship.apps.id` column stored. Three symbols
//! served that column and are gone with it: `AppId::from_uuid` wrapped the
//! stored uuid so a derivation could be reached without re-keying anything,
//! `canonical_app_id_for` rendered the same uuid the way the column would
//! eventually hold it, and `AppId::uuid` handed back the embedded bits. Nothing
//! needs the bits. The two sites that were documented as needing them do not:
//! the app-secret AAD in `zeroship_control::env_store` takes a bare `Uuid` and
//! never sees an `AppId`, and a mismatch there is an AES-256-GCM tag failure
//! rather than a quiet one; the consistent-hash ring is a `BTreeMap` rebuilt in
//! the gateway process, so nothing persists a position for a printed form to
//! disagree with.
//!
//! # Scope
//!
//! `zeroship-cdc-wire` declares its own `AppId` over the same `app_` prefix. The
//! two are deliberately separate types in separate crates: that crate refuses a
//! normal dependency on this one because this crate's closure carries an HTTP
//! client, and `crates/zeroship-cdc-wire/tests/typed_id_oracle.rs` is the
//! differential test that keeps the duplicated parse from drifting from
//! [`crate::typed_id::parse_with_prefix`], which is the parse this type uses.

use crate::entity_id::declare_entity_id;
use crate::typed_id::APP_PREFIX;

declare_entity_id! {
    /// The typed id of one creator app: `app_<base62(uuidv7)>`.
    AppId,
    APP_PREFIX,
    app_id_tests,
}

#[cfg(test)]
mod tests {
    use super::AppId;
    use crate::typed_id::{self, APP_PREFIX, ParseError};

    /// The one look-alike no other entity id has: `oac_<base62-app-id>` is
    /// MINTED FROM the app id and carries the SAME hundred and twenty eight
    /// bits in the same encoding, so only the prefix separates them. That is
    /// why the boundary check is a prefix check, and why this arm asserts the
    /// error is a `WrongPrefix` boundary rejection rather than any refusal: a
    /// `Malformed` here would mean the body was what did the refusing, and the
    /// body is identical.
    ///
    /// The generic refusals - a foreign prefix, a hyphenated uuid, a body of
    /// the wrong length or outside base62, a non-string on the wire - are
    /// asserted for every macro-declared id in
    /// [`crate::entity_id::declare_entity_id`] and are not repeated here.
    #[test]
    fn parse_refuses_the_oauth_client_id_derived_from_the_same_app() {
        let app = uuid::Uuid::now_v7();
        let client_id = typed_id::app_oauth_client_id(&app);
        let err = AppId::parse(&client_id).expect_err("an oauth client id is not an app id");
        match err {
            ParseError::WrongPrefix { expected, got } => {
                assert_eq!(expected, APP_PREFIX);
                assert_eq!(got, typed_id::APP_OAUTH_CLIENT_PREFIX);
            }
            ParseError::Malformed(msg) => panic!("should be a boundary rejection, got {msg}"),
        }

        // The control: the two ids differ ONLY by prefix, so the arm above is
        // measuring the boundary and not a body the parser would refuse anyway.
        let canonical = format!("{APP_PREFIX}_{}", typed_id::uuid_to_base62(&app));
        assert_eq!(
            canonical.strip_prefix(APP_PREFIX),
            client_id.strip_prefix(typed_id::APP_OAUTH_CLIENT_PREFIX),
            "the two ids must share a body, or this is not a prefix test"
        );
        assert!(AppId::parse(&canonical).is_ok());
    }
}
