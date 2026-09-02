//! Mask transforms + the write-side physical placement of a masked column.
//!
//! The masked representation is computed at write time and stored beside the
//! real value, so a default read touches no key material ("Path B" in
//! `docs/archive/sensitive-field-masking.md`, resolved 2026-05-24; Path A -
//! computing the mask on read - was rejected there for key-scope reasons, and
//! the name is worth keeping because the trade-off analysis is recorded under
//! it).
//!
//! # Which column holds what
//!
//! The field's OWN column (`ssn`) holds the **mask**; the sibling
//! `__zs_raw__ssn` holds the **real value**. That is the 2026-08-28 storage
//! flip and it is the reason this module has a relocation stage rather than a
//! second-column-insert stage.
//!
//! It used to be the other way round, and the consequence was a filter oracle:
//! the projection substituted `"ssn_masked" AS "ssn"` but the WHERE builder
//! takes no schema hint and could not, so `find({ ssn: { $gt: v } })` compared
//! against plaintext. The caller never saw a value and did not need to - the
//! set of matching rows is the answer, and repeated probes binary-search it
//! with no authorization check on the path and no audit row written.
//!
//! # The stage order this depends on
//!
//! [`apply_mask_on_write`] COMPUTES the masks and returns them; it does not
//! touch the row. [`relocate_masked_columns`] performs the physical placement
//! and runs LAST, after the encryption, SQLite-binary and plain-bytes passes.
//!
//! The split is load-bearing. The flip makes two passes want to write the same
//! key: the encryption pass replaces `row["ssn"]` with ciphertext and the mask
//! wants `row["ssn"]` to be the mask, so the authoritative value has to move.
//! For an encrypted field the value the relocation moves is ciphertext; for a
//! mask-only field it is plaintext; for `t.bytes().mask()` the bytes pass has
//! to see the real value under the logical key before anything moves. One
//! stage that runs after all of them, MOVES whatever it finds rather than
//! recomputing it, and never conditions the move on the sidechannel, is the
//! only shape where none of those combinations loses data.
//!
//! ## Wiring into the CRUD dispatch
//!
//! Called from `crud::dispatch_insert` / `dispatch_update_one`
//! **AFTER** [`crate::crud::encryption_pass::encrypt_row_on_write`]
//! and **BEFORE** the `query::build_*` call. The encryption pass
//! populates a [`MaskPlaintextSidechannel`] (a
//! `HashMap<String, Zeroizing<String>>`)
//! before swapping the plaintext for ciphertext so the mask pass can
//! read the plaintext without re-decrypting.
//!
//! For non-encrypted-but-masked columns (`t.string().mask({...})` with
//! no `.encrypted()`), the sidechannel is empty for that column; the
//! mask pass reads the plaintext directly from `row[col]`.
//!
//! ## Mask transforms (all 8 named built-ins)
//!
//! | Kind        | Sample input         | Sample output           |
//! |-------------|----------------------|-------------------------|
//! | `Full`      | `"123-45-6789"`      | `"***"`                 |
//! | `Last4`     | `"123-45-6789"`      | `"***-**-6789"`         |
//! | `First4`    | `"4111-1111-1111-1234"` | `"4111-****-****-****"` |
//! | `Email`     | `"alice@example.com"` | `"a***@example.com"`    |
//! | `Name`      | `"Alice Anderson"`   | `"A. A***"`             |
//! | `DateYear`  | `"1985-04-12"`       | `"1985-**-**"`          |
//! | `DateDecade`| `"1985-04-12"`       | `"198?-**-**"`          |
//! | `None`      | (skipped — no sibling) |                       |
//!
//! Per design Q-MASK-L: `null` passes through as `null` (no mask
//! written); empty string `""` → `""`.
//!
//! ## Why no creator-supplied JS mask functions
//!
//! An AI-generated `mask: v => v` would silently return plaintext,
//! defeating the entire fence. Only named built-in strategies — adding
//! a new strategy is a platform PR, not creator config.

use std::collections::HashMap;

use serde_json::Value;
use zeroize::Zeroizing;

