//! What separates one `keyword=value` pair from the next.
//!
//! libpq splits on C's `isspace()` in the C locale, applied to BYTES. This
//! used Rust's `char::is_whitespace`, which is the UNICODE definition and
//! includes U+00A0, U+2007 and the U+2000 block. A value containing any of
//! those ended early and its remainder parsed as further keywords, so the
//! connection could be made as a different ROLE than the string names:
//!
//! ```text
//! host=127.0.0.1 ... user=postgres application_name=x<U+00A0>user=alice
//!   libpq: application_name is the whole "x\xc2\xa0user=alice", user postgres
//!   was:   application_name "x", user ALICE
//! ```
//!
//! The predicate is also not `char::is_ascii_whitespace`, which omits the
//! vertical tab (0x0B) that C's `isspace()` includes and libpq splits on. Both
//! halves are asserted below, because getting one right and the other wrong is
//! the easy mistake.

use compio_postgres::Config;

#[test]
fn only_c_isspace_separates_keyword_pairs() {
    // The defect: a Unicode space must NOT end the value.
    let nbsp = "host=h user=postgres application_name=x\u{a0}user=alice"
        .parse::<Config>()
        .expect("a non-breaking space in a value must parse");
    assert_eq!(
        nbsp.get_application_name(),
        Some("x\u{a0}user=alice"),
        "a Unicode space split the value"
    );
    assert_eq!(
        nbsp.get_user(),
        Some("postgres"),
        "splitting on a Unicode space changed which role we connect as"
    );

    // Other Unicode spaces libpq does not split on.
    for space in ['\u{2007}', '\u{2003}', '\u{feff}'] {
        let dsn = format!("host=h user=u application_name=a{space}b");
        let parsed = dsn.parse::<Config>().expect("unicode space parses");
        assert_eq!(
            parsed.get_application_name(),
            Some(format!("a{space}b").as_str()),
            "U+{:04X} was treated as a separator",
            space as u32
        );
    }

    // THE OTHER HALF: every byte C's isspace() covers MUST separate,
    // including the vertical tab that `is_ascii_whitespace` leaves out.
    for space in [' ', '\t', '\n', '\u{b}', '\u{c}', '\r'] {
        let dsn = format!("host=h{space}user=alice");
        let parsed = dsn.parse::<Config>().expect("ascii space parses");
        assert_eq!(
            parsed.get_user(),
            Some("alice"),
            "0x{:02X} did not separate two pairs",
            space as u32
        );
    }
}
