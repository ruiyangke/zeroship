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

/// The SQL that ends a savepoint's scope: undo its work, then take the name
/// back off the server's savepoint stack.
///
/// `ROLLBACK TO SAVEPOINT` deliberately LEAVES the savepoint defined - that is
/// what makes "roll back to it again later" possible, and it is the documented
/// behaviour, not a quirk. But a `Transaction` whose `rollback` has been
/// called is finished: its Rust value is consumed and no later call can name
/// it. Leaving the name defined lets it outlive the scope that owned it, and
/// PostgreSQL resolves a savepoint name to the MOST RECENTLY established one,
/// so a leftover shadows an enclosing savepoint of the same name and sends the
/// enclosing rollback to the wrong scope. It also leaves a subtransaction open
/// per rolled-back savepoint, which a retry loop accumulates.
///
/// The order matters and is not interchangeable: after a failed statement the
/// subtransaction is in an aborted state, where `RELEASE` is refused and
/// `ROLLBACK TO` is the statement that recovers it.
pub(crate) fn rollback_savepoint(name: &str) -> String {
    let name = quote_identifier(name);
    format!("ROLLBACK TO {name}; RELEASE {name}")
}

/// Escape `value` for placement inside a single-quoted SQL string literal.
///
/// Returns the BODY, without the surrounding `'`, because the callers embed
/// it in a larger literal they are already building.
///
/// Doubling `'` is sufficient for a standard-conforming literal, which is
/// what PostgreSQL parses when `standard_conforming_strings` is `on` - the
/// default since 9.1, and a value this driver never turns off. With it
/// `off`, a backslash would also escape, and `\'` would slip a quote past
/// this function; that is why `E''`-style literals are never generated here.
///
/// # Do not "fix" this to emit `E'...'`
///
/// It looks unsafe next to `quote_literal()` on the server, which renders
/// `back\slash` as `E'back\\slash'` so the result holds under either setting.
/// Copying that here BREAKS REPLICATION, measured on PostgreSQL 16.14:
///
/// ```text
/// START_REPLICATION ... ("publication_names" E'..."back\slash"...')
///   -> ERROR: syntax error (SQLSTATE 42601)
/// ```
///
/// The only caller is `START_REPLICATION`, and a walsender does not parse its
/// commands with the SQL parser. That grammar takes `'...'` with `''`
/// doubling and nothing else - no `E'...'`, and no backslash escapes to
/// defend against. A backslash inside a publication name reaches pgoutput
/// intact through the plain form, which
/// `tests/suite/replication_publication_names.rs` now pins.
///
/// Verifying the escape by evaluating it with `SELECT` is what makes this
/// mistake attractive: `SELECT E'...'` works perfectly and exercises a
/// different parser from the one the value is actually going to.
///
/// ESTABLISHED 2026-08-26, from the walsender's own scanner: it does NOT
/// consult `standard_conforming_strings`, so `SET standard_conforming_strings
/// = off` on a replication connection cannot make a backslash escape here.
/// `src/backend/replication/repl_scanner.l` on `REL_18_STABLE` declares two
/// exclusive states, `xd` and `xq`, and the whole of `xq` is three rules:
/// `{xqstart}` opens it, `{xqdouble}` (`''`) adds one literal quote, and
/// `{xqinside}` (`[^']+`) copies everything else verbatim - backslash
/// included. There is no `xe` state, no `E'...'`, no backslash rule, and the
/// string `standard_conforming_strings` does not occur in the file.
///
/// The control for that last claim: the MAIN SQL scanner,
/// `src/backend/parser/scan.l`, mentions `standard_conforming_strings` five
/// times. So the absence in `repl_scanner.l` is a real difference between the
/// two grammars and not a grep that finds nothing anywhere.
///
/// This closes the question rather than deferring it: doubling `'` is
/// sufficient here unconditionally, and it stays sufficient no matter what a
/// caller sets.
pub(crate) fn escape_literal_body(value: &str) -> String {
    value.replace('\'', "''")
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
        assert_eq!(escape_literal_body("it's"), "it''s");
    }

    #[test]
    fn a_backslash_in_a_literal_is_left_alone() {
        // Under `standard_conforming_strings = on` a backslash is an ordinary
        // character. Doubling it here would corrupt the value instead of
        // protecting it.
        assert_eq!(escape_literal_body(r"a\b"), r"a\b");
    }
}
