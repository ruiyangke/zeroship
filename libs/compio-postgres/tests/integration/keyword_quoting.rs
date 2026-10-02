//! Quoting and escaping inside a `keyword=value` connection string.
//!
//! Every expectation here was taken from psql 16.14 rather than from the
//! manual, and every one of them ALREADY MATCHED -- this file records a
//! surface that was measured and found faithful, so the next person does not
//! re-derive it. That is worth as much as a bug: the lexer is small, hand
//! written, and the obvious places to get it wrong are all pinned below.
//!
//! The one divergence in this area is pinned separately in
//! `libpq_parameter_parity.rs`: a TRAILING empty value, which libpq accepts as
//! the empty string and this refuses. We differ loudly there, which is the
//! safe direction.
//!
//! Separator rules live in `keyword_whitespace.rs`; this file is about what
//! happens INSIDE a value.

use compio_postgres::Config;
use std::str::FromStr;

fn ok(s: &str) -> Config {
    Config::from_str(s).unwrap_or_else(|error| panic!("{s:?} should parse: {error}"))
}

/// The error's SOURCE, because the top-level `Display` is only "invalid
/// connection string" and says nothing about which rule was broken.
fn cause(s: &str) -> String {
    let error = Config::from_str(s).expect_err("expected a parse failure");
    let mut link = std::error::Error::source(&error);
    let mut last = String::new();
    while let Some(current) = link {
        last = current.to_string();
        link = current.source();
    }
    last
}

#[test]
fn a_backslash_escapes_the_next_character() {
    // Outside quotes, a backslash is dropped and the next character is kept
    // verbatim -- including a space, which would otherwise separate pairs.
    assert_eq!(
        ok("user=u application_name=a\\b").get_application_name(),
        Some("ab")
    );
    assert_eq!(
        ok("user=u application_name=a\\ b").get_application_name(),
        Some("a b")
    );
    // A trailing backslash has nothing to escape and is simply dropped.
    assert_eq!(
        ok("user=u application_name=P6\\").get_application_name(),
        Some("P6")
    );

    // We consume the next CHARACTER; libpq consumes the next BYTE. They agree,
    // because the trailing bytes of a multi-byte UTF-8 character are never a
    // separator or a backslash and so are copied verbatim either way.
    assert_eq!(
        ok("user=u application_name=a\\\u{a0}b").get_application_name(),
        Some("a\u{a0}b")
    );
}

#[test]
fn an_options_backslash_survives_conninfo_lexing() {
    let parsed = ok(r"host=h options='-c search_path=a\\ b'");
    assert_eq!(parsed.get_options(), Some(r"-c search_path=a\ b"));
}

#[test]
fn an_equals_sign_is_ordinary_inside_a_value() {
    assert_eq!(
        ok("user=u application_name==P7").get_application_name(),
        Some("=P7")
    );
    assert_eq!(
        ok("user=u application_name=a=b").get_application_name(),
        Some("a=b")
    );
}

#[test]
fn a_quote_mid_value_is_not_special() {
    // Quoting only opens a quoted value at the START of one.
    assert_eq!(
        ok("user=u application_name=lo'cal'host").get_application_name(),
        Some("lo'cal'host")
    );
    // A backslash before the opening quote makes it a value byte, so the value
    // is unquoted and the quote survives into it.
    assert_eq!(
        ok("user=u application_name=\\'a").get_application_name(),
        Some("'a")
    );
    // CONTROL for that one: without the backslash the quote opens a value that
    // never closes, and the string is refused.
    Config::from_str("user=u application_name='a")
        .expect_err("an unterminated quote must be refused");
}

#[test]
fn a_quoted_value_may_hold_a_newline_and_needs_no_separator_after_it() {
    assert_eq!(
        ok("user=u application_name='a\nb'").get_application_name(),
        Some("a\nb")
    );

    // A quoted value ends AT its closing quote, so the next keyword may follow
    // with no whitespace between them.
    let joined = ok("user=postgres application_name='a'user=alice");
    assert_eq!(joined.get_application_name(), Some("a"));
    assert_eq!(joined.get_user(), Some("alice"));

    // CONTROL: the spaced form means the same thing, so the assertion above is
    // about adjacency rather than about the values themselves.
    let spaced = ok("user=postgres application_name='a' user=alice");
    assert_eq!(spaced.get_application_name(), Some("a"));
    assert_eq!(spaced.get_user(), Some("alice"));
}

#[test]
fn an_unterminated_quote_names_what_was_wrong() {
    assert_eq!(
        cause("user=u application_name='P5"),
        "unterminated quoted connection parameter value"
    );
    // CONTROL: closing the quote parses the same string.
    assert_eq!(
        ok("user=u application_name='P5'").get_application_name(),
        Some("P5")
    );
}

#[test]
fn an_empty_keyword_is_refused_rather_than_ending_the_string() {
    // The danger if this were accepted-and-ignored: everything AFTER the empty
    // keyword would be silently dropped, which could include an sslmode.
    for dsn in [
        "host=h user=postgres =foo user=alice",
        "=x host=h",
        "host=h user=u =",
    ] {
        Config::from_str(dsn)
            .err()
            .unwrap_or_else(|| panic!("{dsn:?} must be refused, not silently truncated"));
    }

    // CONTROL: the same strings without the empty keyword parse, and the later
    // pair really does take effect -- so the refusals above are about the empty
    // keyword and not about the rest of the string.
    assert_eq!(
        ok("host=h user=postgres user=alice").get_user(),
        Some("alice")
    );
}
