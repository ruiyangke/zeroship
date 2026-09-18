//! Enforce catalog protections as a minimum for the runtime descriptor.
//!
//! Migration-written sentinels record whether a column is masked or encrypted.
//! A creator-authored descriptor may add protection but cannot remove protection
//! recorded by the catalog. A disagreement refuses the operation.
//!
//! This check compares protection presence, not mask strategy or physical column
//! placement, and needs no encryption key. Catalog results are cached by app and
//! deploy so a new deployment refreshes the protection floor.

use std::collections::HashMap;
use std::rc::Rc;

use crate::schema::{ColumnSchema, FieldMap};

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

fn floor_key(binding: &DbBinding) -> DbBinding {
    binding.clone()
}

/// Reduce a `LiveSchema` to the protected columns alone.
fn floor_from_live(live: &crate::sql::catalog::LiveSchema) -> ProtectionFloor {
    let mut out: ProtectionFloor = HashMap::new();
    for (table, columns) in &live.tables {
        let protected: HashMap<String, StoredProtection> = columns
            .iter()
            .filter_map(|(column, info)| {
                let stored = StoredProtection {
                    masked: info.mask.is_some(),
                    encrypted: info.encrypted,
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
pub(crate) fn descriptor_declares_mask(def: &ColumnSchema) -> bool {
    crate::sql::descriptors::effective_mask(def).is_some()
}

/// Whether the descriptor enables encryption, matching the write pipeline.
pub(crate) fn descriptor_declares_encryption(def: &ColumnSchema) -> bool {
    crate::sql::descriptors::is_encrypted(def)
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
    // Keep catalog work out of the hot write future's stack allocation.
    let live = Box::pin(crate::exec::read_catalog(route)).await?;
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
    schema: &FieldMap,
) -> Result<(), DbError> {
    let floor = resolve_floor(route, binding).await?;
    refuse_offences(&floor, collection, schema)
}

/// Evaluate the protection floor independently of catalog I/O.
fn refuse_offences(
    floor: &ProtectionFloor,
    collection: &str,
    schema: &FieldMap,
) -> Result<(), DbError> {
    let Some(stored_columns) = floor.get(collection) else {
        return Ok(());
    };

    // Deterministic order: a refusal must name the same column on every run, or
    // the failure a creator sees depends on hash iteration.
    let mut offences: Vec<(&String, &'static str)> = Vec::new();
    for (column, stored) in stored_columns {
        let def = schema.get(column.as_str());
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
#[cfg(test)]
pub fn reset_for_tests() {
    crate::orm_context::current().floors_mut(HashMap::clear);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::catalog::{Classification, ColumnInfo, LiveSchema, MaskKind, MaskMeta};

    use crate::value;

    fn test_fields(fields: crate::value::Value) -> FieldMap {
        crate::schema::CollectionSchema::from_fields(&fields)
            .unwrap()
            .into_fields()
    }

    fn test_column(field: crate::value::Value) -> ColumnSchema {
        ColumnSchema::from_descriptor(&field).unwrap()
    }

    fn masked_column() -> ColumnInfo {
        ColumnInfo {
            mask: Some(MaskMeta {
                kind: MaskKind::Last4,
                classification: Classification::Pci,
                raw_column: crate::sql::mapping::raw_column_name("ssn"),
            }),
            ..Default::default()
        }
    }

    fn encrypted_column() -> ColumnInfo {
        ColumnInfo {
            encrypted: true,
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
    /// A column can be encrypted without being masked (`.encrypted()` with
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
        assert!(!descriptor_declares_mask(&test_column(
            value!({ "type": "string" })
        )));
        assert!(!descriptor_declares_mask(&test_column(
            value!({ "type": "string", "mask": { "kind": "none" } })
        )));
        assert!(descriptor_declares_mask(&test_column(
            value!({ "type": "string", "mask": { "kind": "last4" } })
        )));
        // No `kind` at all defaults to `full` in the write pass, so it IS a
        // declaration.
        assert!(descriptor_declares_mask(&test_column(
            value!({ "type": "string", "mask": { "classification": "pii" } })
        )));
    }

    #[test]
    fn encryption_is_declared_by_the_boolean_flag() {
        assert!(!descriptor_declares_encryption(&test_column(
            value!({ "type": "string" })
        )));
        assert!(descriptor_declares_encryption(&test_column(
            value!({ "type": "string", "encrypted": true })
        )));
        assert!(!descriptor_declares_encryption(&test_column(
            value!({ "type": "string", "encrypted": false })
        )));
    }

    /// Removing masking or encryption from a descriptor is always refused.
    #[test]
    fn a_protection_downgrade_is_refused() {
        let floor = floor_from_live(&live_with("ssn", masked_column()));
        // The descriptor still declares the field - it just dropped the `mask`
        // key. That one deletion is the whole defect this fence exists for.
        let downgraded = test_fields(value!({ "ssn": { "type": "string" } }));
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
            &test_fields(value!({ "secret": { "type": "string" } })),
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
            &test_fields(value!({ "ssn": { "type": "string", "mask": { "kind": "last4" } } })),
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
            crate::sql::SchemaName::new("app_floor").unwrap(),
        );
        let current = DbBinding::new(
            "app_floor",
            "deploy_2",
            crate::sql::SchemaName::new("app_floor").unwrap(),
        );
        assert_ne!(
            floor_key(&pinned),
            floor_key(&current),
            "a migration that removes a protection arrives with a new deploy; \
             sharing the key would serve it the previous deploy's floor",
        );
    }
}