use crate::diff::MaskKind;
use zeroship_data_core::error::DbError;

/// Plaintext sidechannel populated by the encryption pass and consumed
/// by the mask pass. Key = column name; value = the raw plaintext
/// string (UTF-8 decode of the wrapped primitive's wire bytes),
/// wrapped in [`Zeroizing`] so the transient plaintext is scrubbed on
/// drop.
///
/// The encryption pass populates this BEFORE replacing the row value
/// with the base64 ciphertext, so the mask pass can derive the sibling
/// column's masked output without a redundant decrypt round-trip.
pub(crate) type MaskPlaintextSidechannel = HashMap<String, Zeroizing<String>>;

/// The masks [`apply_mask_on_write`] derived, keyed by LOGICAL field name.
///
/// Held rather than written straight into the row so that exactly one stage -
/// [`relocate_masked_columns`] - owns physical placement. See the module doc.
pub(crate) type DerivedMasks = Vec<(String, String)>;

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
pub(crate) fn apply_mask_on_write(
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
        let Some(mask_meta) = def.get("mask").and_then(|v| v.as_object()) else {
            continue;
        };
        let kind_str = mask_meta.get("kind").and_then(|v| v.as_str()).unwrap_or("full");
        if kind_str == "none" {
            continue;
        }
        let kind = parse_mask_kind(kind_str)?;

        // Plaintext source: sidechannel first (encrypted column case),
        // then the row's current value (non-encrypted case). The row
        // value MAY already be the base64 ciphertext if the encryption
        // pass ran first AND the sidechannel was not populated — that
        // would be a contract violation, so we prefer the sidechannel.
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
            derived.push((col.clone(), apply_mask_kind(kind, pt.as_str())));
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
/// 1. `row[__zs_raw__<field>] = row[<field>]` (the real value, whatever stage
///    produced it - plaintext, ciphertext, or a base64 bytes envelope);
/// 2. `row[<field>] = mask`;
/// 3. the binary-bind marker moves with the value: `__zsbin__<field>` becomes
///    `__zsbin__<__zs_raw__<field>>`, because it is the RAW column that wants
///    `decode($N, 'base64')::bytea` and the masked column is plain `TEXT`.
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
pub(crate) fn relocate_masked_columns(masks: &DerivedMasks, row: &mut Value) -> Result<(), DbError> {
    if masks.is_empty() {
        return Ok(());
    }
    let Some(obj) = row.as_object_mut() else {
        return Err(DbError::internal(
            "relocate_masked_columns: row must be a JSON object",
        ));
    };
    for (field, masked) in masks {
        let raw_col = crate::query::raw_column_name(field);
        if let Some(value) = obj.remove(field.as_str()) {
            obj.insert(raw_col.clone(), value);
        }
        if let Some(marker) = obj.remove(&format!("__zsbin__{field}")) {
            obj.insert(format!("__zsbin__{raw_col}"), marker);
        }
        obj.insert(field.clone(), Value::String(masked.clone()));
    }
    Ok(())
}

/// Apply a single mask transform.
///
/// Pure function; total over `(MaskKind, &str)`. Edge cases:
/// - Empty string → empty string (`""` in, `""` out) for every kind.
/// - Strings shorter than the preserved-tail / preserved-head length
///   for `Last4` / `First4` get padded by repeated stars (effectively
///   `"***"` for very short inputs).
/// - Non-email-shaped strings for `Email` mask: fall back to `Full`
///   redaction (`"***"`) so we never leak the first character of a
///   string that has no `@`.
/// - Non-date-shaped strings for `DateYear` / `DateDecade`: fall back
///   to `"***"` (same reasoning — don't leak a numeric prefix from
///   what was supposed to be `YYYY-MM-DD`).
pub(crate) fn apply_mask_kind(kind: MaskKind, plaintext: &str) -> String {
    match kind {
        MaskKind::Full => "***".to_string(),
        MaskKind::Last4 => mask_last_n(plaintext, 4),
        MaskKind::First4 => mask_first_n(plaintext, 4),
        MaskKind::Email => mask_email(plaintext),
        MaskKind::Name => mask_name(plaintext),
        MaskKind::DateYear => mask_date_year(plaintext),
        MaskKind::DateDecade => mask_date_decade(plaintext),
        // `None` is filtered out before reaching apply_mask_kind by
        // `apply_mask_on_write`; preserve-plaintext is the documented
        // semantic if a caller ever invokes this directly.
        MaskKind::None => plaintext.to_string(),
    }
}

/// Parse a mask kind string from the schema-wire format. Mirrors the
/// `MaskKind` discriminator the SDK emits in `def.mask.kind`.
fn parse_mask_kind(s: &str) -> Result<MaskKind, DbError> {
    Ok(match s {
        "full" => MaskKind::Full,
        "last4" => MaskKind::Last4,
        "first4" => MaskKind::First4,
        "email" => MaskKind::Email,
        "name" => MaskKind::Name,
        "dateYear" | "date-year" => MaskKind::DateYear,
        "dateDecade" | "date-decade" => MaskKind::DateDecade,
        "none" => MaskKind::None,
        other => {
            return Err(DbError::internal(format!(
                "apply_mask_on_write: unknown mask kind '{other}'"
            )));
        }
    })
}

/// `mask_last_n("123-45-6789", 4)` → `"***-**-6789"`.
///
/// Preserves the trailing `n` ALPHANUMERIC characters of `plaintext`;
/// every other alphanumeric character is replaced by `*`. Non-
/// alphanumeric characters (separators like `-`, `/`, space) are
/// preserved verbatim so the visual structure of the output mirrors
/// the input (the SSN dashes survive in the example above).
fn mask_last_n(plaintext: &str, n: usize) -> String {
    if plaintext.is_empty() {
        return String::new();
    }
    // First pass: count alphanumerics so we know how many to leave
    // visible vs. mask. Counting up front avoids two-pass collection
    // into a Vec.
    let alnum_total = plaintext.chars().filter(|c| c.is_ascii_alphanumeric()).count();
    let keep_tail = alnum_total.saturating_sub(n);

    let mut out = String::with_capacity(plaintext.len());
    let mut alnum_seen = 0usize;
    for c in plaintext.chars() {
        if c.is_ascii_alphanumeric() {
            if alnum_seen < keep_tail {
                out.push('*');
            } else {
                out.push(c);
            }
            alnum_seen += 1;
        } else {
            out.push(c);
        }
    }
    out
}

/// `mask_first_n("4111-1111-1111-1234", 4)` → `"4111-****-****-****"`.
///
/// Preserves the leading `n` alphanumerics; masks the rest with `*`.
/// Same non-alphanumeric-preservation rule as [`mask_last_n`].
fn mask_first_n(plaintext: &str, n: usize) -> String {
    if plaintext.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(plaintext.len());
    let mut alnum_seen = 0usize;
    for c in plaintext.chars() {
        if c.is_ascii_alphanumeric() {
            if alnum_seen < n {
                out.push(c);
            } else {
                out.push('*');
            }
            alnum_seen += 1;
        } else {
            out.push(c);
        }
    }
    out
}

/// `mask_email("alice@example.com")` → `"a***@example.com"`.
///
/// Preserves the first character of the local part + the `@` + the
/// entire domain. If the input has no `@` (or starts with `@`), falls
/// back to full redaction (`"***"`) to avoid leaking a non-email value
/// the creator mistakenly tagged as `email`.
fn mask_email(plaintext: &str) -> String {
    if plaintext.is_empty() {
        return String::new();
    }
    let Some(at_idx) = plaintext.find('@') else {
        // Defensive fallback: not email-shaped, full-redact.
        return "***".to_string();
    };
    if at_idx == 0 {
        // No local part — refuse to leak the domain alone.
        return "***".to_string();
    }
    let (local, domain) = plaintext.split_at(at_idx);
    let first_char = local.chars().next().unwrap_or('*');
    format!("{first_char}***{domain}")
}

/// `mask_name("Alice Anderson")` → `"A. A***"`.
///
/// Preserves first initial of each whitespace-separated token, joined
/// by `". "`. The last token's surname keeps its initial + collapses
/// the remainder to `***`. Single-token inputs become `"X***"`.
///
/// Empty / whitespace-only → empty string.
fn mask_name(plaintext: &str) -> String {
    let tokens: Vec<&str> = plaintext.split_whitespace().collect();
    if tokens.is_empty() {
        return String::new();
    }
    if tokens.len() == 1 {
        let first_char = tokens[0].chars().next().unwrap_or('*');
        return format!("{first_char}***");
    }
    // Initials for every token except the last; last token: initial + ***.
    let mut parts: Vec<String> = Vec::with_capacity(tokens.len());
    for (i, tok) in tokens.iter().enumerate() {
        let first_char = tok.chars().next().unwrap_or('*');
        if i + 1 == tokens.len() {
            parts.push(format!("{first_char}***"));
        } else {
            parts.push(format!("{first_char}."));
        }
    }
    parts.join(" ")
}

/// `mask_date_year("1985-04-12")` → `"1985-**-**"`.
///
/// Accepts the ISO `YYYY-MM-DD` shape. Anything else falls back to
/// `"***"` (don't leak a numeric prefix from a non-date input).
fn mask_date_year(plaintext: &str) -> String {
    if plaintext.is_empty() {
        return String::new();
    }
    let bytes = plaintext.as_bytes();
    if bytes.len() < 10
        || !bytes[0..4].iter().all(|b| b.is_ascii_digit())
        || bytes[4] != b'-'
    {
        return "***".to_string();
    }
    let year = &plaintext[0..4];
    format!("{year}-**-**")
}

/// `mask_date_decade("1985-04-12")` → `"198?-**-**"`.
///
/// Preserves only the first three digits of the year; the units digit
/// becomes `?`. Same fall-back as [`mask_date_year`] for non-date
/// inputs.
///
/// **Idempotent** (SEC-4): the 4th year position may already be the `?`
/// sentinel from a prior masking, so re-masking the function's own
/// output (`"198?-**-**"`) is a no-op rather than collapsing to `"***"`.
/// `wrap_row_on_read` re-applies the mask transform defensively, and the
/// aliased-SELECT read path feeds it the already-masked string; this
/// keeps that legitimate value intact while still redacting plaintext.
fn mask_date_decade(plaintext: &str) -> String {
    if plaintext.is_empty() {
        return String::new();
    }
    let bytes = plaintext.as_bytes();
    let year_ok = bytes.len() >= 10
        && bytes[0..3].iter().all(|b| b.is_ascii_digit())
        && (bytes[3].is_ascii_digit() || bytes[3] == b'?')
        && bytes[4] == b'-';
    if !year_ok {
        return "***".to_string();
    }
    let decade = &plaintext[0..3];
    format!("{decade}?-**-**")
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
/// (pinned by `remasking_a_mask_is_a_no_op_for_every_kind`). What it buys is
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
pub(crate) fn wrap_row_on_read(
    schema: &Value,
    collection: &str,
    row: &mut Value,
) -> Result<(), DbError> {
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
        let Some(mask_meta) = def.get("mask").and_then(|v| v.as_object()) else {
            continue;
        };
        let kind = mask_meta.get("kind").and_then(|v| v.as_str()).unwrap_or("full");
        if kind == "none" {
            continue;
        }
        let classification = mask_meta
            .get("classification")
            .and_then(|v| v.as_str())
            .unwrap_or("pii")
            .to_string();
        let kind = parse_mask_kind(kind).unwrap_or(MaskKind::Full);

        // The field's own slot holds the mask. Re-apply the transform rather
        // than trusting it: we cannot distinguish "already masked" from "a
        // builder lowered this to the raw column" by looking at the value, and
        // re-masking a mask is a no-op for every built-in kind.
        let masked_value: Option<String> =
            obj.get(col).and_then(|v| v.as_str()).map(|s| apply_mask_kind(kind, s));

        // The raw column rides out of a WAL-decoded row (and out of every
        // `RETURNING *`, back when the write builders starred). Strip it here -
        // `read_pipeline`'s surface stage would too, but this pass runs first
        // and the sentinel it writes must not sit beside the value it hides.
        let raw_key = crate::query::raw_column_name(col);
        if obj.contains_key(&raw_key) {
            to_strip.push(raw_key);
        }

        let Some(masked) = masked_value else {
            // The field's column is absent (e.g. a narrowed projection) or is
            // not a string (NULL) — nothing to wrap.
            continue;
        };

        to_wrap.push((col.clone(), masked, classification));
    }

    for stripped in to_strip {
        obj.remove(&stripped);
    }

    for (col, masked, classification) in to_wrap {
        let repr = serde_json::json!({
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
pub(crate) fn mask_sentinel_signature() -> &'static str {
    use std::sync::OnceLock;
    static SIG: OnceLock<String> = OnceLock::new();
    SIG.get_or_init(|| {
        use aes_gcm::{aead::OsRng, AeadCore, Aes256Gcm};
        // Two 12-byte GCM nonces → 24 bytes of OS entropy, hex-encoded.
        let a = Aes256Gcm::generate_nonce(&mut OsRng);
        let b = Aes256Gcm::generate_nonce(&mut OsRng);
        a.iter().chain(b.iter()).map(|x| format!("{x:02x}")).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plaintext_sidechannel_stores_zeroizing_strings() {
        let mut plaintexts = MaskPlaintextSidechannel::new();
        plaintexts.insert(
            "ssn".to_string(),
            Zeroizing::new("123-45-6789".to_string()),
        );
        let got: &Zeroizing<String> = plaintexts.get("ssn").expect("sidechannel entry");
        assert_eq!(got.as_str(), "123-45-6789");
    }

    // -----------------------------------------------------------------
    // apply_mask_kind: per-kind transform unit tests
    // -----------------------------------------------------------------

    #[test]
    fn full_mask_redacts_everything() {
        assert_eq!(apply_mask_kind(MaskKind::Full, "anything"), "***");
        assert_eq!(apply_mask_kind(MaskKind::Full, ""), "***");
    }

    #[test]
    fn last4_preserves_last_four_alphanumerics() {
        assert_eq!(apply_mask_kind(MaskKind::Last4, "123-45-6789"), "***-**-6789");
        assert_eq!(apply_mask_kind(MaskKind::Last4, "1234567890"), "******7890");
    }

    #[test]
    fn last4_handles_short_inputs() {
        // Fewer alphanumerics than the tail length → keep what we have,
        // no stars.
        assert_eq!(apply_mask_kind(MaskKind::Last4, "abc"), "abc");
        assert_eq!(apply_mask_kind(MaskKind::Last4, ""), "");
    }

    #[test]
    fn first4_preserves_first_four_alphanumerics() {
        assert_eq!(
            apply_mask_kind(MaskKind::First4, "4111-1111-1111-1234"),
            "4111-****-****-****"
        );
        assert_eq!(apply_mask_kind(MaskKind::First4, "abcdefgh"), "abcd****");
    }

    #[test]
    fn first4_handles_short_inputs() {
        assert_eq!(apply_mask_kind(MaskKind::First4, "ab"), "ab");
        assert_eq!(apply_mask_kind(MaskKind::First4, ""), "");
    }

    #[test]
    fn email_preserves_first_char_and_domain() {
        assert_eq!(
            apply_mask_kind(MaskKind::Email, "alice@example.com"),
            "a***@example.com"
        );
        assert_eq!(
            apply_mask_kind(MaskKind::Email, "bob@sub.domain.co"),
            "b***@sub.domain.co"
        );
    }

    #[test]
    fn email_falls_back_to_full_for_non_email() {
        // No `@` → full redact (don't leak first char of a non-email).
        assert_eq!(apply_mask_kind(MaskKind::Email, "not-an-email"), "***");
        // Local part empty.
        assert_eq!(apply_mask_kind(MaskKind::Email, "@example.com"), "***");
        assert_eq!(apply_mask_kind(MaskKind::Email, ""), "");
    }

    #[test]
    fn name_mask_uses_initials_and_collapsed_surname() {
        assert_eq!(apply_mask_kind(MaskKind::Name, "Alice Anderson"), "A. A***");
        assert_eq!(
            apply_mask_kind(MaskKind::Name, "Alice Beatrice Carmichael"),
            "A. B. C***"
        );
    }

    #[test]
    fn name_mask_single_token() {
        assert_eq!(apply_mask_kind(MaskKind::Name, "Madonna"), "M***");
    }

    #[test]
    fn name_mask_handles_empty_and_whitespace() {
        assert_eq!(apply_mask_kind(MaskKind::Name, ""), "");
        assert_eq!(apply_mask_kind(MaskKind::Name, "   "), "");
    }

    #[test]
    fn date_year_preserves_year() {
        assert_eq!(apply_mask_kind(MaskKind::DateYear, "1985-04-12"), "1985-**-**");
        assert_eq!(apply_mask_kind(MaskKind::DateYear, "2026-12-31"), "2026-**-**");
    }

    #[test]
    fn date_year_falls_back_for_non_date() {
        assert_eq!(apply_mask_kind(MaskKind::DateYear, "not-a-date"), "***");
        assert_eq!(apply_mask_kind(MaskKind::DateYear, "12345"), "***");
        assert_eq!(apply_mask_kind(MaskKind::DateYear, ""), "");
    }

    #[test]
    fn date_decade_preserves_decade() {
        assert_eq!(
            apply_mask_kind(MaskKind::DateDecade, "1985-04-12"),
            "198?-**-**"
        );
        assert_eq!(
            apply_mask_kind(MaskKind::DateDecade, "2026-12-31"),
            "202?-**-**"
        );
    }

    #[test]
    fn date_decade_falls_back_for_non_date() {
        assert_eq!(apply_mask_kind(MaskKind::DateDecade, "garbage"), "***");
        assert_eq!(apply_mask_kind(MaskKind::DateDecade, ""), "");
    }

    #[test]
    fn none_is_passthrough() {
        // `None` is filtered out before reaching apply_mask_kind by
        // `apply_mask_on_write`, but if a caller invokes it directly
        // it returns the plaintext untouched.
        assert_eq!(apply_mask_kind(MaskKind::None, "secret"), "secret");
    }

    // -----------------------------------------------------------------
    // apply_mask_on_write: integration with row + sidechannel
    // -----------------------------------------------------------------

    /// Run the two write stages in the order `WriteStages::apply_to_doc` does:
    /// derive the masks, then place them.
    fn derive_and_relocate(
        schema: &Value,
        plaintexts: &MaskPlaintextSidechannel,
        row: &mut Value,
    ) {
        let masks = apply_mask_on_write(schema, plaintexts, row).expect("derive masks");
        relocate_masked_columns(&masks, row).expect("relocate");
    }

    #[test]
    fn a_masked_encrypted_write_puts_the_mask_in_the_logical_column_and_moves_the_ciphertext() {
        // Encrypted column: plaintext arrives via the sidechannel.
        let schema = json!({
            "ssn": {
                "type": "string",
                "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" },
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        // Row's `ssn` is the base64 ciphertext (encryption pass ran first).
        let mut row = json!({
            "id": "usr_01",
            "ssn": "BASE64CIPHERTEXT",
            "__zsbin__ssn": true,
        });
        let mut plaintexts = MaskPlaintextSidechannel::new();
        plaintexts.insert(
            "ssn".to_string(),
            Zeroizing::new("123-45-6789".to_string()),
        );

        derive_and_relocate(&schema, &plaintexts, &mut row);

        let raw = crate::query::raw_column_name("ssn");
        let obj = row.as_object().unwrap();
        assert_eq!(obj.get("ssn").and_then(|v| v.as_str()), Some("***-**-6789"));
        assert_eq!(
            obj.get(&raw).and_then(|v| v.as_str()),
            Some("BASE64CIPHERTEXT"),
            "the authoritative value moves to the raw column",
        );
        // The binary-bind marker travels with the value: it is the RAW column
        // that wants `decode($N,'base64')::bytea`; the masked column is TEXT.
        assert!(obj.get("__zsbin__ssn").is_none(), "marker must not stay behind: {row}");
        assert_eq!(obj.get(&format!("__zsbin__{raw}")), Some(&json!(true)));
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
        let schema = json!({
            "ssn": {
                "type": "string",
                "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" },
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let mut row = json!({ "id": "usr_01", "ssn": "BASE64CIPHERTEXT" });
        let plaintexts = MaskPlaintextSidechannel::new();

        derive_and_relocate(&schema, &plaintexts, &mut row);

        assert_eq!(
            row[crate::query::raw_column_name("ssn")].as_str(),
            Some("BASE64CIPHERTEXT"),
            "the ciphertext must survive a write that returns success: {row}",
        );
    }

    #[test]
    fn a_mask_only_write_moves_the_plaintext_to_the_raw_column() {
        // Non-encrypted but masked column: plaintext stays in `row[col]`,
        // sidechannel has no entry.
        let schema = json!({
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let mut row = json!({ "id": "usr_01", "email": "alice@example.com" });
        let plaintexts = MaskPlaintextSidechannel::new();

        derive_and_relocate(&schema, &plaintexts, &mut row);

        let obj = row.as_object().unwrap();
        assert_eq!(
            obj.get("email").and_then(|v| v.as_str()),
            Some("a***@example.com"),
            "the field's own column holds the mask",
        );
        assert_eq!(
            obj.get(&crate::query::raw_column_name("email"))
                .and_then(|v| v.as_str()),
            Some("alice@example.com"),
        );
    }

    /// Re-masking a mask must be a no-op, for EVERY built-in kind.
    ///
    /// `wrap_row_on_read` re-applies the transform to the field's own column on
    /// every read, so idempotence went from "true on one path" to load-bearing
    /// everywhere. A kind that mangled its own output would corrupt every read
    /// of that column, silently and without touching the stored value.
    #[test]
    fn remasking_a_mask_is_a_no_op_for_every_kind() {
        let samples: &[(MaskKind, &[&str])] = &[
            (MaskKind::Full, &["123-45-6789", "", "a"]),
            (MaskKind::Last4, &["123-45-6789", "ab", ""]),
            (MaskKind::First4, &["4111-1111-1111-1234", "ab", ""]),
            (MaskKind::Email, &["alice@example.com", "no-at-sign", ""]),
            (MaskKind::Name, &["Alice Anderson", "Cher", ""]),
            (MaskKind::DateYear, &["1985-04-12", "not-a-date", ""]),
            (MaskKind::DateDecade, &["1985-04-12", "not-a-date", ""]),
        ];
        for (kind, inputs) in samples {
            for input in *inputs {
                let once = apply_mask_kind(*kind, input);
                let twice = apply_mask_kind(*kind, &once);
                assert_eq!(
                    once, twice,
                    "{kind:?} is not idempotent on {input:?}: {once:?} -> {twice:?}",
                );
            }
        }
    }

    #[test]
    fn apply_mask_on_write_skips_kind_none() {
        // Explicit opt-out: no sibling written.
        let schema = json!({
            "ssn": {
                "type": "string",
                "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" },
                "mask": { "kind": "none", "classification": "spi" }
            }
        });
        let mut row = json!({ "id": "usr_01", "ssn": "BASE64CT" });
        let mut plaintexts = MaskPlaintextSidechannel::new();
        plaintexts.insert(
            "ssn".to_string(),
            Zeroizing::new("123-45-6789".to_string()),
        );

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
        let schema = json!({
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let mut row = json!({ "id": "usr_01", "email": null });
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
        let schema = json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "name": { "type": "string" }
        });
        let mut row = json!({ "name": "alice" });
        let plaintexts = MaskPlaintextSidechannel::new();

        derive_and_relocate(&schema, &plaintexts, &mut row);

        let obj = row.as_object().unwrap();
        assert!(obj.get("ssn_masked").is_none());
    }

    #[test]
    fn apply_mask_on_write_handles_multiple_columns() {
        let schema = json!({
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
        let mut row = json!({
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
                obj.get(&crate::query::raw_column_name(field))
                    .and_then(|v| v.as_str()),
                Some(plaintext),
            );
        }
    }

    #[test]
    fn apply_mask_on_write_rejects_unknown_kind() {
        let schema = json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "absurdly-novel-kind", "classification": "spi" }
            }
        });
        let mut row = json!({ "ssn": "abc" });
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
        let schema = json!({
            "name": { "type": "string" },
            "age": { "type": "number" }
        });
        let mut row = json!({ "name": "alice", "age": 30 });
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
        let schema = json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "name": { "type": "string" }
        });
        let mut row = json!({
            "id": "usr_01",
            "ssn": "***-**-6789",
            "name": "alice"
        });

        wrap_row_on_read(&schema, "users", &mut row).unwrap();

        let obj = row.as_object().unwrap();
        let ssn = obj.get("ssn").and_then(|v| v.as_object()).unwrap();
        assert_eq!(ssn.get("sentinel").and_then(|v| v.as_str()), Some("__zsmask__"));
        assert_eq!(ssn.get("masked").and_then(|v| v.as_str()), Some("***-**-6789"));
        assert_eq!(ssn.get("classification").and_then(|v| v.as_str()), Some("spi"));
        let meta = ssn.get("_meta").and_then(|v| v.as_object()).unwrap();
        assert_eq!(meta.get("collection").and_then(|v| v.as_str()), Some("users"));
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
        let schema = json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let raw = crate::query::raw_column_name("ssn");
        let mut row = json!({
            "id": "usr_01",
            "ssn": "***-**-6789",
        });
        row[raw.clone()] = json!("123-45-6789");

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
        assert_eq!(ssn.get("masked").and_then(|v| v.as_str()), Some("***-**-6789"));
    }

    #[test]
    fn wrap_row_on_read_skips_kind_none() {
        // Opt-out: `kind: "none"` retains plaintext-on-read (the
        // decrypt-on-read path); no wrapping happens.
        let schema = json!({
            "ssn": {
                "type": "string",
                "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" },
                "mask": { "kind": "none", "classification": "spi" }
            }
        });
        let mut row = json!({
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
        let schema = json!({
            "email": {
                "type": "string",
                "mask": { "kind": "email" }
            }
        });
        let mut row = json!({
            "id": "usr_01",
            "email": "a***@example.com"
        });

        wrap_row_on_read(&schema, "users", &mut row).unwrap();

        let email = row.get("email").and_then(|v| v.as_object()).unwrap();
        assert_eq!(email.get("classification").and_then(|v| v.as_str()), Some("pii"));
    }

    #[test]
    fn wrap_row_on_read_handles_numeric_id() {
        // typed_id collections use string `id`, but legacy collections
        // can carry numeric PK — `row_pk` must stringify either.
        let schema = json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let mut row = json!({
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
        let schema = json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let mut row = json!({ "ssn": "***-**-6789" });

        wrap_row_on_read(&schema, "users", &mut row).unwrap();

        let ssn = row.get("ssn").and_then(|v| v.as_object()).unwrap();
        let meta = ssn.get("_meta").and_then(|v| v.as_object()).unwrap();
        assert_eq!(meta.get("row_pk").and_then(|v| v.as_str()), Some(""));
        assert_eq!(meta.get("collection").and_then(|v| v.as_str()), Some("users"));
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
        let schema = json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        // Parent holds plaintext; no sibling — the dangerous shape.
        let mut row = json!({ "id": "usr_01", "ssn": "123-45-6789" });

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
        let schema = json!({
            "name": { "type": "string" },
            "age": { "type": "number" }
        });
        let mut row = json!({ "id": "usr_01", "name": "alice", "age": 30 });
        let original = row.clone();

        wrap_row_on_read(&schema, "users", &mut row).unwrap();

        assert_eq!(row, original);
    }

    #[test]
    fn wrap_row_on_read_noop_when_row_not_object() {
        // Defensive: a `Value::Null` row passes through without error.
        let schema = json!({
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
        let schema = json!({
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
        let mut row = json!({
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
}
