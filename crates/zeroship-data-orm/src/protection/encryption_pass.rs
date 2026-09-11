//! Encrypt and decrypt declared fields using native ciphertext buffers.
//! Randomised encryption authenticates the row identity as well as collection
//! and column, so moving ciphertext to another row fails authentication.
//! Mask derivation receives protected plaintext before encryption replaces it.

use zeroize::Zeroizing;
use zeroship_data_sql::value::Value;

use crate::encryption::{plaintext::PlaintextType, KeyStore};
use zeroship_data_orm::error::DbError;

/// A logical field's encoded plaintext and mask input, staged before key lookup.
type PendingEncryption = (String, Zeroizing<Vec<u8>>, Zeroizing<String>);

/// Encrypt declared fields in place using the supplied row identity.
/// Mask inputs are discarded when the caller does not need mask derivation.
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

/// Encrypt logical field values and retain plaintext for mask derivation.
///
/// The sidechannel holds text for strings and numbers, and base64 for bytes.
/// The mask pass uses it before write relocation places the ciphertext and mask
/// according to the runtime descriptor's storage mapping. This pass does not
/// choose physical column names. Absent and null fields remain untouched.
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

    // Stage native plaintext and mask input before async key lookup. Physical
    // placement belongs to the later write relocation stage.
    let mut to_encrypt: Vec<PendingEncryption> = Vec::new();
    for (col, def) in schema_obj.iter() {
        let Some(plaintext_type) = PlaintextType::from_field(def)? else {
            continue;
        };

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
        let plaintext = Zeroizing::new(plaintext_type.encode(value)?);
        let sidechannel_str = Zeroizing::new(plaintext_type.mask_text(&plaintext));
        to_encrypt.push((col.clone(), plaintext, sidechannel_str));
    }

    for (col, plaintext, sidechannel_str) in to_encrypt {
        let key = keys.resolve(app_id).await?;
        let aad =
            crate::encryption::aad::canonical_aad(app_id, collection, &col, row_pk.as_bytes());
        let ciphertext = crate::encryption::aead::encrypt(&key, &plaintext, &aad)?;
        sidechannel.insert(col.clone(), sidechannel_str);
        let obj = row.as_object_mut().expect("checked above");
        obj.insert(col.clone(), Value::Bytes(ciphertext));
    }
    Ok(())
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
    let mut to_decrypt: Vec<(String, PlaintextType, Vec<u8>)> = Vec::new();
    for (col, def) in schema_obj.iter() {
        let Some(plaintext_type) = PlaintextType::from_field(def)? else {
            continue;
        };

        // Default reads of masked fields contain display values, selected by
        // the runtime descriptor. Authorized unmasking fetches storage.rawColumn
        // separately and records the audit. Only unmasked fields decrypt here.
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
        to_decrypt.push((col.clone(), plaintext_type, bytes));
    }

    for (col, plaintext_type, blob) in to_decrypt {
        let key = keys.resolve(app_id).await?;
        let aad =
            crate::encryption::aad::canonical_aad(app_id, collection, &col, row_pk.as_bytes());
        let plaintext = Zeroizing::new(crate::encryption::aead::decrypt(&key, &blob, &aad)?);
        let value = plaintext_type.decode(&plaintext)?;
        let obj = row.as_object_mut().expect("checked above");
        obj.insert(col, value);
    }
    Ok(())
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
                .expect("fixture project key must parse"),
        );
        keys.bind_app("app", "default").unwrap();
        keys.bind_app("app1", "default").unwrap();
        KeyStore::new(ProjectKeySource::supplied(keys))
    }

    /// Writes round-trip only when the read uses the same row identity.
    #[test]
    fn write_then_read_round_trip_randomised() {
        let keys = test_key_store();

        let schema = zeroship_data_sql::value!({
            "ssn": { "type": "string", "encrypted": true },
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
    fn decrypt_row_on_read_skips_masked_display_values() {
        let keys = test_key_store();

        let schema = zeroship_data_sql::value!({
            "contactEmail": {
                "type": "string",
                "encrypted": true,
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
            "secret": {"type":"string", "encrypted":true, "mask":{"classification":"pii"}}
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
            "ssn": { "type": "string", "encrypted": true },
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
