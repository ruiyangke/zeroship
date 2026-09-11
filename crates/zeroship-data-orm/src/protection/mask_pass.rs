//! Derive masks and place protected values using runtime descriptor storage.
//!
//! Encryption retains mask inputs in a zeroizing sidechannel. Mask-only fields
//! read their input from the logical row value. Mask derivation does not mutate the
//! row; relocation runs after encryption and byte conversion, moving the completed
//! value to `storage.rawColumn` and leaving the mask in the visible value column.
//!
//! This ordering preserves ciphertext and binary values without recomputing them.
//! Only named mask strategies are accepted; creator callbacks cannot define a
//! transform that exposes plaintext.

use std::collections::HashMap;

use zeroize::Zeroizing;
use zeroship_data_sql::value::Value;

use crate::catalog::MaskKind;
use zeroship_data_orm::error::DbError;
// The per-kind transform itself lives in the domain tier: `read_set` lowers a
// filter operand on a masked column through it, and `read_set` is below this
// module. See `zeroship_data_orm::masking` for why it could not stay here.
use zeroship_data_orm::masking::apply_mask_kind;

pub type MaskPlaintextSidechannel = HashMap<String, Zeroizing<String>>;

/// One masked field's derived mask, with the physical column its REAL value
/// belongs in.
///
/// The raw column's name travels with the mask rather than being re-derived at
/// placement time, because it is the DESCRIPTOR that names it
/// (`zeroship_data_sql::compile::declared_raw_column`) and
/// [`relocate_masked_columns`] does not hold the descriptor - it holds this.
/// [`apply_mask_on_write`], which does hold it, is where the resolution and its
/// fence belong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedMask {
    /// The LOGICAL field name. Its own column receives [`Self::masked`].
    pub field: String,
    /// The physical column that receives the field's real value.
    pub raw_column: String,
    /// The mask, as the field's own column will store it.
    pub masked: String,
}

/// The masks [`apply_mask_on_write`] derived, in declared-field order.
///
/// Held rather than written straight into the row so that exactly one stage -
/// [`relocate_masked_columns`] - owns physical placement. See the module doc.
pub type DerivedMasks = Vec<DerivedMask>;

/// Derive the masked representation of every masked column on `row`.
///
/// Walks every column on the schema; when the column carries a
/// `mask = { kind: <kind>, classification: <class> }` entry AND
/// `kind != "none"`, computes the mask from the plaintext and returns it under
/// the LOGICAL field name. **Does not mutate `row`** - see
/// [`relocate_masked_columns`].
///
/// Plaintext source order:
/// 1. If `plaintexts[col]` is populated (encrypted-column case, the
///    encryption pass deposited the raw plaintext before swapping in
///    the ciphertext), use it.
/// 2. Otherwise, read `row[col]` directly (non-encrypted-but-masked
///    case — `t.string().mask({...})`).
/// 3. If the column is absent from `row` (partial UPDATE), skip — nothing
///    changed, so nothing is relocated and the stored mask stays in sync.
/// 4. If the column value is `null`, skip — `null` passes through as
///    `null` (no mask written, per Q-MASK-L).
pub fn apply_mask_on_write(
    schema: &Value,
    plaintexts: &MaskPlaintextSidechannel,
    row: &Value,
) -> Result<DerivedMasks, DbError> {
    let Some(schema_obj) = schema.as_object() else {
        return Ok(Vec::new());
    };
    let Some(obj) = row.as_object() else {
        return Err(DbError::internal(
            "apply_mask_on_write: row must be a JSON object",
        ));
    };

    let mut derived: DerivedMasks = Vec::new();

    for (col, def) in schema_obj.iter() {
        let Some(mask) = zeroship_data_sql::descriptors::effective_mask(def) else {
            continue;
        };
        let kind = MaskKind::from_sql(mask.kind).ok_or_else(|| {
            DbError::internal(format!("apply_mask_on_write: unknown mask kind '{}'", mask.kind))
        })?;

        // Resolved BEFORE the plaintext branch, so a descriptor that names a
        // creator-reachable raw column refuses every write to the collection
        // rather than only the ones that mention the column. Same reasoning as
        // `protection::protection_floor::refuse_protection_downgrade`: a write that
        // happens to omit `ssn` is harmless in itself, but letting it through
        // means the broken deploy appears to work until the first write that
        // does mention it.
        let Some(raw_column) = crate::compile::declared_raw_column(col, def)? else {
            // Unreachable: `declared_raw_column` returns `None` only for a
            // field with no effective mask. Both callers use the shared
            // descriptor predicate; keep this branch fallible at the boundary.
            continue;
        };

        let plaintext: Option<Zeroizing<String>> = if let Some(pt) = plaintexts.get(col) {
            Some(pt.clone())
        } else if let Some(value) = obj.get(col) {
            if value.is_null() {
                // null → no sibling write (Q-MASK-L)
                None
            } else if let Some(s) = value.as_str() {
                Some(Zeroizing::new(s.to_string()))
            } else if let Some(n) = value.as_i64() {
                Some(Zeroizing::new(n.to_string()))
            } else if let Some(f) = value.as_f64() {
                Some(Zeroizing::new(f.to_string()))
            } else {
                return Err(DbError::internal(format!(
                    "apply_mask_on_write: column '{col}': cannot serialize value for mask: {value:?}"
                )));
            }
        } else {
            // Column absent from row — partial UPDATE. Skip.
            None
        };

        if let Some(pt) = plaintext {
            derived.push(DerivedMask {
                field: col.clone(),
                raw_column,
                masked: apply_mask_kind(kind, pt.as_str()),
            });
        }
    }

    Ok(derived)
}

