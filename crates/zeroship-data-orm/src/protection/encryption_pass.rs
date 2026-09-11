//! Encrypt and decrypt declared fields using native ciphertext buffers.
//! Randomised encryption authenticates the row identity as well as collection
//! and column, so moving ciphertext to another row fails authentication.
//! Mask derivation receives protected plaintext before encryption replaces it.

use base64::Engine as _;
use zeroize::Zeroizing;
use zeroship_data_sql::value::Value;

use crate::encryption::KeyStore;
use zeroship_data_orm::error::DbError;

/// Encrypt every `t.encrypted(...)`-declared column on `row` in place.
///
/// `row_pk` is the typed_id minted SDK-side (per Camp A, ALWAYS
/// available before INSERT). For UPDATE the caller passes the target
/// row's PK pulled from the filter — `{ id: ... }`.
///
/// Each encrypted plaintext becomes a native byte buffer.
///
/// This is the overload that captures plaintexts for the
/// downstream mask pass. See [`encrypt_row_on_write_with_sidechannel`]
/// for the version that populates a [`crate::protection::mask_pass::MaskPlaintextSidechannel`]
/// (`HashMap<String, Zeroizing<String>>`) BEFORE replacing the plaintext with
/// ciphertext, so the mask pass can derive the sibling
/// `<col>_masked` column without re-decrypting. The original
/// signature stays for callers that don't care about mask integration.
/// One column staged for encryption: `(col, wraps,
/// plaintext_bytes, sidechannel_str)` — collected up front (see
/// [`encrypt_row_on_write`]'s body comment) so the borrow on the
/// schema object can be released before the async `resolve_key` call.
type PendingEncryption = (
    String,
    &'static str,
    Zeroizing<Vec<u8>>,
    Zeroizing<String>,
);

#[cfg(any(test, feature = "test-helpers"))]
pub async fn encrypt_row_on_write(
    keys: &KeyStore,
    app_id: &str,
    collection: &str,
    schema: &Value,
    row_pk: &str,
    row: &mut Value,
) -> Result<(), DbError> {
    let mut sidechannel = crate::protection::mask_pass::MaskPlaintextSidechannel::new();
    encrypt_row_on_write_with_sidechannel(
        keys,
        app_id,
        collection,
        schema,
        row_pk,
        row,
        &mut sidechannel,
    )
    .await
}

