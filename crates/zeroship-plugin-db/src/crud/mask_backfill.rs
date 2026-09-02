//! Mask computation helpers for the migration engine's mask transitions.
//!
//! **The three runner entry points this doc used to describe -
//! `run_mask_backfill`, `run_mask_rewrite`, `run_mask_remove` - do not exist
//! in this module, and nothing outside it calls anything that does.** Grepped
//! 2026-08-28 across `crates/` and `sdks/`: `crud::mask_backfill` has zero
//! consumers. What survives here is the sentinel codec wrapper, the audit-name
//! builders, and two helpers marked "exposed for tests" that no test calls.
//!
//! It is left in place rather than deleted because that is a decision about
//! dead code, not about masking. What was NOT left in place is its vocabulary:
//! every derivation of a physical column name in here now agrees with the
//! storage flip (the field's own column holds the mask), because a second,
//! wrong derivation sitting in a module nothing calls is exactly the thing that
//! gets copied back into a live path.
//!
//! ## Audit-table integration
//!
//! Each batch run inserts an audit row in `__zeroship_migrations`
//! whose `name` is derived from the existing migrations-framework
//! shape (`mig:mask_backfill_<coll>_<col>` /
//! `mig:mask_rewrite_<coll>_<col>`). Progress (`cursor`, `processed`)
//! survives worker restart: a re-invoked backfill picks up from the
//! last `cursor` already written to the audit row. The audit row is
//! finalised `Applied` on the last batch, `Failed` on any
//! propagating error.
//!
//! ## Why this lives outside the existing `migrations.rs` driver
//!
//! `crate::migrations` is a JS-driven loop (`fetchBatch` /
//! `commitBatch` round-tripped through the SDK). The mask backfill must
//! run inside the deploy pipeline, before the V8 isolate hands
//! control back to user code — there is no JS loop available.
//! `mask_backfill` reuses the audit-table shape (`phase = backfill`,
//! `change_class = additive` / `compatible`, `change_kind =
//! mask_backfill` / `mask_rewrite`) so operators querying
//! `__zeroship_migrations` see a uniform history, but the driver is
//! native.
//!
//! ## Batch sizing
//!
//! Default `BATCH_SIZE = 1000` — matches the per-batch cap on the
//! JS-driven `Migration.fetchBatch`. Operators can override per
//! deploy by passing a different `BackfillOpts` (unused today — the
//! constant is exposed so a future caller can lift it).

use serde_json::Value;
use zeroize::Zeroizing;

use crate::crud::mask_pass::apply_mask_kind;
use crate::diff::{Classification, MaskKind};
use crate::encryption::KeyStore;
use zeroship_data_core::error::DbError;

/// Default batch size for backfill / rewrite loops. We use 1000 here
/// as a middle ground between
/// per-batch UPDATE round-trip overhead (smaller = more chatter) and
/// memory footprint per batch (larger = bigger Rust-side Vec<Value>
/// allocation).
pub const BATCH_SIZE: i64 = 1_000;

// The mask-sentinel CODEC (build/parse the
// `__zsmask:…` string) was relocated into the leaf crate
// `zeroship_schema::mask_codec`. It is a schema-shape concern (the contract
// the schema layer writes into DDL and the data plane reads back); the
// backfill *runner* below (`run_mask_backfill` / `run_mask_rewrite`) STAYS
// here in the data plane.
//
// `build_mask_sentinel` re-exports verbatim (it is pure). `parse_mask_sentinel`
// gets a thin wrapper that preserves the `Result<_, DbError>` shape the
// plugin-db callers expect: the leaf codec returns `MaskSentinelError`
// (it cannot name `DbError`), which the `From` impl in `crate::error` maps
// back to `DbError::internal(<same message>)` — byte-identical to the
// pre-extraction parser.
pub use zeroship_schema::mask_codec::build_mask_sentinel;

