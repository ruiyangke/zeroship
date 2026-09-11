//! The live catalog is a FLOOR on a column's protections. The descriptor may
//! raise it; it may never lower it.
//!
//! # The defect this exists for
//!
//! Encryption and masking were decided from the creator-authored runtime
//! descriptor alone: `crud::write_pipeline`'s private `WriteStages` reads
//! `def["mask"]` and `def["encrypted"]` off the descriptor's field map, and a
//! field carrying neither is written verbatim. So DELETING ONE JSON KEY from a
//! field turned a protected column into a plaintext one, with no refusal and no
//! signal - and the next write stored the real value under the field's own
//! name while the `__zs_raw__<col>` sibling the platform built for it sat NULL.
//!
//! Measured on live `PostgreSQL` before this module existed, one masked column and
//! one encrypted column, each written twice through the real pipeline with only
//! the descriptor changing between the two writes:
//!
//! ```text
//! ssn    = "***"               __zs_raw__ssn = "123-45-6789"   <- mask declared
//! ssn    = "987-65-4321"       __zs_raw__ssn = NULL            <- `mask` key deleted
//! secret = <ciphertext bytes>                                  <- encryption declared
//! secret = "hunter3-also-real"                                 <- `encrypted` key deleted
//! ```
//!
//! # Why the catalog is the authority for this and the descriptor is not
//!
//! `crate::descriptor`'s header argues that the catalog "could only ever agree
//! with the descriptor or be stale", because both are folded from the same
//! migration DSL. That is right about SHAPE and wrong about PROTECTION, and the
//! difference is who writes each one:
//!
//! * The descriptor travels inside the `.zship` the worker executes. It is
//!   creator-authored, and the worker is the process that runs creator code.
//! * The sentinels (`zero-migrate:mask:kind=…`, `zero-migrate:enc:<keyId>:<wraps>`) and the
//!   `__zs_raw__<col>` sibling are written by the MIGRATION SERVICE, which does
//!   not execute creator code, under a migration the diff classifier already
//!   grades `ChangeKind::MaskRemove` / `ChangeClass::Destructive`.
//!
//! AGENTS.md's standing invariant is that privilege follows the PROCESS: state a
//! separate service writes and the worker only reads is the one thing the worker
//! must not be able to forge. A protection record is exactly that shape. So the
//! two are not two derivations of one fact - one is a claim by the untrusted
//! side, the other is a record by the trusted one, and when they disagree the
//! untrusted one does not win.
//!
//! The stale case the descriptor header worries about is real and is why this
//! fails CLOSED: a catalog that still declares a protection the descriptor has
//! dropped means either the creator deployed without applying the migration, or
//! the migration was refused. Both are broken deploys, and refusing the write is
//! how a creator finds out.
//!
//! # What it does NOT do
//!
//! It does not read the mask KIND to decide anything, does not sample rows, does
//! not decrypt, and needs no key material. It compares PRESENCE: a column the
//! catalog records as protected must be declared protected by the descriptor
//! too. Raising a protection (declaring a mask the catalog does not have) is a
//! `MaskBackfill` migration's business and is not refused here.
//!
//! # Cost
//!
//! One catalog read per `(app, deploy)` per isolate, cached for the isolate's
//! life. The key carries the DEPLOY TOKEN for the same reason
//! `zeroship_data_orm::schema_cache`'s does: a migration that legitimately
//! removes a protection arrives with a new deploy, so the new binding misses the
//! cache and re-reads. An isolate that outlives the deploy keeps refusing, which
//! is the correct direction to be wrong in.
//!
//! The read takes a pooled checkout, which a caller inside `db.transaction(fn)`
//! also does - `crate::protection::unmask` and the audit-row writer take one on the
//! same path, and `PostgresBackend::fixture_session`'s header records
//! that as the designed shape rather than a leak.

use std::collections::HashMap;
use std::rc::Rc;

use zeroship_data_sql::value::Value;

use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::error::DbError;

use crate::tx_route::TxRoute;

/// Which protections the LIVE database records for one column.
///
/// Both flags come from sentinels the migration service wrote, recovered by the
/// backend's own introspector: `PostgreSQL` parses `pg_description`, `SQLite`
/// parses `sqlite_master.sql`. Neither is derived from the descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct StoredProtection {
    masked: bool,
    encrypted: bool,
}

impl StoredProtection {
    const fn any(self) -> bool {
        self.masked || self.encrypted
    }
}

/// One app-at-deploy's protection floor: `collection -> column -> protections`.
///
/// Only PROTECTED columns are held. A collection with none is absent, and the
/// lookup treats absence as "nothing to enforce" - correct, because a column the
/// catalog does not record as protected imposes no floor.
pub(crate) type ProtectionFloor = HashMap<String, HashMap<String, StoredProtection>>;

