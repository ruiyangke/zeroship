//! Transparent column encryption pass — encrypt-on-write /
//! decrypt-on-read for `t.encrypted(...)`-declared columns.
//!
//! ## How it plugs into CRUD
//!
//! The pass runs around every `BuiltQuery` build site (`build_insert`,
//! `build_update_*`) and every `exec_query` result. For each column on
//! the row whose schema entry carries an `encrypted` metadata block:
//!
//! - **Write path** ([`encrypt_row_on_write`]): serialise the typed
//!   plaintext (per `wraps`), build the canonical AAD (Camp A — row_pk
//!   bound for Randomised, omitted for Deterministic; see
//!   `docs/proposals/p5-encryption-backup-implementation-plan.md` §13),
//!   call `EncryptedColumn::encrypt`, swap the JSON Value to a base64
//!   string of the ciphertext blob. The SQL build layer then recognises
//!   the column and emits `decode($N, 'base64')::bytea` at the
//!   parameter site so the BYTEA column receives raw bytes.
//!
//! - **Read path** ([`decrypt_row_on_read`]): receive the row already
//!   decoded into `Value` (BYTEA columns are surfaced as `\xHHHH...`
//!   hex strings by `compio-postgres`'s text protocol). Parse the hex
//!   back to bytes, call `EncryptedColumn::decrypt` with the same AAD
//!   the write path used, deserialise per `wraps`, swap back into the
//!   row.
//!
//! ## Why row_pk-in-AAD is non-negotiable for Randomised
//!
//! Without binding the row PK, an attacker with UPDATE-only access
//! could copy row A's ciphertext into row B's column slot and read
//! row A's plaintext via row B's read API. With row_pk in AAD,
//! moving the ciphertext breaks AAD reconstruction → tag verification
//! fails → typed `encryption_aead_failed` error surfaces, the
//! mismatch becomes loud. Per §13 (Camp A), plugin-db mints typed_id
//! PKs SDK-side before INSERT, so `row_pk` is ALWAYS available when
//! `encrypt()` is called — single-phase INSERT, no
//! chicken-and-egg.
//!
//! ## Why Deterministic mode omits row_pk
//!
//! Deterministic mode's defining property is "same plaintext + same
//! `(collection, column)` → same ciphertext", which is what makes the
//! B-tree-on-ciphertext equality lookup work. Binding row_pk would
//! produce a different ciphertext per row and break the equality
//! lookup (the entire reason deterministic mode exists). The SDK's
//! `validateEncryptedFieldsInFilter` enforces a strict equality-only
//! filter contract on deterministic columns — range / regex / LIKE are
//! refused before the call reaches Rust.
//!
//! ## Wire shape (JSON Value)
//!
//! `serde_json::Value` cannot carry raw bytes — the only "binary"
//! variant is `Value::String`. We base64-encode the ciphertext blob
//! into a `Value::String` and rely on the SQL build layer to wrap the
//! parameter placeholder with `decode($N, 'base64')::bytea`. The
//! ciphertext blob is the `wire::pack`-framed
//! `[version_flag | nonce | ct+tag]` produced by
//! `crate::encryption::aead`. The base64 step adds ~33% transient
//! memory overhead on the JSON side; the on-disk BYTEA payload is the
//! raw bytes.

use base64::Engine as _;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::backend::EncryptedColumn;
use crate::error::DbError;

