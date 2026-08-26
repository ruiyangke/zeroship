//! URL-form connection-string parity with libpq.
//!
//! Every assertion here was ruled on against a live libpq 16.14 (`psql` inside
//! the `zs-cpg-review-5455` container, which echoes what libpq parsed via
//! `current_setting('application_name')` / `current_database()` /
//! `current_user`, or reports the parse error). The transcript for each case is
//! quoted beside it.
//!
//! These are parse-level assertions: no database is contacted.
//!
//! NOTHING HERE IS `#[ignore]`d ANY MORE. This file arrived with six tests
//! asserting libpq's behaviour for cases where we diverged, red on purpose so
//! each was a ready-made regression test. All six now pass:
//!
//!   * three were already fixed on `main` when the file merged -- it was
//!     written against an older base -- namely the userinfo `@` bound and the
//!     query-string `host=`/`port=` rules;
//!   * three were fixed afterwards: malformed percent escapes, port zero, and
//!     empty credentials meaning unset.
//!
//! The habit that produced that is worth keeping: after any parser change, run
//! `cargo test --test url_parity -- --ignored` if ignores are ever added again.
//! A divergence test that has started PASSING is a fix to record, not a fluke,
//! and one that has started FAILING is a regression with its evidence already
//! written down.

use std::fmt::Write as _;

use compio_postgres::Config;
use compio_postgres::config::Host;

fn cfg(s: &str) -> Config {
    s.parse::<Config>()
        .unwrap_or_else(|e| panic!("{s} should parse: {e}"))
}

fn err(s: &str) -> String {
    let e = s
        .parse::<Config>()
        .err()
        .unwrap_or_else(|| panic!("{s} should have been rejected"));
    let mut msg = e.to_string();
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&e);
    while let Some(s) = src {
        write!(msg, ": {s}").unwrap();
        src = s.source();
    }
    msg
}

fn tcp(c: &Config) -> Vec<String> {
    c.get_hosts()
        .iter()
        .map(|h| match h {
            Host::Tcp(t) => t.clone(),
            Host::Unix(p) => format!("unix:{}", p.display()),
        })
        .collect()
}

// An IPv6 zone id survives bracket stripping and percent-decoding intact.
//
// libpq:
//   postgres://postgres@[::1%25lo]:5432/postgres
//     -> could not translate host name "::1%lo" to address
//   postgres://postgres@[fe80::1%25eth0]:5432/postgres
//     -> connection to server at "fe80::1%eth0" (fe80::1) ... Network is unreachable
// Both name the decoded host, so libpq's parse is `::1%lo` / `fe80::1%eth0`.
#[test]
fn ipv6_zone_id_matches_libpq() {
    assert_eq!(tcp(&cfg("postgres://[::1%25lo]/db")), ["::1%lo"]);
    let c = cfg("postgres://[fe80::1%25eth0]:5433/db");
    assert_eq!(tcp(&c), ["fe80::1%eth0"]);
    assert_eq!(c.get_ports(), [5433]);
    // A plain bracketed literal, and a v4 literal in brackets, which libpq
    // also accepts ("[app=v4inbrackets]...").
    assert_eq!(tcp(&cfg("postgres://[::1]/db")), ["::1"]);
    assert_eq!(tcp(&cfg("postgres://[127.0.0.1]/db")), ["127.0.0.1"]);
}

// Malformed brackets are rejected, as libpq rejects them.
//
// libpq:
//   postgres://postgres@[::1:5432/postgres
//     -> end of string reached when looking for matching "]" in IPv6 host address
//   postgres://postgres@[::1]x:5432/postgres
//     -> unexpected character "x" at position 26 in URI (expected ":" or "/")
//   postgres://postgres@]::1[:5432/postgres
//     -> invalid integer value ":1[:5432" for connection option "port"
#[test]
fn malformed_brackets_are_rejected_like_libpq() {
    assert!(err("postgres://[::1/db").contains("host"));
    assert!(err("postgres://[::1]x/db").contains("host"));
    // No opening bracket: both parsers fall through to the host:port split and
    // fail on the port, not on the host.
    assert!(err("postgres://]::1[/db").contains("port"));
}

// An empty port falls back to 5432, bracketed or not.
//
// libpq:
//   postgres://postgres@127.0.0.1:/postgres?...&application_name=emptyport
//     -> [app=emptyport][db=postgres][usr=postgres][port=5432]
//   postgres://postgres@[::1]:/postgres?...&application_name=v6emptyport
//     -> [app=v6emptyport]...
#[test]
fn empty_port_defaults_to_5432_like_libpq() {
    assert_eq!(cfg("postgres://h:/db").get_ports(), [5432]);
    assert_eq!(cfg("postgres://[::1]:/db").get_ports(), [5432]);
}

