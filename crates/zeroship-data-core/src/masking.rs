//! The mask transforms: one `MaskKind` applied to one plaintext string.
//!
//! # Why this is the domain tier and not the engine
//!
//! `apply_mask_kind` and its per-kind helpers are pure, total functions over
//! `(MaskKind, &str)` - no schema, no row, no connection, no error type. They
//! came out of `zeroship-plugin-db`'s `crud/mask_pass.rs` on 2026-09-03 because
//! TWO tiers call them:
//!
//! * the ENGINE write and read passes (`crud::mask_pass::apply_mask_on_write`
//!   derives the stored mask and `crud::mask_pass::wrap_row_on_read`
//!   re-applies it), and
//! * [`crate::read_set`], which lowers a filter operand on a masked column to
//!   its masked form so the broker compares mask against mask.
//!
//! `read_set` is a domain module, so leaving the transform in the engine would
//! have made this crate depend on the crate that depends on it - a Cargo cycle,
//! unbuildable. The rule is the crate's standing one: a primitive two tiers
//! both use belongs BELOW both.
//!
//! # What did NOT come with it
//!
//! `parse_mask_kind` stayed in the engine. It returns `DbError` on an unknown
//! wire spelling, which makes it a schema-payload parser rather than a
//! transform; its one caller is `apply_mask_on_write`.

use zeroship_data_query_builder::catalog::MaskKind;

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
pub fn apply_mask_kind(kind: MaskKind, plaintext: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