/// Encrypt every `t.encrypted(...)`-declared column on `row` in place.
///
/// `row_pk` is the typed_id minted SDK-side (per Camp A, ALWAYS
/// available before INSERT). For UPDATE the caller passes the target
/// row's PK pulled from the filter — `{ id: ... }`.
///
/// On success, every encrypted column in `row` has its plaintext
/// `Value` swapped for a `Value::String` carrying the base64-encoded
/// ciphertext blob.
///
/// **Marker side-channel**: the function also inserts a sibling
/// `__zsenc__<col>` marker key (also `Value::Bool(true)`) for each
/// encrypted column it processed. The SQL builder uses this marker to
/// know which placeholders need the `decode($N, 'base64')::bytea`
/// cast. The marker is stripped before the row leaves the SQL builder.
///
/// **P5.5 PR 2** — overload that captures plaintexts for the
/// downstream mask pass. See [`encrypt_row_on_write_with_sidechannel`]
/// for the version that populates a [`MaskPlaintextSidechannel`]
/// (`HashMap<String, Zeroizing<String>>`) BEFORE replacing the plaintext with
/// ciphertext, so the mask pass can derive the sibling
/// `<col>_masked` column without re-decrypting. The original
/// signature stays for callers that don't care about mask integration.
pub async fn encrypt_row_on_write<B>(
    backend: &B,
    app_id: &str,
    collection: &str,
    schema: &Value,
    row_pk: &str,
    row: &mut Value,
) -> Result<(), DbError>
where
    B: EncryptedColumn,
{
    let mut sidechannel = crate::crud::mask_pass::MaskPlaintextSidechannel::new();
    encrypt_row_on_write_with_sidechannel(
        backend,
        app_id,
        collection,
        schema,
        row_pk,
        row,
        &mut sidechannel,
    )
    .await
}

/// **P5.5 PR 2** — encrypt with plaintext sidechannel capture.
///
/// Behaves identically to [`encrypt_row_on_write`] EXCEPT it populates
/// `sidechannel[col]` with the raw plaintext for every encrypted
/// column it processes (UTF-8-decoded when `wraps = "string"`; the
/// stringified `f64` for `wraps = "number"`; the base64-encoded raw
/// bytes for `wraps = "bytes"`). The mask pass consumes this map to
/// derive the sibling `<col>_masked` column without paying the
/// decryption round-trip.
///
/// Non-encrypted columns and `null`-valued columns are NOT added to
/// the sidechannel — the mask pass already handles those by reading
/// `row[col]` directly (the non-encrypted path) or skipping (`null`).
pub(crate) async fn encrypt_row_on_write_with_sidechannel<B>(
    backend: &B,
    app_id: &str,
    collection: &str,
    schema: &Value,
    row_pk: &str,
    row: &mut Value,
    sidechannel: &mut crate::crud::mask_pass::MaskPlaintextSidechannel,
) -> Result<(), DbError>
where
    B: EncryptedColumn,
{
    let Some(schema_obj) = schema.as_object() else {
        return Ok(()); // schema not present → no encrypted columns to find
    };
    let Some(obj) = row.as_object_mut() else {
        return Err(DbError::internal(
            "encrypt_row_on_write: row must be a JSON object",
        ));
    };

    // Collect (col, mode, key_id, wraps, plaintext_bytes, sidechannel_str)
    // up front so we can release the borrow on `obj` before calling the
    // async `resolve_key` (which would otherwise hold a mutable borrow
    // across the await).
    //
    // `sidechannel_str` is the human-readable plaintext form the mask
    // pass uses to derive `<col>_masked`. Per `wraps`:
    // - `string` → UTF-8 decode of the bytes (same as JS-side string).
    // - `number` → `f64`.to_string() (the SDK has the same lossiness).
    // - `bytes`  → the JSON-wire base64 form (the same string the SDK
    //              sees on `t.bytes()` fields).
    let mut to_encrypt: Vec<(
        String,
        crate::backend::EncryptionMode,
        String,
        &'static str,
        Zeroizing<Vec<u8>>,
        Zeroizing<String>,
    )> = Vec::new();
    for (col, def) in schema_obj.iter() {
        let Some(enc_meta) = def.get("encrypted").and_then(|v| v.as_object()) else {
            continue;
        };
        let mode = parse_mode(enc_meta)?;
        let key_id = enc_meta
            .get("keyId")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        let wraps = parse_wraps(enc_meta);

        let Some(value) = obj.get(col) else {
            continue; // not present on this row → nothing to do (e.g. partial update)
        };
        if value.is_null() {
            continue; // NULL stays NULL — encrypting NULL has no semantic meaning
        }
        let plaintext = Zeroizing::new(serialise_wrapped(value, wraps)?);
        let sidechannel_str = Zeroizing::new(plaintext_to_sidechannel_string(value, wraps));
        to_encrypt.push((col.clone(), mode, key_id, wraps, plaintext, sidechannel_str));
    }

    for (col, mode, key_id, _wraps, plaintext, sidechannel_str) in to_encrypt {
        let key = backend.resolve_key(app_id, &key_id).await?;
        let aad = crate::encryption::aad::canonical_aad(
            collection,
            &col,
            match mode {
                crate::backend::EncryptionMode::Randomised => Some(row_pk.as_bytes()),
                crate::backend::EncryptionMode::Deterministic => None,
            },
        );
        let ciphertext = backend.encrypt(&key, mode, &plaintext, &aad)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&ciphertext);
        // Stash the plaintext for the mask pass BEFORE replacing the
        // row value with the base64 ciphertext.
        sidechannel.insert(col.clone(), sidechannel_str);
        let obj = row.as_object_mut().expect("checked above");
        obj.insert(col.clone(), Value::String(b64));
        // Sibling marker so the SQL builder knows to wrap the
        // placeholder with `decode($N, 'base64')::bytea`. Stripped
        // before the row leaves the build layer.
        obj.insert(format!("__zsenc__{col}"), Value::Bool(true));
    }
    Ok(())
}