// Unparseable and out-of-range ports are rejected, as libpq rejects them.
//
// libpq:
//   :abc      -> invalid integer value "abc" for connection option "port"
//   :5432.0   -> invalid integer value "5432.0" for connection option "port"
//   :5432:5433-> invalid integer value "5432:5433" for connection option "port"
//   :-1       -> invalid port number: "-1"
//   :65536    -> invalid port number: "65536"
//   :99999    -> invalid port number: "99999"
#[test]
fn bad_ports_are_rejected_like_libpq() {
    for url in [
        "postgres://h:abc/db",
        "postgres://h:5432.0/db",
        "postgres://h:5432:5433/db",
        "postgres://h:-1/db",
        "postgres://h:65536/db",
        "postgres://h:99999/db",
    ] {
        assert!(err(url).contains("port"), "{url}");
    }
}

// A leading `+` and a percent-encoded port both reach 5432, as in libpq.
//
// libpq:
//   postgres://postgres@127.0.0.1:+5432/postgres    -> [port=5432], connected
//   postgres://postgres@127.0.0.1:%35%34%33%32/...  -> [app=encport], connected
#[test]
fn plus_and_encoded_ports_match_libpq() {
    assert_eq!(cfg("postgres://h:+5432/db").get_ports(), [5432]);
    assert_eq!(cfg("postgres://h:%35%34%33%32/db").get_ports(), [5432]);
}

// A repeated query key keeps the last occurrence.
//
// libpq:
//   ?application_name=a&application_name=b -> [app=b]
//   ?dbname=template1 (over path /postgres) -> [db=template1]
#[test]
fn last_duplicate_query_key_wins_like_libpq() {
    assert_eq!(
        cfg("postgres://h/db?application_name=a&application_name=b").get_application_name(),
        Some("b")
    );
    assert_eq!(
        cfg("postgres://h/db?dbname=x&dbname=y").get_dbname(),
        Some("y")
    );
    // A query `dbname` overrides the path, and a query `user` overrides the
    // userinfo user -- both confirmed live ([db=template1], [usr=postgres]).
    assert_eq!(
        cfg("postgres://h/pathdb?dbname=querydb").get_dbname(),
        Some("querydb")
    );
    assert_eq!(cfg("postgres://a@h/db?user=b").get_user(), Some("b"));
}

// An empty query and a trailing `&` are both accepted.
//
// libpq:
//   postgres://postgres@127.0.0.1:5432/postgres?            -> connected
//   ...?connect_timeout=2&application_name=a&               -> [app=a]
#[test]
fn empty_query_and_trailing_ampersand_match_libpq() {
    assert_eq!(cfg("postgres://h/db?").get_dbname(), Some("db"));
    assert_eq!(
        cfg("postgres://h/db?application_name=a&").get_application_name(),
        Some("a")
    );
}

// A query parameter with no `=` is rejected, as libpq rejects it.
//
// libpq:
//   ?...&application_name  -> missing key/value separator "=" in URI query parameter
//   ?...&a&application_name=x -> missing key/value separator "=" ... : "a"
//   ?...&&application_name=x  -> missing key/value separator "=" ... : ""
//   ?...&=x&application_name=y-> invalid URI query parameter: ""
//
// We reject all four too, with different (worse) messages: the `&`-spanning
// ones surface as `unknown option \`a&application_name\`` because our key scan
// is not bounded by `&`. The outcome is right, so this is not a defect.
#[test]
fn query_parameter_without_separator_is_rejected_like_libpq() {
    assert!(err("postgres://h/db?application_name").contains("unterminated parameter"));
    assert!(err("postgres://h/db?a&application_name=x").contains("unknown option"));
    assert!(err("postgres://h/db?&application_name=x").contains("unknown option"));
    assert!(err("postgres://h/db?=x").contains("unknown option"));
}

// The dbname keeps encoded and literal slashes; the path is one dbname.
//
// libpq:
//   /post%2Fgres    -> FATAL: database "post/gres" does not exist
//   /postgres/extra -> FATAL: database "postgres/extra" does not exist
//   /postgres/      -> FATAL: database "postgres/" does not exist
//   /               -> connected to the default database
#[test]
fn dbname_path_matches_libpq() {
    assert_eq!(cfg("postgres://h/a%2Fb").get_dbname(), Some("a/b"));
    assert_eq!(cfg("postgres://h/a/b").get_dbname(), Some("a/b"));
    assert_eq!(cfg("postgres://h/a/").get_dbname(), Some("a/"));
    assert_eq!(cfg("postgres://h/").get_dbname(), None);
}