/// Encrypt with plaintext sidechannel capture.
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
pub async fn encrypt_row_on_write_with_sidechannel(
    keys: &KeyStore,
    app_id: &str,
    collection: &str,
    schema: &Value,
    row_pk: &str,
    row: &mut Value,
    sidechannel: &mut crate::protection::mask_pass::MaskPlaintextSidechannel,
) -> Result<(), DbError> {
    let Some(schema_obj) = schema.as_object() else {
        return Ok(()); // schema not present → no encrypted columns to find
    };
    let Some(obj) = row.as_object_mut() else {
        return Err(DbError::internal(
            "encrypt_row_on_write: row must be a JSON object",
        ));
    };

    // Collect (col, wraps, plaintext_bytes, sidechannel_str)
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
    let mut to_encrypt: Vec<PendingEncryption> = Vec::new();
    for (col, def) in schema_obj.iter() {
        let Some(enc_meta) = def.get("encrypted").and_then(|v| v.as_object()) else {
            continue;
        };
        let wraps = parse_wraps(enc_meta);

        let Some(value) = obj.get(col) else {
            continue; // not present on this row → nothing to do (e.g. partial update)
        };
        if value.is_null() {
            continue; // NULL stays NULL — encrypting NULL has no semantic meaning
        }
        if row_pk.is_empty() {
            return Err(DbError::validation(
                "encrypted_row_id_required",
                "encrypted values require a row identity",
            ));
        }
        let plaintext = Zeroizing::new(serialise_wrapped(value, wraps)?);
        let sidechannel_str = Zeroizing::new(plaintext_to_sidechannel_string(value, wraps));
        to_encrypt.push((col.clone(), wraps, plaintext, sidechannel_str));
    }

    for (col, _wraps, plaintext, sidechannel_str) in to_encrypt {
        let key = keys.resolve(app_id).await?;
        let aad = crate::encryption::aad::canonical_aad(app_id, collection, &col, row_pk.as_bytes());
        let ciphertext = crate::encryption::aead::encrypt(&key, &plaintext, &aad)?;
        sidechannel.insert(col.clone(), sidechannel_str);
        let obj = row.as_object_mut().expect("checked above");
        obj.insert(col.clone(), Value::Bytes(ciphertext));
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
        "bytes" => value
            .as_bytes()
            .map(|v| base64::engine::general_purpose::STANDARD.encode(v))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Decrypt every `t.encrypted(...)`-declared column on `row` in place.
///
/// `row_pk` is read from `row["id"]` and authenticated on every decrypt.
///
/// On a tag-verification failure (tampered ciphertext, wrong AAD,
/// wrong key) the function surfaces
/// `ValidationFailed { code: "encryption_aead_failed" }`.
pub async fn decrypt_row_on_read(
    keys: &KeyStore,
    app_id: &str,
    collection: &str,
    schema: &Value,
    row: &mut Value,
) -> Result<(), DbError> {
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
    let mut to_decrypt: Vec<(String, &'static str, Vec<u8>)> = Vec::new();
    for (col, def) in schema_obj.iter() {
        let Some(enc_meta) = def.get("encrypted").and_then(|v| v.as_object()) else {
            continue;
        };
        let wraps = parse_wraps(enc_meta);

        // Which physical column carries this field's ciphertext, and whether
        // this read is allowed to decrypt it at all.
        //
        // For a MASKED encrypted field the ciphertext lives in the raw column,
        // which no statement this runtime issues carries any more: SELECT and
        // RETURNING both project the field's own column, which holds the mask.
        // (A `RETURNING *` write used to carry it; a WAL-decoded row still
        // does.) Decrypting is then
        // pointless AND unsafe: pointless because the mask pass overwrites the
        // slot with a sentinel, unsafe because the plaintext would sit in the
        // row while it happened. The query-hint unmask path does not need it
        // either; `dispatch_unmask_for_query` re-fetches the cell under its own
        // authorization check and audit row.
        //
        // So: an unmasked encrypted field decrypts from its own column, and a
        // masked one is left alone. The old gate also decrypted whenever the
        // row happened to carry the sibling key, which is how a write's
        // `RETURNING *` used to hand plaintext to the mask pass. That producer
        // is gone; this gate is not, because the WAL consumer is not.
        let masked = zeroship_data_sql::descriptors::effective_mask(def).is_some();
        if masked {
            continue;
        }

        let Some(value) = obj.get(col) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        if row_pk.is_empty() {
            return Err(DbError::validation(
                "encrypted_row_id_required",
                "encrypted values require a row identity",
            ));
        }
        let bytes = value
            .as_bytes()
            .ok_or_else(|| {
                DbError::internal(format!("encrypted column '{col}' requires native bytes"))
            })?
            .to_vec();
        to_decrypt.push((col.clone(), wraps, bytes));
    }

    for (col, wraps, blob) in to_decrypt {
        let key = keys.resolve(app_id).await?;
        let aad = crate::encryption::aad::canonical_aad(app_id, collection, &col, row_pk.as_bytes());
        let plaintext = Zeroizing::new(crate::encryption::aead::decrypt(&key, &blob, &aad)?);
        let value = deserialise_wrapped(&plaintext, wraps)?;
        let obj = row.as_object_mut().expect("checked above");
        obj.insert(col, value);
    }
    Ok(())
}

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
        "bytes" => value.as_bytes().map(<[u8]>::to_vec).ok_or_else(|| {
            DbError::validation(
                "encrypted_value_type_mismatch",
                "encrypted bytes require native bytes",
            )
        }),
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
                DbError::internal(format!("decrypted plaintext is not valid UTF-8: {e}"))
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
            // `zeroship_data_sql::value::Number::from_f64` returns None for NaN /
            // Infinity. Coerce to `null` for those — they're not
            // legitimate column values anyway.
            Ok(zeroship_data_sql::value::Number::from_f64(n).map_or(Value::Null, Value::Number))
        }
        "bytes" => Ok(Value::Bytes(bytes.to_vec())),
        other => Err(DbError::internal(format!(
            "encrypted wraps must be string/number/bytes, got '{other}'"
        ))),
    }
}