/// Render a plaintext JSON `Value` to the canonical string form the
/// mask pass needs. Mirrors [`serialise_wrapped`]'s shape but stays in
/// `String` land (the mask pass consumes strings, not bytes).
fn plaintext_to_sidechannel_string(value: &Value, wraps: &str) -> String {
    match wraps {
        "string" => value.as_str().unwrap_or("").to_string(),
        "number" => value.as_f64().map(|n| n.to_string()).unwrap_or_default(),
        "bytes" => value.as_str().unwrap_or("").to_string(),
        _ => String::new(),
    }
}

/// Decrypt every `t.encrypted(...)`-declared column on `row` in place.
///
/// `row_pk` is read from `row["id"]`. The function mirrors the
/// AAD-shape policy of [`encrypt_row_on_write`] — Randomised binds
/// `row_pk`, Deterministic omits it. The wire format is mode-agnostic
/// on the read side; the AAD reconstruction is what selects the
/// mode-appropriate behaviour.
///
/// On a tag-verification failure (tampered ciphertext, wrong AAD,
/// wrong key) the function surfaces
/// `ValidationFailed { code: "encryption_aead_failed" }`.
pub async fn decrypt_row_on_read<B>(
    backend: &B,
    app_id: &str,
    collection: &str,
    schema: &Value,
    row: &mut Value,
) -> Result<(), DbError>
where
    B: EncryptedColumn,
{
    let Some(schema_obj) = schema.as_object() else {
        return Ok(());
    };
    let Some(obj) = row.as_object_mut() else {
        // Not an object (e.g. NULL row); nothing to do.
        return Ok(());
    };

    // Per Camp A: row_pk arrives in the RETURNING / SELECT result on the
    // `id` column (typed_id minted SDK-side, always present on rows
    // produced by INSERT/UPDATE/SELECT *). Allow either string PK or
    // numeric id (legacy collections); typed_ids serialise as strings.
    let row_pk = match obj.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };

    // Same async-borrow shuffle as the write path.
    let mut to_decrypt: Vec<(String, crate::backend::EncryptionMode, String, &'static str, Vec<u8>)> =
        Vec::new();
    for (col, def) in schema_obj.iter() {
        let Some(enc_meta) = def.get("encrypted").and_then(|v| v.as_object()) else {
            continue;
        };
        let mode = parse_mode(enc_meta)?;
        let key_id = enc_meta
            .get("keyId")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        let wraps = parse_wraps(enc_meta);

        let Some(value) = obj.get(col) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        // Read rows carry encrypted blobs as base64 on both backends.
        // For back-compat with older PG callers we also accept the
        // legacy `\x...` text-protocol BYTEA envelope.
        let Some(wire_str) = value.as_str() else {
            // The row carries something other than the expected BYTEA
            // text shape — fail loud so a regression in the introspect /
            // bind layer surfaces immediately rather than producing
            // garbled plaintext.
            return Err(DbError::internal(format!(
                "decrypt_row_on_read: column '{col}' expected encrypted blob text, got {value:?}"
            )));
        };
        let bytes = if wire_str.starts_with("\\x") {
            hex_to_bytes(wire_str)?
        } else {
            base64::engine::general_purpose::STANDARD
                .decode(wire_str)
                .map_err(|e| {
                    DbError::internal(format!(
                        "decrypt_row_on_read: column '{col}' is not valid base64: {e}"
                    ))
                })?
        };
        to_decrypt.push((col.clone(), mode, key_id, wraps, bytes));
    }

    for (col, mode, key_id, wraps, blob) in to_decrypt {
        let key = backend.resolve_key(app_id, &key_id).await?;
        let aad = crate::encryption::aad::canonical_aad(
            collection,
            &col,
            match mode {
                crate::backend::EncryptionMode::Randomised => Some(row_pk.as_bytes()),
                crate::backend::EncryptionMode::Deterministic => None,
            },
        );
        let plaintext = Zeroizing::new(backend.decrypt(&key, mode, &blob, &aad)?);
        let value = deserialise_wrapped(&plaintext, wraps)?;
        let obj = row.as_object_mut().expect("checked above");
        obj.insert(col, value);
    }
    Ok(())
}