// Userinfo splits on the FIRST colon only; a missing password stays absent.
//
// libpq (against scram-sha-256 on 172.17.0.9, so the password is observable):
//   postgres://postgres@172.17.0.9/postgres
//     -> fe_sendauth: no password supplied
//   postgres://postgres:zeroship:x@172.17.0.9/postgres
//     -> FATAL: password authentication failed  (it sent "zeroship:x")
//   postgres://postgres:zero%73hip@172.17.0.9/postgres
//     -> connected  (percent-decoded to "zeroship")
#[test]
fn userinfo_split_matches_libpq() {
    let c = cfg("postgres://u@h/db");
    assert_eq!(c.get_user(), Some("u"));
    assert_eq!(c.get_password(), None);

    let c = cfg("postgres://u:p:q@h/db");
    assert_eq!(c.get_user(), Some("u"));
    assert_eq!(c.get_password(), Some(&b"p:q"[..]));

    let c = cfg("postgres://u:zero%73hip@h/db");
    assert_eq!(c.get_password(), Some(&b"zeroship"[..]));
}

// The scheme is case-SENSITIVE, and an unknown scheme is not a URL at all.
//
// libpq falls through to keyword=value parsing and fails there:
//   POSTGRES://... -> invalid connection option "POSTGRES://postgres@127.0.0.1:5432/postgres?connect_timeout"
//   Postgres://... -> same
//   mysql://...    -> same
//
// RFC 3986 makes schemes case-insensitive; libpq does not, and matching libpq
// is the contract here. Do not "fix" this.
#[test]
fn scheme_is_case_sensitive_like_libpq() {
    for url in [
        "POSTGRES://h/db",
        "Postgres://h/db",
        "PostgreSQL://h/db",
        "postgres2://h/db",
        "mysql://h/db",
    ] {
        assert!(url.parse::<Config>().is_err(), "{url}");
    }
    // The two spellings libpq does accept.
    assert_eq!(tcp(&cfg("postgres://h/db")), ["h"]);
    assert_eq!(tcp(&cfg("postgresql://h/db")), ["h"]);
}

// An empty value for a closed-set option is rejected, as in libpq.
//
// libpq:
//   ?sslmode=              -> invalid sslmode value: ""
//   ?target_session_attrs= -> invalid target_session_attrs value: ""
//   ?nosuchoption=1        -> invalid URI query parameter: "nosuchoption"
#[test]
fn empty_enum_values_and_unknown_keys_are_rejected_like_libpq() {
    assert!(err("postgres://h/db?sslmode=").contains("sslmode"));
    assert!(err("postgres://h/db?target_session_attrs=").contains("target_session_attrs"));
    assert!(err("postgres://h/db?nosuchoption=1").contains("nosuchoption"));
}

// ---------------------------------------------------------------------------
// Known divergences from libpq. Red until fixed.
// ---------------------------------------------------------------------------

// The userinfo `@` scan must stop at the authority's `/`.
//
// libpq bounds the search for the credentials `@` at `/` (but NOT at `?`):
//   postgres://127.0.0.1:5432/postgres?connect_timeout=2&user=postgres&application_name=c@d
//     -> [app=c@d][db=postgres][usr=postgres][port=5432]     (connected)
//   postgres://127.0.0.1:5432?connect_timeout=2&user=postgres&application_name=a@b
//     -> could not translate host name "b" to address        (no `/`, so libpq
//                                                             DOES take "b")
// We scan the whole string, so the first case becomes
//   user="127.0.0.1" password="5432/postgres?...&application_name=c" host="d"
// -- the connection is redirected to an attacker-nameable host and everything
// before the `@`, which can include a query-string `password=`, is sent there
// as the password. Live confirmation that libpq gets it right:
//   postgres://172.17.0.9:5432/postgres?...&user=postgres&password=zero@ship
//     -> FATAL: password authentication failed for user "postgres"
//        (right host, right user, sent "zero@ship")
#[test]
fn at_sign_after_the_path_is_not_userinfo() {
    let c = cfg("postgres://127.0.0.1:5432/postgres?user=postgres&application_name=c@d");
    assert_eq!(tcp(&c), ["127.0.0.1"]);
    assert_eq!(c.get_user(), Some("postgres"));
    assert_eq!(c.get_password(), None);
    assert_eq!(c.get_dbname(), Some("postgres"));
    assert_eq!(c.get_application_name(), Some("c@d"));

    let c = cfg("postgres://172.17.0.9:5432/postgres?user=postgres&password=zero@ship");
    assert_eq!(tcp(&c), ["172.17.0.9"]);
    assert_eq!(c.get_password(), Some(&b"zero@ship"[..]));

    // Faithful to libpq in the OTHER direction: with no `/`, libpq really does
    // treat the trailing `@b` as a host, so we must keep doing that too.
    let c = cfg("postgres://127.0.0.1:5432?user=postgres&application_name=a@b");
    assert_eq!(tcp(&c), ["b"]);
}