/// Parse a `__zsmask:kind=…,classification=…`
/// sentinel string back into a `(MaskKind, Classification)` pair.
///
/// Thin `DbError`-shaped wrapper over the relocated leaf codec
/// [`zeroship_schema::mask_codec::parse_mask_sentinel`]. Returns
/// `Err(DbError::Internal { … })` with the code-discriminator
/// `mask_sentinel_malformed` for any parse failure — unknown kind,
/// unknown classification, missing field, extra trailing junk — exactly
/// as before the codec was extracted. The caller surfaces the typed error
/// from the introspector with the column name appended so an operator
/// hand-debugging `pg_description` sees exactly which sibling is malformed.
pub fn parse_mask_sentinel(s: &str) -> Result<(MaskKind, Classification), DbError> {
    zeroship_schema::mask_codec::parse_mask_sentinel(s).map_err(DbError::from)
}

/// Compute the masked representation for one row's
/// parent column value, optionally decrypting `value` first when the
/// column is `t.encrypted(...)`-declared.
///
/// `value` is the parent column's value as surfaced by the backend's
/// text protocol — for PG, BYTEA-encrypted columns arrive as `\xHH…`
/// hex strings; plaintext columns arrive as plain strings. The
/// decrypt path mirrors
/// [`crate::crud::encryption_pass::decrypt_row_on_read`]'s AAD policy
/// (Randomised binds `row_pk`; Deterministic omits it).
///
/// For null-valued parents the function returns `Ok(None)` — the mask
/// pass writes nothing (matches `apply_mask_on_write`'s Q-MASK-L
/// pass-through-null rule).
///
/// The `keys` parameter carries the decrypt path: an encrypted column
/// resolves its key + recovers plaintext before the mask transform;
/// plaintext columns (`enc_meta == None`) never touch it and take the direct
/// branch.
///
/// It was a `B: EncryptedColumn` bound until 2026-09-02, which made every
/// caller pick a vendor to instantiate it with. A [`KeyStore`] is the whole of
/// what that bound supplied.
#[allow(clippy::too_many_arguments)]
pub async fn apply_mask_to_one_row(
    keys: &KeyStore,
    app_id: &str,
    collection: &str,
    column: &str,
    enc_meta: Option<&crate::diff::EncryptionMeta>,
    kind: MaskKind,
    row_pk: &str,
    value: &Value,
) -> Result<Option<String>, DbError> {
    if value.is_null() {
        return Ok(None);
    }

    // Decrypt path: column is encrypted → recover plaintext bytes →
    // serialise per `wraps` → mask. Mirrors
    // `decrypt_row_on_read`'s code path without going through the
    // shared helper because we only need the plaintext STRING form
    // (the mask consumer), not the typed JSON value.
    let plaintext_string: String = if let Some(enc) = enc_meta {
        let Some(hex_str) = value.as_str() else {
            return Err(DbError::internal(format!(
                "apply_mask_to_one_row: encrypted column '{column}' \
                 expected BYTEA text shape, got {value:?}"
            )));
        };
        let bytes = hex_to_bytes(hex_str)?;
        let key = keys.resolve(app_id, &enc.key_id).await?;
        let aad = crate::encryption::aad::canonical_aad(
            collection,
            column,
            match enc.mode {
                crate::backend::EncryptionMode::Randomised => Some(row_pk.as_bytes()),
                crate::backend::EncryptionMode::Deterministic => None,
            },
        );
        let plaintext_bytes = crate::encryption::aead::decrypt(&key, &bytes, &aad)?;
        wrapped_bytes_to_string(&plaintext_bytes, enc.wraps)
    } else {
        plaintext_from_value(column, value)?
    };

    Ok(Some(apply_mask_kind(kind, &plaintext_string)))
}

/// Read the parent column's value as the string the mask transform
/// consumes. Used by [`apply_mask_to_one_row`]'s "no enc_meta" branch.
fn plaintext_from_value(column: &str, value: &Value) -> Result<String, DbError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Null => Ok(String::new()),
        other => Err(DbError::internal(format!(
            "apply_mask_to_one_row: column '{column}' has \
             unsupported value shape {other:?}"
        ))),
    }
}