/// **P5 PR 3.5** — pre-process a row batch produced by the SQLite
/// CRUD path so the shared [`decrypt_row_on_read`] helper can consume
/// it without forking.
///
/// The SQLite CRUD read path surfaces BLOB columns as base64
/// `Value::String` (the JSON-transport encoding for raw bytes). The
/// shared decrypt helper expects PG's text-protocol `\xHHHH...` hex
/// shape because the existing PG arm rides on `compio-postgres`'s
/// BYTEA text encoding. Rather than fork the decrypt body, we rewrite
/// SQLite-side encrypted-column values from base64 → `\x`-hex in place
/// before invoking the shared helper.
///
/// **Only encrypted columns** are rewritten — non-encrypted BLOB
/// columns (vector blobs, FTS rank, …) are left untouched.
pub(crate) fn rewrite_sqlite_encrypted_row_blobs_to_hex(
    schema: &Value,
    rows: &mut [Value],
) -> Result<(), DbError> {
    let Some(schema_obj) = schema.as_object() else {
        return Ok(());
    };
    let enc_cols: Vec<&str> = schema_obj
        .iter()
        .filter(|(_, def)| def.get("encrypted").is_some())
        .map(|(name, _)| name.as_str())
        .collect();
    if enc_cols.is_empty() {
        return Ok(());
    }
    for row in rows.iter_mut() {
        let Some(obj) = row.as_object_mut() else {
            continue;
        };
        for col in &enc_cols {
            let Some(cur) = obj.get(*col) else { continue };
            if cur.is_null() {
                continue;
            }
            let Some(b64) = cur.as_str() else {
                return Err(DbError::internal(format!(
                    "rewrite_sqlite_encrypted_row_blobs_to_hex: column '{col}' expected base64 string, got {cur:?}"
                )));
            };
            let raw = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| {
                    DbError::internal(format!(
                        "rewrite_sqlite_encrypted_row_blobs_to_hex: column '{col}' base64 decode failed: {e}"
                    ))
                })?;
            let mut hex = String::with_capacity(2 + raw.len() * 2);
            hex.push('\\');
            hex.push('x');
            for b in &raw {
                use std::fmt::Write as _;
                let _ = write!(hex, "{b:02x}");
            }
            obj.insert((*col).to_string(), Value::String(hex));
        }
    }
    Ok(())
}