// A query-string `host=` / `port=` REPLACES the authority; it does not append.
//
// libpq, probed so the two hypotheses give opposite outcomes:
//   postgres://postgres@127.0.0.1:5432/postgres?connect_timeout=2&host=nonexistent.invalid
//     -> could not translate host name "nonexistent.invalid" to address
//        (a reachable authority host did NOT save it => replace, not append)
//   postgres://postgres@nonexistent.invalid:5432/postgres?connect_timeout=2&host=127.0.0.1
//     -> connected
//   postgres://postgres@127.0.0.1:5432/postgres?connect_timeout=2&port=9999
//     -> connection to server at "127.0.0.1", port 9999 failed: Connection refused
//   postgres://postgres@127.0.0.1:9999/postgres?connect_timeout=2&port=5432
//     -> connected
//
// We push instead of replacing: `?host=` leaves the authority host first in
// the failover list (so `postgres://placeholder/db?host=real` talks to
// `placeholder`), and `?port=` yields 2 ports for 1 host, which `connect`
// then refuses outright with "invalid number of ports".
#[test]
fn query_host_and_port_replace_the_authority() {
    let c = cfg("postgres://127.0.0.1:5432/db?host=nonexistent.invalid");
    assert_eq!(tcp(&c), ["nonexistent.invalid"]);

    let c = cfg("postgres://127.0.0.1:9999/db?port=5432");
    assert_eq!(c.get_ports(), [5432]);
}

// A query-string `host=` is a comma-separated LIST.
//
// libpq:
//   ...?user=postgres&host=nonexistent.invalid,127.0.0.1&application_name=commahost
//     -> [app=commahost] (connected: two hosts, failover)
//   ...?user=postgres&host=127.0.0.1,nonexistent.invalid&application_name=commahost2
//     -> [app=commahost2] (connected)
//
// We hand the whole value to `host_param`, which never splits, so the config
// ends up with one host literally named "nonexistent.invalid,127.0.0.1".
// `Config::param("host", ..)` -- the keyword=value path -- does split, so we
// are inconsistent with ourselves as well as with libpq.
#[test]
fn query_host_is_comma_split() {
    let c = cfg("postgres:///db?host=nonexistent.invalid,127.0.0.1");
    assert_eq!(tcp(&c), ["nonexistent.invalid", "127.0.0.1"]);
}

// A malformed percent escape is an error, not literal text.
//
// libpq:
//   ?application_name=%zz -> invalid percent-encoded token: "%zz"
//   ?application_name=a%  -> invalid percent-encoded token: "a%"
//   ?application_name=%2  -> invalid percent-encoded token: "%2"
//   postgres://postgres@[fe80::1%eth0]:5432/postgres
//                         -> invalid percent-encoded token: "fe80::1%eth0"
//
// `percent_encoding::percent_decode` never fails on a bad escape -- it emits
// the bytes verbatim -- so we silently connect with `%zz` as the value. A
// stray `%` in a password or dbname is a typo, and libpq is right to say so
// rather than authenticate with the wrong string.
#[test]
fn malformed_percent_escapes_are_rejected() {
    for url in [
        "postgres://h/db?application_name=%zz",
        "postgres://h/db?application_name=a%",
        "postgres://h/db?application_name=%2",
        "postgres://h%zz/db",
        "postgres://u%zz@h/db",
        "postgres://[fe80::1%eth0]/db",
    ] {
        assert!(url.parse::<Config>().is_err(), "{url} should be rejected");
    }
}

// Port 0 is not a port.
//
// libpq: postgres://postgres@127.0.0.1:0/postgres -> invalid port number: "0"
// We accept it and produce `ports=[0]`.
#[test]
fn port_zero_is_rejected() {
    assert!(err("postgres://h:0/db").contains("port"));
}

// An empty `user` or `password` means unset, not the empty string.
//
// libpq:
//   postgres://:zeroship@172.17.0.9:5432/postgres?...
//     -> FATAL: role "root" does not exist   (empty userinfo user discarded,
//                                             fell back to the OS user)
//   postgres://postgres:zeroship@172.17.0.9:5432/postgres?...&user=
//     -> FATAL: password authentication failed for user "root"   (same)
//   postgres://postgres:@172.17.0.9:5432/postgres?...
//     -> fe_sendauth: no password supplied    (empty password == none)
//   postgres://postgres:zeroship@172.17.0.9:5432/postgres?...&password=
//     -> fe_sendauth: no password supplied
//
// We store `Some("")`, which suppresses the `whoami` fallback we already have
// in `connect_raw` and makes us send an empty password where libpq would
// report that none was supplied.
#[test]
fn empty_user_and_password_are_unset() {
    assert_eq!(cfg("postgres://:p@h/db").get_user(), None);
    assert_eq!(cfg("postgres://h/db?user=").get_user(), None);
    assert_eq!(cfg("postgres://u:@h/db").get_password(), None);
    assert_eq!(cfg("postgres://h/db?password=").get_password(), None);
}