fn floor_key(binding: &DbBinding) -> DbBinding { binding.clone() }

/// Reduce a `LiveSchema` to the protected columns alone.
fn floor_from_live(live: &zeroship_data_sql::catalog::LiveSchema) -> ProtectionFloor {
    let mut out: ProtectionFloor = HashMap::new();
    for (table, columns) in &live.tables {
        let protected: HashMap<String, StoredProtection> = columns
            .iter()
            .filter_map(|(column, info)| {
                let stored = StoredProtection {
                    masked: info.mask.is_some(),
                    encrypted: info.encryption.is_some(),
                };
                stored.any().then(|| (column.clone(), stored))
            })
            .collect();
        if !protected.is_empty() {
            out.insert(table.clone(), protected);
        }
    }
    out
}

/// Does the descriptor's field definition declare a mask that will actually be
/// applied?
///
/// `mask: { kind: "none" }` is the documented opt-out, so it is NOT a
/// declaration: [`crate::protection::mask_pass::apply_mask_on_write`] skips it exactly
/// as it skips an absent block, and both therefore store plaintext under the
/// field's own name. A fence that accepted `kind: "none"` would refuse the
/// one-key deletion and wave through the one-word edit that does the same thing.
pub(crate) fn descriptor_declares_mask(def: &Value) -> bool {
    zeroship_data_sql::descriptors::effective_mask(def).is_some()
}

/// Does the descriptor's field definition declare encryption?
///
/// Presence of the block is the whole test, matching
/// [`crate::protection::encryption_pass::encrypt_row_on_write_with_sidechannel`],
/// which encrypts whenever `def["encrypted"]` is an object. There is no
/// `mode: "none"` opt-out to mirror.
pub(crate) fn descriptor_declares_encryption(def: &Value) -> bool {
    def.get("encrypted").is_some_and(Value::is_object)
}

/// Resolve (and memoise) this binding's protection floor.
async fn resolve_floor(
    route: &TxRoute,
    binding: &DbBinding,
) -> Result<Rc<ProtectionFloor>, DbError> {
    let key = floor_key(binding);
    if let Some(hit) = crate::orm_context::current().floors(|f| f.get(&key).cloned()) {
        return Ok(hit);
    }
    // Boxed: an inline `LiveSchema` future makes the whole write path's future
    // ~16 KB, and it is constructed on EVERY write while the await it guards
    // runs at most once per binding. The allocation is on the cold arm; the
    // stack saving is on the hot one.
    let live = Box::pin(route.backend().introspect_schema(binding.app_id())).await?;
    let floor = Rc::new(floor_from_live(&live));
    crate::orm_context::current().floors_mut(|f| f.insert(key, Rc::clone(&floor)));
    Ok(floor)
}

/// Refuse a write to `collection` when the descriptor has dropped a protection
/// the live database still records for one of its columns.
///
/// Refuses the whole collection, not just documents that mention the affected
/// column. A write that happens to omit `ssn` is harmless in itself, but letting
/// it through means the deploy appears to work until the first write that does
/// mention it - which is the silent shape this fence exists to remove.
///
/// # Errors
///
/// [`DbError::config`] with code `protection_removed_from_descriptor`, naming
/// the column and which protection the database still records. Also the
/// backend's own catalog-read failure, which is propagated rather than swallowed
/// so an unreadable catalog refuses the write instead of permitting it.
pub async fn refuse_protection_downgrade(
    route: &TxRoute,
    binding: &DbBinding,
    collection: &str,
    schema: &Value,
) -> Result<(), DbError> {
    let floor = resolve_floor(route, binding).await?;
    refuse_offences(&floor, collection, schema)
}