/// Audit-name shape for the mask backfill on `(collection, column)`.
/// Matches the migrations-framework `name` convention (`mig:<tag>`)
/// so operators querying `__zeroship_migrations.collection` see a
/// consistent prefix across both JS-driven and native backfills.
#[must_use]
pub fn backfill_audit_name(collection: &str, column: &str) -> String {
    format!("mask_backfill_{collection}_{column}")
}

/// Audit-name shape for the mask rewrite on `(collection, column)`.
#[must_use]
pub fn rewrite_audit_name(collection: &str, column: &str) -> String {
    format!("mask_rewrite_{collection}_{column}")
}

/// Backfill state surfaced to the apply layer — primarily for
/// observability + testing. The apply layer ignores the return value
/// on the production path; tests inspect `processed` to assert the
/// loop saw every row.
#[derive(Debug, Default, Clone)]
pub struct BackfillReport {
    /// Total rows updated by this run (across every batch).
    pub processed: i64,
}

// ---------------------------------------------------------------------
// THE MASK BACKFILL / REWRITE / REMOVE RUNNERS ARE DELETED.
//
// They walked every row of a creator's table writing the `<col>_masked`
// sibling, and two of them finished with DDL: the backfill with
// `ALTER COLUMN ... SET NOT NULL`, the remove with
// `ALTER TABLE ... DROP COLUMN`. plugin-db does not touch DDL, and it is not
// the schema authority, so a mask lifecycle it can only half-perform does not
// belong here. Their former schema-apply caller is deleted too.
//
// WHERE THIS GOES INSTEAD. Backfill is a migration-engine capability:
// `PlanStep::Backfill` carries a structured, resumable `BackfillSpec` that is
// journaled and checksummed - a better home than an ad-hoc row walk here.
//
// ONE CASE HAS NO HOME, recorded rather than left to be discovered. A mask over
// an ENCRYPTED column cannot be expressed as an engine backfill: the runner
// deleted here called `backend.resolve_key(...)` then `backend.decrypt(...)`,
// computing the mask in Rust from AEAD-decrypted plaintext. `BackfillSpec` is
// structured SQL and the engine holds no key material. So adding a mask to an
// existing encrypted column is unsupported end to end, by decision, not by
// oversight. Masks on encrypted columns still work when declared UP FRONT -
// the CRUD write path computes them per row (`apply_mask_to_one_row`).
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// Shared loop
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// Audit-table helpers
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// Helpers shared with the existing encryption pass
// ---------------------------------------------------------------------

