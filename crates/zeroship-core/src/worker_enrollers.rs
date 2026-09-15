//! The operator's worker enroller import document.
//!
//! An ENROLLER is a worker deployment unit's bootstrap credential: one Ed25519
//! keypair per host or pool, in exactly one execution zone. The private half is
//! mounted into the unit's workers as the credential
//! [`crate::service_peers::ServiceKeyring::load_worker_enroller`] reads; the
//! public half reaches Control through THIS document, which Control imports at
//! startup (`control.worker_enrollers_file`,
//! `crates/zeroship-control/src/worker_enrolment.rs`, `import_enrollers`).
//!
//! ```json
//! { "enrollers": [
//!     { "id": "wen_...", "zone": "default",
//!       "public_key": "<base64url, no padding, raw 32-byte Ed25519 key>" }
//! ] }
//! ```
//!
//! ONE definition for both ends: Control parses it, and `zeroship dev init`
//! reads and extends it, through [`parse_enroller_import`], so the writer can
//! never produce a document the reader refuses. What the import DOES with a
//! valid document is Control's and lives there.

use std::collections::BTreeSet;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};

/// The name of the execution zone every deployment declares, seeded by
/// `db/migrations-ts/20260914000450_execution_zones_default_zone.ts`. A
/// single-host deployment's one enroller lives in it.
pub const DEFAULT_EXECUTION_ZONE: &str = "default";

/// One enroller as the document names it, validated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnrollerRecord {
    /// The enroller's `wen_` typed id.
    pub id: String,
    /// The NAME of the execution zone the enroller's unit runs in.
    pub zone: String,
    /// The raw Ed25519 public key.
    pub public_key: [u8; 32],
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Document {
    enrollers: Vec<Entry>,
}

/// Unknown members are refused rather than ignored, so a misspelt `zone` is a
/// refused document and not an enroller filed in no zone.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    zone: String,
    public_key: String,
}

/// Parse and validate an import document.
///
/// # Errors
///
/// Returns a message naming the offending entry when the bytes are not the
/// document, the list is empty, an id is not a `wen_` typed id, a zone name is
/// empty or padded, a public key is not a usable Ed25519 key, or two entries
/// share an id or a public key. An empty list is refused because a configured
/// document naming no enroller is far likelier a wrong file than an intent.
pub fn parse_enroller_import(bytes: &[u8]) -> Result<Vec<EnrollerRecord>, String> {
    let document: Document = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if document.enrollers.is_empty() {
        return Err("the document names no enrollers".to_owned());
    }
    let mut ids = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut records = Vec::with_capacity(document.enrollers.len());
    for entry in document.enrollers {
        let record = validate(entry)?;
        if !ids.insert(record.id.clone()) {
            return Err(format!("enroller {} is named twice", record.id));
        }
        if !keys.insert(record.public_key) {
            return Err(format!(
                "enroller {} has a public key another entry already names; every enroller \
                 needs a key of its own",
                record.id
            ));
        }
        records.push(record);
    }
    Ok(records)
}

/// Render an import document, one entry per record, in the order given.
///
/// # Panics
///
/// Never: the document is plain strings, which always serialize.
#[must_use]
pub fn render_enroller_import(records: &[EnrollerRecord]) -> String {
    let document = Document {
        enrollers: records
            .iter()
            .map(|record| Entry {
                id: record.id.clone(),
                zone: record.zone.clone(),
                public_key: URL_SAFE_NO_PAD.encode(record.public_key),
            })
            .collect(),
    };
    let mut text = serde_json::to_string_pretty(&document).expect("the document serializes");
    text.push('\n');
    text
}