/// The verdict, split from the catalog read above.
///
/// Split on 2026-09-04, and the reason is a measurement rather than a taste.
/// The only place this fence's REFUSAL was bound was
/// `zeroship-data-v8/tests/mask_flip.rs`, whose test build enables
/// `test-helpers` - so the durable proof that a
/// protection downgrade is refused came from a build configuration that DOES
/// NOT SHIP. On the same day, the capability this fence reads
/// (`Catalog`) turned out to be gated on that same feature while the
/// caller was not, and the shipped binaries had not compiled for a day. A fence
/// whose only witness needs the feature is one flag away from being a fence
/// that only exists in test builds.
///
/// Everything above this line needs a backend; nothing below it does. Taking
/// the resolved floor as a parameter is what lets `#[cfg(test)] mod tests`
/// drive the refusal in the engine's DEFAULT-feature build, where
/// `feature = "test-helpers"` is off.
fn refuse_offences(
    floor: &ProtectionFloor,
    collection: &str,
    schema: &Value,
) -> Result<(), DbError> {
    let Some(stored_columns) = floor.get(collection) else {
        return Ok(());
    };
    let fields = schema.as_object();

    // Deterministic order: a refusal must name the same column on every run, or
    // the failure a creator sees depends on hash iteration.
    let mut offences: Vec<(&String, &'static str)> = Vec::new();
    for (column, stored) in stored_columns {
        let def = fields.and_then(|f| f.get(column.as_str()));
        // A column the descriptor no longer DECLARES AT ALL is not this fence's
        // business: nothing can be written to it, so nothing can be downgraded.
        let Some(def) = def else { continue };
        if stored.masked && !descriptor_declares_mask(def) {
            offences.push((column, "masked"));
        }
        if stored.encrypted && !descriptor_declares_encryption(def) {
            offences.push((column, "encrypted"));
        }
    }
    if offences.is_empty() {
        return Ok(());
    }
    offences.sort_unstable();

    let detail = offences
        .iter()
        .map(|(column, protection)| format!("{column} ({protection})"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(DbError::config(
        "protection_removed_from_descriptor",
        format!(
            "db: collection '{collection}' is refused because this deploy's runtime schema \
             descriptor removed a protection the database still records: {detail}. The database \
             is the authority on whether a column is protected; a descriptor may add a \
             protection but may not drop one. Remove it with a migration, which rewrites the \
             stored values and the column's sentinel, and redeploy."
        ),
    ))
}

/// Empty this isolate's resolved floors.
///
/// The peer of the schema-cache and lane resets, called from the same helper: a
/// test that installs a second descriptor over the same binding would otherwise
/// see the floor the first one resolved.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_tests() {
    crate::orm_context::current().floors_mut(HashMap::clear);
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_data_sql::catalog::WrappedType;
    use zeroship_data_sql::catalog::{
        Classification, ColumnInfo, EncryptionMeta, LiveSchema, MaskKind, MaskMeta,
    };

    use zeroship_data_sql::value;

    fn masked_column() -> ColumnInfo {
        ColumnInfo {
            mask: Some(MaskMeta {
                kind: MaskKind::Last4,
                classification: Classification::Pci,
                sibling_column: zeroship_data_sql::compile::raw_column_name("ssn"),
            }),
            ..Default::default()
        }
    }

    fn encrypted_column() -> ColumnInfo {
        ColumnInfo {
            encryption: Some(EncryptionMeta {
                key_id: "k1".to_string(),
                wraps: WrappedType::String,
            }),
            ..Default::default()
        }
    }

    fn live_with(column: &str, info: ColumnInfo) -> LiveSchema {
        let mut live = LiveSchema::default();
        live.tables
            .entry("people".to_string())
            .or_default()
            .insert(column.to_string(), info);
        live
    }

    #[test]
    fn the_floor_keeps_only_protected_columns() {
        let mut live = live_with("ssn", masked_column());
        live.tables
            .get_mut("people")
            .unwrap()
            .insert("nickname".to_string(), ColumnInfo::default());
        let floor = floor_from_live(&live);
        let people = floor.get("people").expect("the protected table survives");
        assert!(people.contains_key("ssn"));
        assert!(
            !people.contains_key("nickname"),
            "an unprotected column imposes no floor and must not be carried: {people:?}",
        );
    }

    /// The floor records the two protections independently.
    ///
    /// A column can be encrypted without being masked (`t.encrypted(...)` with
    /// no `.mask()` at the wire level), and the fence has to refuse a descriptor
    /// that dropped EITHER. Collapsing them to one "protected" bit would let a
    /// descriptor keep the mask, drop the encryption, and pass.
    #[test]
    fn masked_and_encrypted_are_recorded_separately() {
        let masked = floor_from_live(&live_with("ssn", masked_column()));
        assert_eq!(
            masked["people"]["ssn"],
            StoredProtection {
                masked: true,
                encrypted: false
            },
        );
        let encrypted = floor_from_live(&live_with("secret", encrypted_column()));
        assert_eq!(
            encrypted["people"]["secret"],
            StoredProtection {
                masked: false,
                encrypted: true
            },
        );
    }

    #[test]
    fn a_table_with_no_protected_column_is_absent_from_the_floor() {
        let live = live_with("nickname", ColumnInfo::default());
        assert!(
            floor_from_live(&live).is_empty(),
            "an entry for a table with nothing to enforce is dead weight the \
             lookup would have to skip",
        );
    }

    #[test]
    fn kind_none_is_not_a_mask_declaration() {
        // The one-word edit and the one-key deletion reach the same write
        // behaviour, so the fence must read them the same way.
        assert!(!descriptor_declares_mask(&value!({ "type": "string" })));
        assert!(!descriptor_declares_mask(
            &value!({ "type": "string", "mask": { "kind": "none" } })
        ));
        assert!(descriptor_declares_mask(
            &value!({ "type": "string", "mask": { "kind": "last4" } })
        ));
        // No `kind` at all defaults to `full` in the write pass, so it IS a
        // declaration.
        assert!(descriptor_declares_mask(
            &value!({ "type": "string", "mask": { "classification": "pii" } })
        ));
    }

    #[test]
    fn encryption_is_declared_by_the_block_alone() {
        assert!(!descriptor_declares_encryption(
            &value!({ "type": "string" })
        ));
        assert!(descriptor_declares_encryption(
            &value!({ "type": "string", "encrypted": {  } })
        ));
        // A non-object `encrypted` is not a declaration; the encryption pass
        // reads it with `as_object()` and skips the column.
        assert!(!descriptor_declares_encryption(
            &value!({ "type": "string", "encrypted": true })
        ));
    }

    /// THE FENCE REFUSES IN A BUILD THAT HAS NO `test-helpers`.
    ///
    /// This module is `#[cfg(test)]`, so it compiles under `cargo test -p
    /// zeroship-data-orm --lib` with DEFAULT features - where this crate's
    /// own `feature = "test-helpers"` is off, because nothing in the build
    /// turns it on (the `[dev-dependencies]` entries enable it on the three
    /// crates BELOW, never on this one). That is the configuration the
    /// pre-existing witness could not reach:
    /// `zeroship-data-v8/tests/mask_flip.rs` enables `test-helpers` in its
    /// test build, so the original proof that a downgrade is refused came from
    /// a build that does not ship.
    ///
    /// Both protections, because the fence reads them independently and a
    /// single-protection test cannot refute the collapsed-to-one-bit shape.
    ///
    /// **`cfg(not(feature))` and not a runtime assertion.** The configuration
    /// is what this test IS, so it is spelled where the compiler enforces it: a
    /// `--all-features` run does not collect it at all, rather than collecting
    /// it and failing an assertion about its own build. `mask_flip.rs` binds the
    /// same refusal on a live database with the feature ON, so the two are
    /// complementary and neither configuration is left unwitnessed. Verify this
    /// one still runs with:
    ///   cargo test -p zeroship-data-orm --lib protection_floor
    #[cfg(not(feature = "test-helpers"))]
    #[test]
    fn a_downgrade_is_refused_without_the_test_helpers_feature() {
        let floor = floor_from_live(&live_with("ssn", masked_column()));
        // The descriptor still declares the field - it just dropped the `mask`
        // key. That one deletion is the whole defect this fence exists for.
        let downgraded = value!({ "ssn": { "type": "string" } });
        let err = refuse_offences(&floor, "people", &downgraded)
            .expect_err("a dropped mask must be refused, not written in the clear");
        let rendered = format!("{err:?}");
        assert!(
            rendered.contains("protection_removed_from_descriptor"),
            "the refusal must carry the typed code creators match on: {rendered}",
        );
        assert!(
            rendered.contains("ssn (masked)"),
            "the refusal must name the column and the protection: {rendered}",
        );

        let enc_floor = floor_from_live(&live_with("secret", encrypted_column()));
        let enc_err = refuse_offences(
            &enc_floor,
            "people",
            &value!({ "secret": { "type": "string" } }),
        )
        .expect_err("a dropped encryption block must be refused too");
        assert!(
            format!("{enc_err:?}").contains("secret (encrypted)"),
            "the encryption arm must name its own protection: {enc_err:?}",
        );

        // The control, differing in one variable: the SAME floor and the SAME
        // collection, with the protection still declared, is permitted. Without
        // it, a fence that refused everything would pass the two cases above.
        refuse_offences(
            &floor,
            "people",
            &value!({ "ssn": { "type": "string", "mask": { "kind": "last4" } } }),
        )
        .expect("a descriptor that still declares the mask must be permitted");
    }

    /// The floor is keyed by BINDING, so two deploys of one app do not share a
    /// resolved answer.
    #[test]
    fn the_floor_key_carries_the_deploy_token() {
        let pinned = DbBinding::new(
            "app_floor",
            "deploy_1",
            zeroship_data_sql::SchemaName::new("app_floor").unwrap(),
        );
        let current = DbBinding::new(
            "app_floor",
            "deploy_2",
            zeroship_data_sql::SchemaName::new("app_floor").unwrap(),
        );
        assert_ne!(
            floor_key(&pinned),
            floor_key(&current),
            "a migration that removes a protection arrives with a new deploy; \
             sharing the key would serve it the previous deploy's floor",
        );
    }
}
