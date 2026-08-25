//! Quoting for values this driver interpolates into SQL text.
//!
//! Every such value goes through here. The driver almost never builds SQL -
//! parameters travel as Bind values, out of band, where quoting cannot apply
//! - but a handful of statements name a thing rather than pass a value, and
//! an identifier can never be a parameter. `SAVEPOINT $1` is not valid SQL.
//! Those sites are: savepoint names ([`crate::transaction`]) and the slot and
//! publication names in `START_REPLICATION` ([`crate::replication`]).
//!
//! This module exists because the second group did not use the first group's
//! quoting. `quote_identifier` lived privately in `transaction.rs`, and
//! `replication.rs` interpolated caller strings raw with a doc comment saying
//! the caller should sanitise them. One gate, honoured at every site but one,
//! is the shape that hides longest: the sites that DO quote are evidence the
//! rule is understood, so nobody re-reads the one that doesn't.

/// Render `identifier` as a quoted SQL identifier.
///
/// Doubling `"` is the whole of the escape rule for a quoted identifier -
/// there is no backslash escape inside one, so a doubled quote cannot be
/// re-interpreted the way a backslash can.
///
/// Quoting is unconditional. A rule for deciding when a name "needs" quoting
/// has to know PostgreSQL's full unquoted-identifier grammar AND its
/// case-folding, and being wrong in the permissive direction is silent: an
/// unquoted `Orders` folds to `orders` and finds a different object, or none.
/// Quoting a name that did not need it changes nothing.
pub(crate) fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Render `value` as a complete SQL string literal, quotes included.
///
/// Returns the WHOLE literal rather than a body the caller wraps, because
/// which quoting is correct depends on the content: a value containing a
/// backslash needs escape-string syntax, and a caller that supplied its own
/// `'` could not know that.
///
/// # Why this does not just double the quote
///
/// Doubling `'` is sufficient only while `standard_conforming_strings` is
/// `on`. That has been the default since 9.1 and this driver never turns it
/// off - but the driver does not OWN the setting. A session can `SET` it, and
/// the server accepts that; a DSN can carry
/// `options=-c standard_conforming_strings=off`; an operator can change the
/// default. Relying on it made correctness depend on ambient state nothing
/// here checks.
///
/// MEASURED against PostgreSQL 16.14 with the setting `off`, using the old
/// body-only form:
///
/// ```text
/// 'back\slash'      -> backslash        (the \s was eaten as an escape)
/// 'quote\'injected' -> syntax error     (the \' closed the literal early)
/// ```
///
/// The second is the injection this module exists to prevent. So the rule is
/// now PostgreSQL's own, taken from what `quote_literal()` emits: plain
/// `'...'` when there is no backslash, and `E'...'` with both `'` and `\`
/// doubled when there is. `E''` syntax means the same thing under either
/// setting.
pub(crate) fn quote_literal(value: &str) -> String {
    if value.contains('\\') {
        // `E'...'`: backslash escapes are ACTIVE here by definition, so the
        // backslashes themselves have to be doubled as well as the quotes.
        format!("E'{}'", value.replace('\\', r"\\").replace('\'', "''"))
    } else {
        format!("'{}'", value.replace('\'', "''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_inside_an_identifier_is_doubled_not_dropped() {
        assert_eq!(quote_identifier(r#"a"b"#), r#""a""b""#);
    }

    #[test]
    fn an_identifier_that_needs_no_quoting_is_quoted_anyway() {
        // The mixed-case and comma cases below are why: unquoted, the server
        // would fold or split them. Quoting the plain case too means one rule
        // rather than a predicate that has to be right about the grammar.
        assert_eq!(quote_identifier("plain"), "\"plain\"");
        assert_eq!(quote_identifier("Orders"), "\"Orders\"");
        assert_eq!(quote_identifier("eu,us"), "\"eu,us\"");
    }

    #[test]
    fn a_quote_inside_a_literal_is_doubled_not_dropped() {
        assert_eq!(quote_literal("it's"), "'it''s'");
    }

    /// A value with no backslash needs no escape-string syntax, and gets the
    /// plain form - which is what `quote_literal()` on the server produces
    /// too, measured.
    #[test]
    fn a_literal_without_a_backslash_stays_plain() {
        assert_eq!(quote_literal("plain"), "'plain'");
        assert_eq!(quote_literal(""), "''");
    }

    /// THE ONE THAT WAS WRONG. A backslash must not depend on
    /// `standard_conforming_strings`.
    ///
    /// MEASURED against PostgreSQL 16.14: `SELECT quote_literal('back\slash')`
    /// returns `E'back\\slash'`, and with `standard_conforming_strings = off`
    /// the old plain form `'back\slash'` came back as `backslash` - the `\s`
    /// eaten as an escape. Worse, `\'` inside a plain literal terminated it
    /// early, which is the injection this module exists to prevent.
    #[test]
    fn a_backslash_forces_escape_string_syntax() {
        assert_eq!(quote_literal(r"a\b"), r"E'a\\b'");
        assert_eq!(quote_literal(r"back\slash"), r"E'back\\slash'");
    }

    /// Both escapes at once, which is the shape an attacker would reach for:
    /// under the old form `\'` closed the literal.
    #[test]
    fn a_backslash_and_a_quote_are_both_escaped() {
        assert_eq!(quote_literal(r"quote\'injected"), r"E'quote\\''injected'");
    }
}