/// Serialise a plaintext `Value` into the canonical byte layout for its
/// `wraps` type. Layouts:
///
/// - `"string"` → UTF-8 bytes (no terminator)
/// - `"number"` → `f64` big-endian (8 bytes). We use f64 to match
///   `t.number()`'s storage type (`DOUBLE PRECISION`); integers are
///   stored as their f64 representation.
/// - `"bytes"` → base64-decoded raw bytes (the SDK passes byte fields
///   as base64-encoded `Value::String`).
fn serialise_wrapped(value: &Value, wraps: &str) -> Result<Vec<u8>, DbError> {
    match wraps {
        "string" => match value.as_str() {
            Some(s) => Ok(s.as_bytes().to_vec()),
            None => Err(DbError::validation(
                "encrypted_value_type_mismatch",
                format!("encrypted column declared wraps=string but value is {value:?}"),
            )),
        },
        "number" => match value.as_f64() {
            Some(n) => Ok(n.to_be_bytes().to_vec()),
            None => Err(DbError::validation(
                "encrypted_value_type_mismatch",
                format!("encrypted column declared wraps=number but value is {value:?}"),
            )),
        },
        "bytes" => match value.as_str() {
            Some(s) => base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(|e| {
                    DbError::validation(
                        "encrypted_value_type_mismatch",
                        format!(
                            "encrypted column declared wraps=bytes but value is not base64: {e}"
                        ),
                    )
                }),
            None => Err(DbError::validation(
                "encrypted_value_type_mismatch",
                format!("encrypted column declared wraps=bytes but value is {value:?}"),
            )),
        },
        other => Err(DbError::internal(format!(
            "encrypted wraps must be string/number/bytes, got '{other}'"
        ))),
    }
}

/// Inverse of [`serialise_wrapped`].
fn deserialise_wrapped(bytes: &[u8], wraps: &str) -> Result<Value, DbError> {
    match wraps {
        "string" => {
            let s = std::str::from_utf8(bytes).map_err(|e| {
                DbError::internal(format!(
                    "decrypted plaintext is not valid UTF-8: {e}"
                ))
            })?;
            Ok(Value::String(s.to_string()))
        }
        "number" => {
            if bytes.len() != 8 {
                return Err(DbError::internal(format!(
                    "decrypted number plaintext must be 8 bytes, got {}",
                    bytes.len()
                )));
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(bytes);
            let n = f64::from_be_bytes(arr);
            // `serde_json::Number::from_f64` returns None for NaN /
            // Infinity. Coerce to `null` for those — they're not
            // legitimate column values anyway.
            Ok(serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number))
        }
        "bytes" => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
            Ok(Value::String(b64))
        }
        other => Err(DbError::internal(format!(
            "encrypted wraps must be string/number/bytes, got '{other}'"
        ))),
    }
}

/// Parse the `mode` field from an `encrypted` metadata object. Returns
/// a typed error if the mode is missing or unknown.
fn parse_mode(
    enc_meta: &serde_json::Map<String, Value>,
) -> Result<crate::backend::EncryptionMode, DbError> {
    let mode_str = enc_meta
        .get("mode")
        .and_then(|v| v.as_str())
        .ok_or_else(|| DbError::internal("encrypted.mode missing in schema"))?;
    match mode_str {
        "randomised" | "randomized" => Ok(crate::backend::EncryptionMode::Randomised),
        "deterministic" => Ok(crate::backend::EncryptionMode::Deterministic),
        other => Err(DbError::internal(format!(
            "encrypted.mode must be 'randomised' or 'deterministic', got '{other}'"
        ))),
    }
}