/// Move each masked field's real value to its raw column and put the mask in
/// the field's own column.
///
/// The ONE stage that owns physical placement, and the last one to run. For
/// every `(field, mask)` in `masks`:
///
/// 1. `row[storage.rawColumn] = row[<field>]` (the real value, whatever stage
///    produced it - plaintext or native ciphertext);
/// 2. `row[<field>] = mask`;
///
/// Step 1 MOVES rather than recomputes. That is what makes the
/// documented contract violation in `apply_mask_on_write` - the encryption pass
/// ran and the sidechannel was not populated, so the value in hand is
/// ciphertext - survivable: the ciphertext lands in the raw column intact and
/// only the mask is wrong. Recomputing the raw value here, or making the move
/// conditional on the sidechannel being populated, would overwrite the
/// ciphertext with a mask of itself on a write that returns success.
///
/// A field absent from `masks` is untouched: a partial UPDATE that does not
/// mention `ssn` neither relocates nor re-masks it.
pub fn relocate_masked_columns(masks: &DerivedMasks, row: &mut Value) -> Result<(), DbError> {
    if masks.is_empty() {
        return Ok(());
    }
    let Some(obj) = row.as_object_mut() else {
        return Err(DbError::internal(
            "relocate_masked_columns: row must be a JSON object",
        ));
    };
    for DerivedMask {
        field,
        raw_column,
        masked,
    } in masks
    {
        if let Some(value) = obj.shift_remove(field.as_str()) {
            obj.insert(raw_column.clone(), value);
        }
        obj.insert(field.clone(), Value::String(masked.clone()));
    }
    Ok(())
}

// =====================================================================
// Read-side flip: wrap masked columns in MaskedValueRepr
// =====================================================================