/// Convert decrypted plaintext bytes to the human-readable string the
/// mask pass needs. Mirrors `plaintext_to_sidechannel_string` in
/// `encryption_pass.rs` but operates on raw bytes (we already
/// decrypted the BYTEA blob).
fn wrapped_bytes_to_string(bytes: &[u8], wraps: crate::diff::WrappedType) -> String {
    match wraps {
        crate::diff::WrappedType::String => {
            // UTF-8-validate; non-UTF-8 plaintext is a contract
            // violation (the SDK only ever wraps strings on
            // `wraps = "string"`). We render lossy here to avoid
            // panicking — the mask string ends up garbled but the
            // backfill proceeds.
            String::from_utf8_lossy(bytes).into_owned()
        }
        crate::diff::WrappedType::Number => {
            if bytes.len() != 8 {
                return String::new();
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(bytes);
            f64::from_be_bytes(arr).to_string()
        }
        crate::diff::WrappedType::Bytes => {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        }
    }
}

/// Decode a PG text-protocol `\xHH..` hex string into raw bytes.
/// Duplicates `encryption_pass.rs::hex_to_bytes` so we don't have to
/// flip its visibility for one consumer.
fn hex_to_bytes(s: &str) -> Result<Vec<u8>, DbError> {
    let hex = s.strip_prefix("\\x").unwrap_or(s);
    if !hex.len().is_multiple_of(2) {
        return Err(DbError::internal(format!(
            "mask_backfill: BYTEA text has odd hex length {}",
            hex.len()
        )));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = nibble(bytes[i])?;
        let lo = nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn nibble(c: u8) -> Result<u8, DbError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(DbError::internal(format!(
            "mask_backfill: BYTEA text non-hex byte 0x{c:02x}"
        ))),
    }
}

// ---------------------------------------------------------------------
// Pure-helper: kind+classification → masked string (no I/O)
// ---------------------------------------------------------------------

/// Compute the masked sibling value from an arbitrary plaintext
/// string. Pure wrapper around [`apply_mask_kind`] — exposed so the
/// SQLite integration test can derive the expected sibling without
/// reaching into `mask_pass`'s private surface.
///
/// `plaintexts` is the encryption-pass sidechannel form
/// ([`crate::crud::mask_pass::MaskPlaintextSidechannel`]) — for the
/// non-encrypted case, callers pass a fresh empty map and the
/// function reads `row[col]` directly. Same precedence as
/// `apply_mask_on_write`.
#[must_use]
#[allow(dead_code)] // helper exposed for tests
pub fn compute_masked_for_plaintext(kind: MaskKind, plaintext: &str) -> String {
    apply_mask_kind(kind, plaintext)
}

/// Compute the mask for the same shape `apply_mask_on_write` consumes —
/// `(schema, row, plaintexts)` — returning `(logical_field → masked_string)`
/// pairs WITHOUT mutating the row.
///
/// The key is the LOGICAL field name, because after the storage flip that is
/// the column the mask lives in. It used to be `<col>_masked`; leaving the
/// suffix here would have kept a second, wrong derivation of a physical column
/// name alive in a module nothing calls, which is exactly how one gets copied
/// back into a live path.
#[must_use]
#[allow(dead_code)] // helper exposed for tests
pub fn compute_masked_pairs_for_row(
    schema: &Value,
    row: &Value,
    plaintexts: &crate::crud::mask_pass::MaskPlaintextSidechannel,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(schema_obj) = schema.as_object() else {
        return out;
    };
    let Some(obj) = row.as_object() else {
        return out;
    };
    for (col, def) in schema_obj.iter() {
        let Some(mask_meta) = def.get("mask").and_then(|v| v.as_object()) else {
            continue;
        };
        let kind_str = mask_meta
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("full");
        if kind_str == "none" {
            continue;
        }
        let Some(kind) = MaskKind::from_sql(kind_str) else {
            continue;
        };
        let plaintext: Option<Zeroizing<String>> = if let Some(pt) = plaintexts.get(col) {
            Some(pt.clone())
        } else if let Some(v) = obj.get(col) {
            if v.is_null() {
                None
            } else if let Some(s) = v.as_str() {
                Some(Zeroizing::new(s.to_string()))
            } else {
                v.as_i64().map(|n| Zeroizing::new(n.to_string()))
            }
        } else {
            None
        };
        if let Some(pt) = plaintext {
            out.push((col.clone(), apply_mask_kind(kind, pt.as_str())));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_mask_sentinel_round_trips() {
        let s = build_mask_sentinel(MaskKind::Last4, Classification::Spi);
        assert_eq!(s, "__zsmask:kind=last4,classification=spi");
        let (kind, class) = parse_mask_sentinel(&s).unwrap();
        assert_eq!(kind, MaskKind::Last4);
        assert_eq!(class, Classification::Spi);
    }

    #[test]
    fn build_mask_sentinel_for_every_kind_classification_pair() {
        for kind in [
            MaskKind::Full,
            MaskKind::Last4,
            MaskKind::First4,
            MaskKind::Email,
            MaskKind::Name,
            MaskKind::DateYear,
            MaskKind::DateDecade,
        ] {
            for class in [
                Classification::Public,
                Classification::Pii,
                Classification::Spi,
                Classification::Phi,
                Classification::Pci,
                Classification::Internal,
            ] {
                let s = build_mask_sentinel(kind, class);
                let parsed = parse_mask_sentinel(&s).unwrap();
                assert_eq!(parsed, (kind, class), "round-trip {kind:?} / {class:?}");
            }
        }
    }

    #[test]
    fn malformed_mask_sentinel_returns_typed_error() {
        // Missing prefix.
        let err = parse_mask_sentinel("kind=last4,classification=spi").unwrap_err();
        assert!(
            err.clone()
                .into_string()
                .contains("mask_sentinel_malformed")
        );

        // Unknown kind.
        let err = parse_mask_sentinel("__zsmask:kind=blink_182,classification=pii").unwrap_err();
        let msg = err.clone().into_string();
        assert!(msg.contains("mask_sentinel_malformed"));
        assert!(msg.contains("blink_182"));

        // Unknown classification.
        let err = parse_mask_sentinel("__zsmask:kind=last4,classification=cosmic").unwrap_err();
        assert!(err.clone().into_string().contains("cosmic"));

        // Missing kind.
        let err = parse_mask_sentinel("__zsmask:classification=pii").unwrap_err();
        assert!(err.clone().into_string().contains("missing kind="));

        // Extra junk.
        let err =
            parse_mask_sentinel("__zsmask:kind=last4,classification=pii,extra=bogus").unwrap_err();
        assert!(err.clone().into_string().contains("unrecognised key"));
    }

    #[test]
    fn audit_name_shapes_are_distinct() {
        assert_eq!(
            backfill_audit_name("users", "ssn"),
            "mask_backfill_users_ssn"
        );
        assert_eq!(rewrite_audit_name("users", "ssn"), "mask_rewrite_users_ssn");
        assert_ne!(
            backfill_audit_name("users", "ssn"),
            rewrite_audit_name("users", "ssn")
        );
    }

    #[test]
    fn compute_masked_pairs_for_row_reads_from_sidechannel_for_encrypted() {
        // Encrypted column: plaintext arrives via sidechannel; row
        // holds the base64 ciphertext.
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" },
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let row = serde_json::json!({ "id": "u1", "ssn": "BASE64CT" });
        let mut pt = crate::crud::mask_pass::MaskPlaintextSidechannel::new();
        pt.insert("ssn".to_string(), Zeroizing::new("123-45-6789".to_string()));
        let pairs = compute_masked_pairs_for_row(&schema, &row, &pt);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("ssn".to_string(), "***-**-6789".to_string()));
    }

    #[test]
    fn compute_masked_pairs_for_row_reads_from_row_for_plaintext() {
        let schema = serde_json::json!({
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let row = serde_json::json!({ "id": "u1", "email": "alice@example.com" });
        let pt = crate::crud::mask_pass::MaskPlaintextSidechannel::new();
        let pairs = compute_masked_pairs_for_row(&schema, &row, &pt);
        assert_eq!(pairs.len(), 1);
        assert_eq!(
            pairs[0],
            ("email".to_string(), "a***@example.com".to_string())
        );
    }

    #[test]
    fn compute_masked_pairs_for_row_skips_kind_none() {
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "none", "classification": "spi" }
            }
        });
        let row = serde_json::json!({ "id": "u1", "ssn": "123-45-6789" });
        let pt = crate::crud::mask_pass::MaskPlaintextSidechannel::new();
        let pairs = compute_masked_pairs_for_row(&schema, &row, &pt);
        assert!(pairs.is_empty(), "kind=none must produce no pairs");
    }

    #[test]
    fn compute_masked_pairs_for_row_skips_null_parent() {
        let schema = serde_json::json!({
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let row = serde_json::json!({ "id": "u1", "email": null });
        let pt = crate::crud::mask_pass::MaskPlaintextSidechannel::new();
        let pairs = compute_masked_pairs_for_row(&schema, &row, &pt);
        assert!(pairs.is_empty(), "null parent must produce no pairs");
    }

    #[test]
    fn compute_masked_pairs_for_row_emits_one_per_masked_column() {
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            },
            "name": { "type": "string" }
        });
        let row = serde_json::json!({
            "id": "u1",
            "ssn": "123-45-6789",
            "email": "alice@example.com",
            "name": "alice"
        });
        let pt = crate::crud::mask_pass::MaskPlaintextSidechannel::new();
        let pairs = compute_masked_pairs_for_row(&schema, &row, &pt);
        assert_eq!(pairs.len(), 2);
        let mut sorted = pairs.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(sorted[0].0, "email");
        assert_eq!(sorted[0].1, "a***@example.com");
        assert_eq!(sorted[1].0, "ssn");
        assert_eq!(sorted[1].1, "***-**-6789");
    }
}