/// Pick the `wraps` field; default `"string"` for forward-compat with
/// `t.encrypted()` calls that omit it.
fn parse_wraps(enc_meta: &serde_json::Map<String, Value>) -> &'static str {
    match enc_meta.get("wraps").and_then(|v| v.as_str()) {
        Some("string") => "string",
        Some("number") => "number",
        Some("bytes") => "bytes",
        _ => "string",
    }
}

/// Convert a Postgres `\x`-prefixed hex string into raw bytes. Used on
/// the read path to recover the wire blob from the text-protocol BYTEA
/// representation.
fn hex_to_bytes(s: &str) -> Result<Vec<u8>, DbError> {
    let hex = s.strip_prefix("\\x").unwrap_or(s);
    if hex.len() % 2 != 0 {
        return Err(DbError::internal(format!(
            "BYTEA text has odd hex length: {}",
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
            "BYTEA text contains non-hex byte 0x{c:02x}"
        ))),
    }
}

/// Strip every `__zsenc__<col>` marker from `row` after the SQL builder
/// has consumed them to position the BYTEA cast at the right
/// placeholder. The markers must NOT survive to the parameter list (PG
/// has no such column).
pub fn strip_encryption_markers(row: &mut Value) {
    if let Some(obj) = row.as_object_mut() {
        obj.retain(|k, _| !k.starts_with("__zsenc__"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `serialise_wrapped` / `deserialise_wrapped` round-trip for the
    /// three supported wrapped types.
    #[test]
    fn round_trip_string() {
        let v = Value::String("hello".to_string());
        let bytes = serialise_wrapped(&v, "string").unwrap();
        assert_eq!(bytes, b"hello");
        let back = deserialise_wrapped(&bytes, "string").unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn round_trip_number() {
        let v: Value = serde_json::from_str("3.14159").unwrap();
        let bytes = serialise_wrapped(&v, "number").unwrap();
        assert_eq!(bytes.len(), 8);
        let back = deserialise_wrapped(&bytes, "number").unwrap();
        // f64 round-trips exactly via to/from_be_bytes
        assert_eq!(back.as_f64(), v.as_f64());
    }

    #[test]
    fn round_trip_bytes() {
        // Original "bytes" field carries base64 on the JSON wire.
        let raw = [0xde, 0xad, 0xbe, 0xef];
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let v = Value::String(b64.clone());
        let bytes = serialise_wrapped(&v, "bytes").unwrap();
        assert_eq!(bytes, raw);
        let back = deserialise_wrapped(&bytes, "bytes").unwrap();
        // Round-trip preserves the base64 form.
        assert_eq!(back.as_str(), Some(b64.as_str()));
    }

    /// `serialise_wrapped` rejects a non-string value when wraps=string.
    #[test]
    fn serialise_rejects_type_mismatch() {
        let v: Value = serde_json::from_str("42").unwrap();
        let err = serialise_wrapped(&v, "string").unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "encrypted_value_type_mismatch");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    /// hex_to_bytes round-trips with and without the `\x` prefix.
    #[test]
    fn hex_to_bytes_round_trip() {
        assert_eq!(hex_to_bytes("\\xdeadbeef").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(hex_to_bytes("deadbeef").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(hex_to_bytes("\\x").unwrap(), Vec::<u8>::new());
    }

    /// hex_to_bytes rejects odd-length and non-hex content with typed
    /// errors (`Internal`, not user-facing).
    #[test]
    fn hex_to_bytes_rejects_odd_length() {
        let err = hex_to_bytes("\\xabc").unwrap_err();
        assert!(matches!(err, DbError::Internal { .. }));
    }

    #[test]
    fn hex_to_bytes_rejects_garbage() {
        let err = hex_to_bytes("zz").unwrap_err();
        assert!(matches!(err, DbError::Internal { .. }));
    }

    /// `parse_mode` accepts both `randomised` and `randomized` and the
    /// canonical `deterministic`, rejects anything else with a typed
    /// internal error.
    #[test]
    fn parse_mode_variants() {
        let mut m = serde_json::Map::new();
        m.insert("mode".to_string(), Value::String("randomised".to_string()));
        assert!(matches!(
            parse_mode(&m).unwrap(),
            crate::backend::EncryptionMode::Randomised
        ));
        m.insert("mode".to_string(), Value::String("randomized".to_string()));
        assert!(matches!(
            parse_mode(&m).unwrap(),
            crate::backend::EncryptionMode::Randomised
        ));
        m.insert("mode".to_string(), Value::String("deterministic".to_string()));
        assert!(matches!(
            parse_mode(&m).unwrap(),
            crate::backend::EncryptionMode::Deterministic
        ));
        m.insert("mode".to_string(), Value::String("nope".to_string()));
        assert!(parse_mode(&m).is_err());
    }

    /// `strip_encryption_markers` removes every `__zsenc__*` key,
    /// preserves every other key.
    #[test]
    fn strip_markers_removes_only_marker_keys() {
        let mut row = serde_json::json!({
            "id": "usr_01",
            "ssn": "ciphertext-b64",
            "__zsenc__ssn": true,
            "name": "alice",
        });
        strip_encryption_markers(&mut row);
        let obj = row.as_object().unwrap();
        assert!(obj.contains_key("id"));
        assert!(obj.contains_key("ssn"));
        assert!(obj.contains_key("name"));
        assert!(!obj.contains_key("__zsenc__ssn"));
    }

    /// `encrypt_row_on_write` + `decrypt_row_on_read` round-trip with a
    /// stub backend (`AeadKey` directly, no PG round-trip). Pins the
    /// AAD policy: Randomised binds row_pk; Deterministic omits it.
    /// A row_pk mismatch on read of a Randomised column produces
    /// `encryption_aead_failed`.
    #[test]
    fn write_then_read_round_trip_randomised() {
        use crate::backend::{EncryptedColumn, EncryptionMode};
        use crate::encryption::aead::AeadKey;

        // Minimal in-test backend that exercises the same encrypt /
        // decrypt path the real PG impl uses.
        struct StubBackend;
        impl EncryptedColumn for StubBackend {
            type KeyHandle = AeadKey;
            async fn resolve_key(&self, _app_id: &str, _key_id: &str) -> Result<Self::KeyHandle, DbError> {
                Ok(AeadKey { k_enc: [0x11; 32], k_siv: [0x22; 32] })
            }
            fn encrypt(&self, key: &Self::KeyHandle, mode: EncryptionMode, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, DbError> {
                match mode {
                    EncryptionMode::Randomised => crate::encryption::aead::encrypt_randomised(key, plaintext, aad),
                    EncryptionMode::Deterministic => crate::encryption::aead::encrypt_deterministic(key, plaintext, aad),
                }
            }
            fn decrypt(&self, key: &Self::KeyHandle, _mode: EncryptionMode, blob: &[u8], aad: &[u8]) -> Result<Vec<u8>, DbError> {
                crate::encryption::aead::decrypt(key, blob, aad)
            }
        }

        let schema = serde_json::json!({
            "ssn": { "type": "string", "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" } },
            "name": { "type": "string" },
        });
        let mut row = serde_json::json!({ "id": "usr_01HX", "ssn": "123-45-6789", "name": "alice" });

        let rt = compio::runtime::Runtime::new().expect("compio runtime");
        rt.block_on(async {
            encrypt_row_on_write(&StubBackend, "app1", "users", &schema, "usr_01HX", &mut row).await.unwrap();
        });

        // After encrypt: ssn should be a base64 string, name unchanged,
        // marker present.
        let obj = row.as_object().unwrap();
        assert!(obj.get("ssn").and_then(|v| v.as_str()).is_some());
        assert_ne!(obj["ssn"].as_str().unwrap(), "123-45-6789");
        assert_eq!(obj.get("name").and_then(|v| v.as_str()), Some("alice"));
        assert_eq!(obj.get("__zsenc__ssn"), Some(&Value::Bool(true)));

        // Simulate the read path: the SQL bind layer returned BYTEA as
        // PG hex text, and stripped the marker. We mimic that by
        // base64-decoding our ciphertext and re-encoding as PG hex
        // text. (In production the BYTEA round-trip is handled by
        // compio-postgres; here we do it by hand for the unit test.)
        let b64 = obj["ssn"].as_str().unwrap().to_string();
        let raw = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let hex_str = format!(
            "\\x{}",
            raw.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let mut read_row = serde_json::json!({ "id": "usr_01HX", "ssn": hex_str, "name": "alice" });

        rt.block_on(async {
            decrypt_row_on_read(&StubBackend, "app1", "users", &schema, &mut read_row).await.unwrap();
        });

        assert_eq!(read_row["ssn"].as_str(), Some("123-45-6789"));
        assert_eq!(read_row["name"].as_str(), Some("alice"));

        // Same ciphertext under a DIFFERENT row_pk must fail tag check
        // (Camp A defence — the row-swap attack surfaces as
        // `encryption_aead_failed`).
        let mut wrong_pk_row = serde_json::json!({ "id": "usr_02HX", "ssn": hex_str, "name": "alice" });
        let err = rt.block_on(async {
            decrypt_row_on_read(&StubBackend, "app1", "users", &schema, &mut wrong_pk_row).await
        });
        match err {
            Err(DbError::ValidationFailed { code, .. }) => {
                assert_eq!(code, "encryption_aead_failed");
            }
            other => panic!("expected ValidationFailed/encryption_aead_failed, got {other:?}"),
        }
    }

    /// Deterministic mode: same plaintext under same `(collection,
    /// column)` produces the same ciphertext, regardless of row_pk
    /// (proof that AAD omits row_pk in this mode). Equality-on-
    /// ciphertext lookups depend on this.
    #[test]
    fn deterministic_same_plaintext_yields_same_ciphertext() {
        use crate::backend::{EncryptedColumn, EncryptionMode};
        use crate::encryption::aead::AeadKey;

        struct StubBackend;
        impl EncryptedColumn for StubBackend {
            type KeyHandle = AeadKey;
            async fn resolve_key(&self, _app_id: &str, _key_id: &str) -> Result<Self::KeyHandle, DbError> {
                Ok(AeadKey { k_enc: [0x33; 32], k_siv: [0x44; 32] })
            }
            fn encrypt(&self, key: &Self::KeyHandle, mode: EncryptionMode, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, DbError> {
                match mode {
                    EncryptionMode::Randomised => crate::encryption::aead::encrypt_randomised(key, plaintext, aad),
                    EncryptionMode::Deterministic => crate::encryption::aead::encrypt_deterministic(key, plaintext, aad),
                }
            }
            fn decrypt(&self, key: &Self::KeyHandle, _mode: EncryptionMode, blob: &[u8], aad: &[u8]) -> Result<Vec<u8>, DbError> {
                crate::encryption::aead::decrypt(key, blob, aad)
            }
        }

        let schema = serde_json::json!({
            "ssn": { "type": "string", "encrypted": { "mode": "deterministic", "keyId": "default", "wraps": "string" } },
        });

        let mut row_a = serde_json::json!({ "id": "usr_a", "ssn": "shared" });
        let mut row_b = serde_json::json!({ "id": "usr_b", "ssn": "shared" });

        let rt = compio::runtime::Runtime::new().expect("compio runtime");
        rt.block_on(async {
            encrypt_row_on_write(&StubBackend, "app1", "users", &schema, "usr_a", &mut row_a).await.unwrap();
            encrypt_row_on_write(&StubBackend, "app1", "users", &schema, "usr_b", &mut row_b).await.unwrap();
        });

        // Both row encryptions used DIFFERENT row_pks but produced
        // IDENTICAL ciphertext — that's the deterministic property.
        assert_eq!(row_a["ssn"], row_b["ssn"]);
    }
}