/// Wrap each masked column on `row` in a
/// `MaskedValueRepr` so the JS-side SDK can construct `MaskedValue<T>`
/// from the wire payload.
///
/// Called AFTER the SELECT (or RETURNING) materialises rows, BEFORE
/// the row crosses back to V8.
///
/// One row shape, two sources of it:
///
/// - a **SELECT or RETURNING** projects only logical names, so `row[col]`
///   already holds the masked string and no raw key is present. That was true
///   of SELECT alone until the write builders stopped starring: a
///   `RETURNING *` write returned every physical column, so the row also
///   carried `__zs_raw__<col>` with the real value;
/// - the **WAL consumer**, which decodes pgoutput with no schema in reach and
///   no projection to apply, and therefore still produces the second shape.
///
/// Both are handled by the same two steps: re-apply the mask transform to
/// `row[col]`, and remove the raw key if it is there.
///
/// The re-application is not redundant. `row[col]` is the mask on every
/// correct path, and re-masking a mask is a no-op for every built-in kind
/// (pinned by `zeroship_data_orm::masking`'s
/// `remasking_a_mask_is_a_no_op_for_every_kind`, which travelled with the
/// transform). What it buys is
/// that a builder that somehow lowered a masked field to its raw column - the
/// class of bug the flip exists to make impossible, not a class that is
/// impossible to reintroduce - is masked here rather than returned.
///
/// The wire shape mirrors the SDK's `MaskedValueRepr` (sdks/db/src/
/// types.ts): a `sentinel: "__zsmask__"` discriminator plus `masked`
/// (the user-facing string) and `classification` (drives unmask
/// authorization). Per-row metadata (`{collection, row_pk,
/// column}`) rides on a `_meta` key so `.unmask()` can route
/// the round-trip back to the right row.
///
/// **Opt-out** (`mask: { kind: "none" }`): columns explicitly opted
/// out of masking are skipped — they retain whatever value the SELECT
/// produced (typically plaintext via the decrypt-on-read path).
///
/// Returns `Ok(())` when the schema declares no masked columns or the
/// row is missing fields; never errors on a malformed row.
pub fn wrap_row_on_read(schema: &Value, collection: &str, row: &mut Value) -> Result<(), DbError> {
    let Some(schema_obj) = schema.as_object() else {
        return Ok(());
    };
    let Some(obj) = row.as_object_mut() else {
        return Ok(());
    };

    // The unmask round-trip needs the row's PK to identify which
    // row to fetch plaintext for. We pluck it once up-front; rows that
    // didn't surface an `id` (composite-PK collections, or rows that
    // came back via a projection without `id`) get the empty string —
    // `unmask()` will reject those with a typed error.
    let row_pk = obj
        .get("id")
        .map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        })
        .unwrap_or_default();

    // Collect replacements first so we don't hold a mutable borrow on
    // `obj` while iterating the schema.
    let mut to_wrap: Vec<(String, String, String)> = Vec::new(); // (col, masked_value, classification)
    let mut to_strip: Vec<String> = Vec::new();

    for (col, def) in schema_obj.iter() {
        let Some(mask) = zeroship_data_sql::descriptors::effective_mask(def) else {
            continue;
        };
        let classification = mask.classification.to_string();
        let kind = MaskKind::from_sql(mask.kind).unwrap_or(MaskKind::Full);

        // The field's own slot holds the mask. Re-apply the transform rather
        // than trusting it: we cannot distinguish "already masked" from "a
        // builder lowered this to the raw column" by looking at the value, and
        // re-masking a mask is a no-op for every built-in kind.
        let masked_value: Option<String> = obj
            .get(col)
            .and_then(|v| v.as_str())
            .map(|s| apply_mask_kind(kind, s));

        // The raw column rides out of a WAL-decoded row (and out of every
        // `RETURNING *`, back when the write builders starred). Strip it here -
        // `read_pipeline`'s surface stage would too, but this pass runs first
        // and the sentinel it writes must not sit beside the value it hides.
        //
        // The name comes from the descriptor, not from a `format!` here: see
        // `zeroship_data_sql::compile::declared_raw_column`, which also refuses a
        // descriptor naming a column creator code could reach. Propagated
        // rather than swallowed - falling back to the derivation on a refusal
        // would leave the pass reading a column the write pass refused to
        // write, and report success.
        if let Some(raw_key) = crate::compile::declared_raw_column(col, def)? {
            if obj.contains_key(&raw_key) {
                to_strip.push(raw_key);
            }
        }

        let Some(masked) = masked_value else {
            // The field's column is absent (e.g. a narrowed projection) or is
            // not a string (NULL) — nothing to wrap.
            continue;
        };

        to_wrap.push((col.clone(), masked, classification));
    }

    for stripped in to_strip {
        obj.shift_remove(&stripped);
    }

    for (col, masked, classification) in to_wrap {
        let repr = zeroship_data_sql::value!({
            "sentinel": "__zsmask__",
            // DB-7: an unforgeable per-process signature. Only sentinels the
            // read pipeline itself produced carry it; the decoder refuses to
            // mint a MaskedValue from any sentinel lacking it, so app JS cannot
            // fabricate a `__zsmask__` object (e.g. stashed in a JSONB column it
            // controls) and have it minted into a MaskedValue pointing at an
            // attacker-chosen (collection, row, column).
            "_sig": mask_sentinel_signature(),
            "masked": masked,
            "classification": classification,
            "_meta": {
                "collection": collection,
                "row_pk": row_pk,
                "column": col,
            },
        });
        obj.insert(col, repr);
    }
    Ok(())
}