fn validate(entry: Entry) -> Result<EnrollerRecord, String> {
    crate::typed_id::parse_with_prefix(&entry.id, crate::typed_id::WORKER_ENROLLER_PREFIX)
        .map_err(|error| format!("{:?} is not a worker enroller id: {error}", entry.id))?;
    if entry.zone.is_empty() || entry.zone.trim() != entry.zone {
        return Err(format!(
            "enroller {} names zone {:?}, which is not an execution zone name",
            entry.id, entry.zone
        ));
    }
    let raw = URL_SAFE_NO_PAD
        .decode(entry.public_key.as_bytes())
        .map_err(|_| format!("enroller {}: public_key is not base64url", entry.id))?;
    let public_key = <[u8; 32]>::try_from(raw.as_slice())
        .map_err(|_| format!("enroller {}: public_key is not a raw Ed25519 key", entry.id))?;
    // A 32-byte string is not thereby a key. A point off the curve can verify
    // nothing, and a small-order one verifies signatures nobody made, so both
    // are refused here where the operator can still see which line it was.
    match ed25519_dalek::VerifyingKey::from_bytes(&public_key) {
        Ok(key) if !key.is_weak() => {}
        _ => {
            return Err(format!(
                "enroller {}: public_key is not a usable Ed25519 public key",
                entry.id
            ));
        }
    }
    Ok(EnrollerRecord {
        id: entry.id,
        zone: entry.zone,
        public_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> EnrollerRecord {
        EnrollerRecord {
            id: crate::typed_id::new_worker_enroller_id(),
            zone: DEFAULT_EXECUTION_ZONE.to_owned(),
            public_key: ed25519_dalek::SigningKey::from_bytes(&[3_u8; 32])
                .verifying_key()
                .to_bytes(),
        }
    }

    /// The writer's output is the reader's input: a rendered document parses
    /// back to the records it was rendered from.
    #[test]
    fn a_rendered_document_parses_back_to_its_records() {
        let mut second = record();
        second.id = crate::typed_id::new_worker_enroller_id();
        second.public_key = ed25519_dalek::SigningKey::from_bytes(&[4_u8; 32])
            .verifying_key()
            .to_bytes();
        let records = vec![record(), second];
        assert_eq!(
            parse_enroller_import(render_enroller_import(&records).as_bytes()),
            Ok(records)
        );
    }

    /// Each refusal changes one thing about an otherwise valid entry. The
    /// control is the unchanged entry, which parses.
    #[test]
    fn a_document_that_is_wrong_in_any_one_way_is_refused() {
        let valid = record();
        let key = URL_SAFE_NO_PAD.encode(valid.public_key);
        let document = |entries: serde_json::Value| {
            serde_json::to_vec(&serde_json::json!({ "enrollers": entries })).expect("json")
        };
        let entry = |id: &str, zone: &str, key: &str| {
            serde_json::json!({"id": id, "zone": zone, "public_key": key})
        };
        assert!(parse_enroller_import(&document(serde_json::json!([entry(
            &valid.id,
            DEFAULT_EXECUTION_ZONE,
            &key
        )])))
        .is_ok());

        let other_id = crate::typed_id::new_worker_enroller_id();
        for (label, bytes) in [
            ("not JSON", b"not json".to_vec()),
            ("no enrollers", document(serde_json::json!([]))),
            (
                "an unknown member",
                document(serde_json::json!([{
                    "id": valid.id, "zone": DEFAULT_EXECUTION_ZONE, "public_key": key,
                    "status": "active"
                }])),
            ),
            (
                "a worker instance id",
                document(serde_json::json!([entry(
                    "wkr_0000000000000000000000001",
                    DEFAULT_EXECUTION_ZONE,
                    &key
                )])),
            ),
            (
                "an empty zone",
                document(serde_json::json!([entry(&valid.id, "", &key)])),
            ),
            (
                "a padded zone",
                document(serde_json::json!([entry(&valid.id, " default", &key)])),
            ),
            (
                "a short key",
                document(serde_json::json!([entry(
                    &valid.id,
                    DEFAULT_EXECUTION_ZONE,
                    &URL_SAFE_NO_PAD.encode([7_u8; 31])
                )])),
            ),
            (
                "a small-order key",
                document(serde_json::json!([entry(
                    &valid.id,
                    DEFAULT_EXECUTION_ZONE,
                    &URL_SAFE_NO_PAD.encode([0_u8; 32])
                )])),
            ),
            (
                "one id twice",
                document(serde_json::json!([
                    entry(&valid.id, DEFAULT_EXECUTION_ZONE, &key),
                    entry(
                        &valid.id,
                        DEFAULT_EXECUTION_ZONE,
                        &URL_SAFE_NO_PAD.encode(
                            ed25519_dalek::SigningKey::from_bytes(&[5_u8; 32])
                                .verifying_key()
                                .to_bytes()
                        )
                    ),
                ])),
            ),
            (
                "one key twice",
                document(serde_json::json!([
                    entry(&valid.id, DEFAULT_EXECUTION_ZONE, &key),
                    entry(&other_id, DEFAULT_EXECUTION_ZONE, &key),
                ])),
            ),
        ] {
            assert!(
                parse_enroller_import(&bytes).is_err(),
                "{label} must be refused"
            );
        }
    }
}