/// Pick the `wraps` field; default `"string"` for forward-compat with
/// `t.encrypted()` calls that omit it.
fn parse_wraps(enc_meta: &zeroship_data_sql::value::Map<String, Value>) -> &'static str {
    match enc_meta.get("wraps").and_then(|v| v.as_str()) {
        Some("string") => "string",
        Some("number") => "number",
        Some("bytes") => "bytes",
        _ => "string",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercise project-key resolution with explicitly bound test apps.
    fn test_key_store() -> KeyStore {
        use crate::encryption::{ProjectKeySource, SuppliedProjectKeys};
        let keys = std::rc::Rc::new(
            SuppliedProjectKeys::new()
                .with_hex("default", &"11".repeat(32))
                .expect("fixture root key must parse"),
        );
        keys.bind_app("app", "default").unwrap();
        keys.bind_app("app1", "default").unwrap();
        KeyStore::new(ProjectKeySource::supplied(keys))
    }

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
        let raw = [0xde, 0xad, 0xbe, 0xef];
        let v = Value::Bytes(raw.to_vec());
        let bytes = serialise_wrapped(&v, "bytes").unwrap();
        assert_eq!(bytes, raw);
        let back = deserialise_wrapped(&bytes, "bytes").unwrap();
        assert_eq!(back.as_bytes(), Some(raw.as_slice()));
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

    /// Writes round-trip only when the read uses the same row identity.
    #[test]
    fn write_then_read_round_trip_randomised() {
        let keys = test_key_store();

        let schema = zeroship_data_sql::value!({
            "ssn": { "type": "string", "encrypted": { "wraps": "string" } },
            "name": { "type": "string" },
        });
        let mut row =
            zeroship_data_sql::value!({ "id": "usr_01HX", "ssn": "123-45-6789", "name": "alice" });

        let rt = compio::runtime::Runtime::new().expect("compio runtime");
        rt.block_on(async {
            encrypt_row_on_write(&keys, "app1", "users", &schema, "usr_01HX", &mut row)
                .await
                .unwrap();
        });

        let obj = row.as_object().unwrap();
        let raw = obj["ssn"]
            .as_bytes()
            .expect("ciphertext is native bytes")
            .to_vec();
        assert_ne!(raw, b"123-45-6789");
        assert_eq!(obj["name"], "alice");
        assert!(!obj.contains_key("__zsbin__ssn"));
        let mut read_row = zeroship_data_sql::value!({ "id": "usr_01HX", "ssn": Value::Bytes(raw.clone()), "name": "alice" });

        rt.block_on(async {
            decrypt_row_on_read(&keys, "app1", "users", &schema, &mut read_row)
                .await
                .unwrap();
        });

        assert_eq!(read_row["ssn"].as_str(), Some("123-45-6789"));
        assert_eq!(read_row["name"].as_str(), Some("alice"));

        // Same ciphertext under a DIFFERENT row_pk must fail tag check
        // (Camp A defence — the row-swap attack surfaces as
        // `encryption_aead_failed`).
        let mut wrong_pk_row = zeroship_data_sql::value!({ "id": "usr_02HX", "ssn": Value::Bytes(raw.clone()), "name": "alice" });
        let err = rt.block_on(async {
            decrypt_row_on_read(&keys, "app1", "users", &schema, &mut wrong_pk_row).await
        });
        match err {
            Err(DbError::ValidationFailed { code, .. }) => {
                assert_eq!(code, "encryption_aead_failed");
            }
            other => panic!("expected ValidationFailed/encryption_aead_failed, got {other:?}"),
        }
    }

    #[test]
    fn decrypt_row_on_read_skips_masked_default_aliases() {
        let keys = test_key_store();

        let schema = zeroship_data_sql::value!({
            "contactEmail": {
                "type": "string",
                "encrypted": { "wraps": "string" },
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let mut read_row = zeroship_data_sql::value!({
            "id": "usr_01HX",
            "contactEmail": "a***@example.com"
        });

        let rt = compio::runtime::Runtime::new().expect("compio runtime");
        rt.block_on(async {
            decrypt_row_on_read(&keys, "app1", "users", &schema, &mut read_row)
                .await
                .unwrap();
        });

        assert_eq!(read_row["contactEmail"].as_str(), Some("a***@example.com"));
    }

    #[compio::test]
    async fn decrypt_row_on_read_respects_implicit_full_mask() {
        let keys = KeyStore::new(crate::encryption::ProjectKeySource::supplied(
            std::rc::Rc::new(crate::encryption::SuppliedProjectKeys::new()),
        ));
        let schema = zeroship_data_sql::value!({
            "secret": {"type":"string", "encrypted":{}, "mask":{"classification":"pii"}}
        });
        let mut row = zeroship_data_sql::value!({"id":"row", "secret":"***"});
        decrypt_row_on_read(&keys, "app", "records", &schema, &mut row)
            .await
            .unwrap();
        assert_eq!(row["secret"].as_str(), Some("***"));
    }

    /// Equal plaintexts produce different ciphertexts across rows.
    #[test]
    fn same_plaintext_yields_distinct_ciphertext() {
        let keys = test_key_store();

        let schema = zeroship_data_sql::value!({
            "ssn": { "type": "string", "encrypted": { "wraps": "string" } },
        });

        let mut row_a = zeroship_data_sql::value!({ "id": "usr_a", "ssn": "shared" });
        let mut row_b = zeroship_data_sql::value!({ "id": "usr_b", "ssn": "shared" });

        let rt = compio::runtime::Runtime::new().expect("compio runtime");
        rt.block_on(async {
            encrypt_row_on_write(&keys, "app1", "users", &schema, "usr_a", &mut row_a)
                .await
                .unwrap();
            encrypt_row_on_write(&keys, "app1", "users", &schema, "usr_b", &mut row_b)
                .await
                .unwrap();
        });

        assert_ne!(row_a["ssn"], row_b["ssn"]);
    }
}