/// DB-7: per-process secret stamped into every pipeline-minted mask sentinel
/// (`_sig`) and verified at rehydration. App JS cannot read it — the rehydrator
/// consumes the raw sentinel into a `MaskedValue` (whose internal fields do not
/// expose `_sig`) before any handler sees the row, and the value is never
/// serialized back to JS. Generated once per process from the OS RNG.
pub fn mask_sentinel_signature() -> &'static str {
    use std::sync::OnceLock;
    static SIG: OnceLock<String> = OnceLock::new();
    SIG.get_or_init(|| {
        use aes_gcm::{AeadCore, Aes256Gcm, aead::OsRng};
        // Two 12-byte GCM nonces → 24 bytes of OS entropy, hex-encoded.
        let a = Aes256Gcm::generate_nonce(&mut OsRng);
        let b = Aes256Gcm::generate_nonce(&mut OsRng);
        a.iter()
            .chain(b.iter())
            .map(|x| format!("{x:02x}"))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_data_sql::value;

    #[test]
    fn plaintext_sidechannel_stores_zeroizing_strings() {
        let mut plaintexts = MaskPlaintextSidechannel::new();
        plaintexts.insert("ssn".to_string(), Zeroizing::new("123-45-6789".to_string()));
        let got: &Zeroizing<String> = plaintexts.get("ssn").expect("sidechannel entry");
        assert_eq!(got.as_str(), "123-45-6789");
    }

    // -----------------------------------------------------------------
    // apply_mask_on_write: integration with row + sidechannel
    // -----------------------------------------------------------------

    /// Run the two write stages in the order `WriteStages::apply_to_doc` does:
    /// derive the masks, then place them.
    fn derive_and_relocate(schema: &Value, plaintexts: &MaskPlaintextSidechannel, row: &mut Value) {
        let masks = apply_mask_on_write(schema, plaintexts, row).expect("derive masks");
        relocate_masked_columns(&masks, row).expect("relocate");
    }

    #[test]
    fn a_masked_encrypted_write_puts_the_mask_in_the_logical_column_and_moves_the_ciphertext() {
        // Encrypted column: plaintext arrives via the sidechannel.
        let schema = value!({
            "ssn": {
                "type": "string",
                "encrypted": true,
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let mut row = value!({
            "id": "usr_01",
            "ssn": "BASE64CIPHERTEXT",
        });
        let mut plaintexts = MaskPlaintextSidechannel::new();
        plaintexts.insert("ssn".to_string(), Zeroizing::new("123-45-6789".to_string()));

        derive_and_relocate(&schema, &plaintexts, &mut row);

        let raw = crate::compile::raw_column_name("ssn");
        let obj = row.as_object().unwrap();
        assert_eq!(obj.get("ssn").and_then(|v| v.as_str()), Some("***-**-6789"));
        assert_eq!(
            obj.get(&raw).and_then(|v| v.as_str()),
            Some("BASE64CIPHERTEXT"),
            "the authoritative value moves to the raw column",
        );
        // The binary-bind marker travels with the value: it is the RAW column
        // that wants `decode($N,'base64')::bytea`; the masked column is TEXT.
        assert!(
            obj.get("__zsbin__ssn").is_none(),
            "marker must not stay behind: {row}"
        );
        assert!(!obj.contains_key(&format!("__zsbin__{raw}")));
    }

    /// The documented contract violation: the encryption pass ran but the
    /// sidechannel was not populated, so the only value in hand is ciphertext.
    ///
    /// The mask is then garbage (a mask of the ciphertext), which is ugly. What
    /// must NOT happen is losing the ciphertext, and that is why the relocation
    /// MOVES the slot rather than recomputing it or conditioning the move on
    /// the sidechannel.
    #[test]
    fn a_missing_sidechannel_still_preserves_the_ciphertext() {
        let schema = value!({
            "ssn": {
                "type": "string",
                "encrypted": true,
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let mut row = value!({ "id": "usr_01", "ssn": "BASE64CIPHERTEXT" });
        let plaintexts = MaskPlaintextSidechannel::new();

        derive_and_relocate(&schema, &plaintexts, &mut row);

        assert_eq!(
            row[crate::compile::raw_column_name("ssn")].as_str(),
            Some("BASE64CIPHERTEXT"),
            "the ciphertext must survive a write that returns success: {row}",
        );
    }

    #[test]
    fn a_mask_only_write_moves_the_plaintext_to_the_raw_column() {
        // Non-encrypted but masked column: plaintext stays in `row[col]`,
        // sidechannel has no entry.
        let schema = value!({
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let mut row = value!({ "id": "usr_01", "email": "alice@example.com" });
        let plaintexts = MaskPlaintextSidechannel::new();

        derive_and_relocate(&schema, &plaintexts, &mut row);

        let obj = row.as_object().unwrap();
        assert_eq!(
            obj.get("email").and_then(|v| v.as_str()),
            Some("a***@example.com"),
            "the field's own column holds the mask",
        );
        assert_eq!(
            obj.get(&crate::compile::raw_column_name("email"))
                .and_then(|v| v.as_str()),
            Some("alice@example.com"),
        );
    }

    #[test]
    fn apply_mask_on_write_skips_kind_none() {
        // Explicit opt-out: no sibling written.
        let schema = value!({
            "ssn": {
                "type": "string",
                "encrypted": true,
                "mask": { "kind": "none", "classification": "spi" }
            }
        });
        let mut row = value!({ "id": "usr_01", "ssn": "BASE64CT" });
        let mut plaintexts = MaskPlaintextSidechannel::new();
        plaintexts.insert("ssn".to_string(), Zeroizing::new("123-45-6789".to_string()));

        derive_and_relocate(&schema, &plaintexts, &mut row);

        let obj = row.as_object().unwrap();
        assert!(
            obj.get("ssn_masked").is_none(),
            "kind=none must NOT emit a sibling: {row}"
        );
    }

    #[test]
    fn apply_mask_on_write_skips_null_value() {
        // null passes through as null (Q-MASK-L); no sibling write.
        let schema = value!({
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let mut row = value!({ "id": "usr_01", "email": null });
        let plaintexts = MaskPlaintextSidechannel::new();

        derive_and_relocate(&schema, &plaintexts, &mut row);

        let obj = row.as_object().unwrap();
        assert!(
            obj.get("email_masked").is_none(),
            "null parent must not emit a sibling"
        );
    }

    #[test]
    fn apply_mask_on_write_skips_absent_column() {
        // Partial UPDATE: parent column not on the row at all → no
        // sibling write (the existing row's masked value stays in sync
        // because the plaintext didn't change).
        let schema = value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "name": { "type": "string" }
        });
        let mut row = value!({ "name": "alice" });
        let plaintexts = MaskPlaintextSidechannel::new();

        derive_and_relocate(&schema, &plaintexts, &mut row);

        let obj = row.as_object().unwrap();
        assert!(obj.get("ssn_masked").is_none());
    }

    #[test]
    fn apply_mask_on_write_handles_multiple_columns() {
        let schema = value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            },
            "dob": {
                "type": "string",
                "mask": { "kind": "dateYear", "classification": "pii" }
            }
        });
        let mut row = value!({
            "id": "usr_01",
            "ssn": "123-45-6789",
            "email": "bob@example.com",
            "dob": "1985-04-12"
        });
        let plaintexts = MaskPlaintextSidechannel::new();

        derive_and_relocate(&schema, &plaintexts, &mut row);

        let obj = row.as_object().unwrap();
        for (field, mask, plaintext) in [
            ("ssn", "***-**-6789", "123-45-6789"),
            ("email", "b***@example.com", "bob@example.com"),
            ("dob", "1985-**-**", "1985-04-12"),
        ] {
            assert_eq!(obj.get(field).and_then(|v| v.as_str()), Some(mask));
            assert_eq!(
                obj.get(&crate::compile::raw_column_name(field))
                    .and_then(|v| v.as_str()),
                Some(plaintext),
            );
        }
    }

    #[test]
    fn apply_mask_on_write_rejects_unknown_kind() {
        let schema = value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "absurdly-novel-kind", "classification": "spi" }
            }
        });
        let row = value!({ "ssn": "abc" });
        let plaintexts = MaskPlaintextSidechannel::new();

        let err = apply_mask_on_write(&schema, &plaintexts, &row).unwrap_err();
        match err {
            DbError::Internal { .. } => {}
            other => panic!("expected DbError::Internal, got {other:?}"),
        }
    }

    #[test]
    fn apply_mask_on_write_noop_when_no_masked_columns() {
        // Schema with only non-masked fields — pass is a no-op.
        let schema = value!({
            "name": { "type": "string" },
            "age": { "type": "number" }
        });
        let mut row = value!({ "name": "alice", "age": 30 });
        let plaintexts = MaskPlaintextSidechannel::new();
        let original = row.clone();

        derive_and_relocate(&schema, &plaintexts, &mut row);

        assert_eq!(row, original);
    }

    // -----------------------------------------------------------------
    // wrap_row_on_read: read-side flip
    // -----------------------------------------------------------------

    #[test]
    fn wrap_row_on_read_aliased_select_shape() {
        // SELECT "ssn_masked" AS "ssn", ... — the parent slot already
        // contains the masked string; no sibling key is present.
        let schema = value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "name": { "type": "string" }
        });
        let mut row = value!({
            "id": "usr_01",
            "ssn": "***-**-6789",
            "name": "alice"
        });

        wrap_row_on_read(&schema, "users", &mut row).unwrap();

        let obj = row.as_object().unwrap();
        let ssn = obj.get("ssn").and_then(|v| v.as_object()).unwrap();
        assert_eq!(
            ssn.get("sentinel").and_then(|v| v.as_str()),
            Some("__zsmask__")
        );
        assert_eq!(
            ssn.get("masked").and_then(|v| v.as_str()),
            Some("***-**-6789")
        );
        assert_eq!(
            ssn.get("classification").and_then(|v| v.as_str()),
            Some("spi")
        );
        let meta = ssn.get("_meta").and_then(|v| v.as_object()).unwrap();
        assert_eq!(
            meta.get("collection").and_then(|v| v.as_str()),
            Some("users")
        );
        assert_eq!(meta.get("row_pk").and_then(|v| v.as_str()), Some("usr_01"));
        assert_eq!(meta.get("column").and_then(|v| v.as_str()), Some("ssn"));
        // Non-masked column untouched.
        assert_eq!(obj.get("name").and_then(|v| v.as_str()), Some("alice"));
    }

    #[test]
    fn wrap_row_on_read_strips_the_raw_column_from_a_physical_row() {
        // A row carrying every physical column: the mask under `ssn` AND the
        // real value under the raw column. The wrap must return the mask and
        // remove the raw key.
        //
        // Named for the SHAPE, not for a producer. It was
        // `..._from_a_returning_star_row` while the write builders starred;
        // they project now, and the surviving producer of this shape is the WAL
        // consumer. The fixture is hand-built either way, so what the test
        // exercises never depended on which producer made the row - only the
        // name did.
        let schema = value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let raw = crate::compile::raw_column_name("ssn");
        let mut row = value!({
            "id": "usr_01",
            "ssn": "***-**-6789",
        });
        row[raw.clone()] = value!("123-45-6789");

        wrap_row_on_read(&schema, "users", &mut row).unwrap();

        let obj = row.as_object().unwrap();
        assert!(
            obj.get(&raw).is_none(),
            "the raw column must not survive to the JS boundary: {row}",
        );
        assert!(
            !serde_json::to_string(&row).unwrap().contains("123-45-6789"),
            "and neither must its value: {row}",
        );
        let ssn = obj.get("ssn").and_then(|v| v.as_object()).unwrap();
        assert_eq!(
            ssn.get("masked").and_then(|v| v.as_str()),
            Some("***-**-6789")
        );
    }

    #[test]
    fn wrap_row_on_read_skips_kind_none() {
        // Opt-out: `kind: "none"` retains plaintext-on-read (the
        // decrypt-on-read path); no wrapping happens.
        let schema = value!({
            "ssn": {
                "type": "string",
                "encrypted": true,
                "mask": { "kind": "none", "classification": "spi" }
            }
        });
        let mut row = value!({
            "id": "usr_01",
            "ssn": "decrypted-plaintext"
        });

        wrap_row_on_read(&schema, "users", &mut row).unwrap();

        // Parent slot unchanged: still a bare string.
        assert_eq!(
            row.get("ssn").and_then(|v| v.as_str()),
            Some("decrypted-plaintext"),
            "kind=none must NOT wrap: {row}"
        );
    }

    #[test]
    fn wrap_row_on_read_uses_default_pii_classification() {
        // When the schema mask block omits `classification`, default is
        // `"pii"` (mirrors the SDK's default).
        let schema = value!({
            "email": {
                "type": "string",
                "mask": { "kind": "email" }
            }
        });
        let mut row = value!({
            "id": "usr_01",
            "email": "a***@example.com"
        });

        wrap_row_on_read(&schema, "users", &mut row).unwrap();

        let email = row.get("email").and_then(|v| v.as_object()).unwrap();
        assert_eq!(
            email.get("classification").and_then(|v| v.as_str()),
            Some("pii")
        );
    }

    #[test]
    fn wrap_row_on_read_handles_numeric_id() {
        // typed_id collections use string `id`, but legacy collections
        // can carry numeric PK — `row_pk` must stringify either.
        let schema = value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let mut row = value!({
            "id": 42,
            "ssn": "***-**-6789"
        });

        wrap_row_on_read(&schema, "users", &mut row).unwrap();

        let ssn = row.get("ssn").and_then(|v| v.as_object()).unwrap();
        let meta = ssn.get("_meta").and_then(|v| v.as_object()).unwrap();
        assert_eq!(meta.get("row_pk").and_then(|v| v.as_str()), Some("42"));
    }

    #[test]
    fn wrap_row_on_read_handles_missing_id() {
        // Projection that excluded `id` — `row_pk` falls back to empty
        // string; the wrap still happens (`unmask()` will surface a typed
        // error when row_pk is empty).
        let schema = value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let mut row = value!({ "ssn": "***-**-6789" });

        wrap_row_on_read(&schema, "users", &mut row).unwrap();

        let ssn = row.get("ssn").and_then(|v| v.as_object()).unwrap();
        let meta = ssn.get("_meta").and_then(|v| v.as_object()).unwrap();
        assert_eq!(meta.get("row_pk").and_then(|v| v.as_str()), Some(""));
        assert_eq!(
            meta.get("collection").and_then(|v| v.as_str()),
            Some("users")
        );
    }

    #[test]
    fn sec4_wrap_row_on_read_never_returns_parent_plaintext_when_no_sibling() {
        // SEC-4: an aggregate that grouped on a masked column WITHOUT
        // substituting the sibling lands here with the parent slot
        // holding PLAINTEXT and no `<col>_masked` sibling present. The
        // old code wrapped the parent value verbatim — i.e. it surfaced
        // plaintext to JS as if it were the masked display string. The
        // wrap must NOT trust the parent slot as already-masked: it must
        // either re-mask or refuse, never emit the raw value.
        let schema = value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        // Parent holds plaintext; no sibling — the dangerous shape.
        let mut row = value!({ "id": "usr_01", "ssn": "123-45-6789" });

        wrap_row_on_read(&schema, "users", &mut row).unwrap();

        // Whatever shape the parent slot now carries, it must not be the
        // raw plaintext string.
        let surfaced = row.get("ssn").cloned().unwrap_or(Value::Null);
        if let Some(s) = surfaced.as_str() {
            assert_ne!(
                s, "123-45-6789",
                "SEC-4: wrap_row_on_read must never surface the parent \
                 plaintext verbatim as if already masked: {row}"
            );
        }
        // If it did wrap into a sentinel, the masked payload must be the
        // re-masked value, not plaintext.
        if let Some(obj) = surfaced.as_object() {
            assert_eq!(
                obj.get("masked").and_then(Value::as_str),
                Some("***-**-6789"),
                "SEC-4: a parent-only masked column must be re-masked, not \
                 echoed as plaintext: {row}"
            );
        }
    }

    #[test]
    fn wrap_row_on_read_noop_when_no_masked_columns() {
        // Schema with only non-masked fields — row passes through
        // unchanged.
        let schema = value!({
            "name": { "type": "string" },
            "age": { "type": "number" }
        });
        let mut row = value!({ "id": "usr_01", "name": "alice", "age": 30 });
        let original = row.clone();

        wrap_row_on_read(&schema, "users", &mut row).unwrap();

        assert_eq!(row, original);
    }

    #[test]
    fn wrap_row_on_read_noop_when_row_not_object() {
        // Defensive: a `Value::Null` row passes through without error.
        let schema = value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let mut row = Value::Null;
        wrap_row_on_read(&schema, "users", &mut row).unwrap();
        assert_eq!(row, Value::Null);
    }

    #[test]
    fn wrap_row_on_read_handles_multiple_masked_columns() {
        let schema = value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            },
            "dob": {
                "type": "string",
                "mask": { "kind": "dateYear", "classification": "pii" }
            }
        });
        // Aliased-SELECT shape: parent slots hold masked strings.
        let mut row = value!({
            "id": "usr_01",
            "ssn": "***-**-6789",
            "email": "b***@example.com",
            "dob": "1985-**-**"
        });

        wrap_row_on_read(&schema, "users", &mut row).unwrap();

        for (col, classification) in [("ssn", "spi"), ("email", "pii"), ("dob", "pii")] {
            let wrapped = row.get(col).and_then(|v| v.as_object()).unwrap();
            assert_eq!(
                wrapped.get("sentinel").and_then(|v| v.as_str()),
                Some("__zsmask__"),
                "col {col}"
            );
            assert_eq!(
                wrapped.get("classification").and_then(|v| v.as_str()),
                Some(classification),
                "col {col}"
            );
        }
    }

    // -----------------------------------------------------------------
    // The raw column's name comes from the DESCRIPTOR, not from a `format!`
    // -----------------------------------------------------------------

    /// A masked field def as the migration fold emits it, with the raw column
    /// spelled by the emitter that wrote the DDL.
    ///
    /// The fixtures below declare a name `zeroship_data_sql::compile::raw_column_name`
    /// does NOT produce. That is deliberate: a fixture spelling the derived name
    /// passes against a body that ignores the descriptor entirely, which is the
    /// state this pair of tests exists to move off.
    fn masked_def_with_raw(raw: &str) -> Value {
        value!({
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" },
            "storage": { "valueColumn": "ssn", "rawColumn": raw },
        })
    }

    #[test]
    fn a_write_relocates_to_the_raw_column_the_descriptor_declares() {
        let schema = value!({ "ssn": masked_def_with_raw("__zs_raw2__ssn") });
        let mut row = value!({ "id": "usr_01", "ssn": "123-45-6789" });

        derive_and_relocate(&schema, &MaskPlaintextSidechannel::new(), &mut row);

        let obj = row.as_object().unwrap();
        assert_eq!(
            obj.get("__zs_raw2__ssn").and_then(|v| v.as_str()),
            Some("123-45-6789"),
            "the real value belongs in the column the descriptor names: {row}",
        );
        assert!(
            obj.get(&crate::compile::raw_column_name("ssn")).is_none(),
            "nothing may be written to a column the descriptor did not name: {row}",
        );
        assert_eq!(obj.get("ssn").and_then(|v| v.as_str()), Some("***-**-6789"));
        // The binary-bind marker follows the value to whichever column holds
        // it, so it has to be renamed off the DECLARED name too.
        assert!(!obj.contains_key("__zsbin____zs_raw2__ssn"));
        assert!(obj.get("__zsbin__ssn").is_none());
    }

    #[test]
    fn wrap_row_on_read_strips_the_raw_column_the_descriptor_declares() {
        let schema = value!({ "ssn": masked_def_with_raw("__zs_raw2__ssn") });
        let mut row = value!({
            "id": "usr_01",
            "ssn": "***-**-6789",
            "__zs_raw2__ssn": "123-45-6789",
        });

        wrap_row_on_read(&schema, "users", &mut row).unwrap();

        assert!(
            !serde_json::to_string(&row).unwrap().contains("123-45-6789"),
            "the declared raw column's value must not survive to the JS boundary: {row}",
        );
    }

    /// The fence, at both pass boundaries.
    ///
    /// `zeroship_data_sql::compile::declared_raw_column` owns the verdict; these two
    /// arms prove each pass PROPAGATES it rather than falling back to the
    /// derivation, which would place plaintext in a filterable column while
    /// reporting success. `protection::protection_floor` does not catch this shape -
    /// the descriptor still declares the mask, so its presence comparison is
    /// satisfied.
    #[test]
    fn a_descriptor_naming_a_creator_reachable_raw_column_refuses_both_passes() {
        let schema = value!({ "ssn": masked_def_with_raw("nickname") });

        let row = value!({ "id": "usr_01", "ssn": "123-45-6789" });
        let err = apply_mask_on_write(&schema, &MaskPlaintextSidechannel::new(), &row)
            .expect_err("the write pass must refuse a reachable raw column");
        assert!(
            format!("{err:?}").contains("nickname"),
            "the refusal must name the offending column; got {err:?}",
        );

        let mut read_row = value!({ "id": "usr_01", "ssn": "***-**-6789" });
        wrap_row_on_read(&schema, "users", &mut read_row)
            .expect_err("the read pass must refuse a reachable raw column");
    }

    /// The control every arm above needs: a descriptor with NO `storage` block
    /// still relocates and strips, under the derived name. Without it, a body
    /// that refused every masked field would satisfy the fence arm and look
    /// green on a tree where masking no longer worked at all.
    #[test]
    fn a_descriptor_without_a_storage_block_still_uses_the_derived_name() {
        let schema = value!({
            "ssn": { "type": "string", "mask": { "kind": "last4", "classification": "spi" } }
        });
        let raw = crate::compile::raw_column_name("ssn");
        let mut row = value!({ "id": "usr_01", "ssn": "123-45-6789" });

        derive_and_relocate(&schema, &MaskPlaintextSidechannel::new(), &mut row);
        assert_eq!(row[raw.as_str()].as_str(), Some("123-45-6789"));

        wrap_row_on_read(&schema, "users", &mut row).unwrap();
        assert!(
            row.get(&raw).is_none(),
            "the derived raw column is still stripped on read: {row}",
        );
    }
}
