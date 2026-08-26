//! Parity against libpq's connection-parameter surface.
//!
//! The list below is every connection parameter PostgreSQL 18's libpq accepts.
//! For each one this driver must do exactly one of two things: implement it, or
//! refuse it by name. The outcome this pins down is the third one -- accepting a
//! parameter and silently ignoring it, which looks identical to support from the
//! caller's side and misconfigures them quietly.
//!
//! A refusal has to name the key through the error's SOURCE chain, because the
//! top-level `Display` is only "invalid connection string"; without the cause a
//! caller cannot tell which option they got wrong.
//!
//! When libpq gains a parameter, add it here and rule on it. This table failing
//! is the point: it means we have not decided.

use compio_postgres::config::{Host, ProtocolVersion};
use compio_postgres::{Config, NoTls};

#[allow(dead_code)]
mod common;

/// What this driver is expected to do with a libpq parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// We implement it.
    Accepted,
    /// We deliberately do not, and say so naming the key.
    Refused,
    /// We implement it, but it names EXTERNAL STATE that this test cannot
    /// stand up, so parsing a probe value legitimately fails.
    ///
    /// This needs its own verdict rather than being folded into one of the
    /// other two, because both would be wrong in a way that reads as right.
    /// `Accepted` fails outright. `Refused` PASSES - the error mentions the
    /// key - while asserting the opposite of the truth, which is exactly the
    /// silent-misclassification this whole test exists to prevent.
    ///
    /// The evidence that it is implemented is that the failure is about
    /// RESOLVING the value: an unrecognised key reports `unknown option
    /// <key>` and could not mention the value, whereas a key we act on
    /// reports that it could not find `<value>`.
    Resolved,
}

use Verdict::{Accepted, Refused, Resolved};

/// `(parameter, a value libpq would consider well formed, our verdict)`.
///
/// Values matter: several accepted parameters validate their argument, so a
/// placeholder would test the value parser instead of the key.
const LIBPQ_PARAMETERS: &[(&str, &str, Verdict)] = &[
    ("application_name", "app", Accepted),
    ("channel_binding", "prefer", Accepted),
    ("client_encoding", "UTF8", Accepted),
    ("connect_timeout", "10", Accepted),
    ("dbname", "db", Accepted),
    ("fallback_application_name", "fb", Accepted),
    ("gssdelegation", "0", Accepted),
    ("gssencmode", "disable", Accepted),
    ("gsslib", "gssapi", Refused),
    ("hostaddr", "127.0.0.1", Accepted),
    ("keepalives", "1", Accepted),
    ("keepalives_count", "3", Accepted),
    ("keepalives_idle", "10", Accepted),
    ("keepalives_interval", "5", Accepted),
    ("krbsrvname", "postgres", Refused),
    ("load_balance_hosts", "disable", Accepted),
    ("max_protocol_version", "3.2", Accepted),
    ("min_protocol_version", "3.2", Accepted),
    ("oauth_client_id", "id", Refused),
    ("oauth_client_secret", "secret", Refused),
    ("oauth_issuer", "https://example.test", Refused),
    ("oauth_scope", "scope", Refused),
    // Space-free on purpose: in keyword syntax an unquoted value ends at the
    // first space, so `-c geqo=off` would test the DSN lexer, not this key.
    ("options", "-cgeqo=off", Accepted),
    ("passfile", "/tmp/pgpass", Accepted),
    ("password", "p", Accepted),
    ("port", "5432", Accepted),
    ("replication", "false", Accepted),
    ("require_auth", "password", Accepted),
    ("requirepeer", "postgres", Accepted),
    ("requiressl", "0", Accepted),
    ("scram_client_key", "key", Refused),
    ("scram_server_key", "key", Refused),
    ("service", "svc", Resolved),
    ("ssl_max_protocol_version", "TLSv1.3", Accepted),
    ("ssl_min_protocol_version", "TLSv1.2", Accepted),
    ("sslcert", "/tmp/client.crt", Accepted),
    ("sslcertmode", "allow", Accepted),
    ("sslcompression", "0", Accepted),
    ("sslcrl", "/tmp/root.crl", Accepted),
    ("sslcrldir", "/tmp/crls", Accepted),
    ("sslkey", "/tmp/client.key", Accepted),
    ("sslkeylogfile", "/tmp/keys.log", Accepted),
    ("sslmode", "prefer", Accepted),
    ("sslnegotiation", "postgres", Accepted),
    ("sslpassword", "pw", Accepted),
    ("sslrootcert", "/tmp/root.crt", Accepted),
    ("sslsni", "1", Accepted),
    ("target_session_attrs", "any", Accepted),
    ("tcp_user_timeout", "5", Accepted),
    ("user", "u", Accepted),
];

/// Does any error in the chain name `key`?
fn a_cause_names(error: &compio_postgres::Error, key: &str) -> bool {
    std::iter::successors(std::error::Error::source(error), |error| {
        std::error::Error::source(*error)
    })
    .any(|cause| cause.to_string().contains(key))
}

#[test]
fn every_libpq_parameter_is_implemented_or_refused_by_name() {
    let mut wrong = Vec::new();

    for (key, value, verdict) in LIBPQ_PARAMETERS {
        let outcome = format!("host=h {key}={value}").parse::<Config>();
        match (verdict, outcome) {
            (Accepted, Ok(_)) => {}
            (Refused, Err(error)) if a_cause_names(&error, key) => {}
            (Refused, Err(error)) => wrong.push(format!(
                "{key}: refused, but no cause names it, so the caller cannot \
                 tell which option is wrong ({error})"
            )),
            (Accepted, Err(error)) => {
                wrong.push(format!(
                    "{key}: expected to be implemented, refused ({error})"
                ));
            }
            (Refused, Ok(_)) => wrong.push(format!(
                "{key}: accepted but unimplemented, so it is silently ignored"
            )),
            // Naming the VALUE is what separates "we tried to resolve it" from
            // "we do not know this key": an unknown-option error cannot
            // mention a value it never looked at.
            (Resolved, Err(error)) if a_cause_names(&error, value) => {}
            (Resolved, Err(error)) => wrong.push(format!(
                "{key}: expected a failure to RESOLVE {value}, but no cause \
                 names it, so this looks like the key being rejected ({error})"
            )),
            (Resolved, Ok(_)) => wrong.push(format!(
                "{key}: resolved {value} against external state this test did \
                 not create, so it cannot have been looked up"
            )),
        }
    }

    assert!(
        wrong.is_empty(),
        "libpq parameter parity:\n  {}",
        wrong.join("\n  ")
    );
}

/// `host` and `hostaddr` are positional in a URL and covered above only as
/// keywords; this pins that the table above is the whole libpq surface rather
/// than a subset someone trimmed to make it pass.
#[test]
fn the_parity_table_covers_every_parameter_libpq_18_accepts() {
    // PostgreSQL 18 libpq accepts 51 connection parameters. `host` is exercised
    // by every row above as the DSN's own host, so the table carries the other
    // 50.
    assert_eq!(
        LIBPQ_PARAMETERS.len(),
        50,
        "the libpq parameter table changed size without its floor being re-ruled"
    );
}

/// The count above cannot see a SUBSTITUTION: add one parameter and drop
/// another and it still reads 50, so the table can drift to a different set of
/// keys while its floor stays green. This pins the set itself.
///
/// The list was not copied from the documentation. It was derived on
/// 2026-08-23 by probing the libpq the test server actually ships
/// (`libpq.so.5.18`, PostgreSQL 18) with a URL whose host does not resolve:
/// libpq validates parameter NAMES before it does any network I/O, so an
/// unknown key answers `invalid URI query parameter` while a known key gets as
/// far as `could not translate host name`. Two different messages, therefore a
/// probe that discriminates. Candidates came from the strings in that binary,
/// plus every underscore-delimited tail of each -- without the tails the sweep
/// missed `application_name`, `host` and `password`, because a C compiler
/// stores a literal that is a suffix of another as a pointer into it, so
/// `application_name` never appears standalone inside
/// `fallback_application_name`.
///
/// To re-derive after a libpq upgrade, repeat that sweep rather than reading a
/// release note; `host` is the one accepted key deliberately absent here, for
/// the reason given above.
#[test]
fn the_parity_table_names_exactly_the_parameters_libpq_18_accepts() {
    const EXPECTED: &[&str] = &[
        "application_name",
        "channel_binding",
        "client_encoding",
        "connect_timeout",
        "dbname",
        "fallback_application_name",
        "gssdelegation",
        "gssencmode",
        "gsslib",
        "hostaddr",
        "keepalives",
        "keepalives_count",
        "keepalives_idle",
        "keepalives_interval",
        "krbsrvname",
        "load_balance_hosts",
        "max_protocol_version",
        "min_protocol_version",
        "oauth_client_id",
        "oauth_client_secret",
        "oauth_issuer",
        "oauth_scope",
        "options",
        "passfile",
        "password",
        "port",
        "replication",
        "require_auth",
        "requirepeer",
        "requiressl",
        "scram_client_key",
        "scram_server_key",
        "service",
        "ssl_max_protocol_version",
        "ssl_min_protocol_version",
        "sslcert",
        "sslcertmode",
        "sslcompression",
        "sslcrl",
        "sslcrldir",
        "sslkey",
        "sslkeylogfile",
        "sslmode",
        "sslnegotiation",
        "sslpassword",
        "sslrootcert",
        "sslsni",
        "target_session_attrs",
        "tcp_user_timeout",
        "user",
    ];

    let mut actual: Vec<&str> = LIBPQ_PARAMETERS.iter().map(|(key, _, _)| *key).collect();
    actual.sort_unstable();

    // Sorting also makes a duplicated row visible: a table that names one key
    // twice and another not at all still has 50 entries.
    assert_eq!(
        actual, EXPECTED,
        "the parity table no longer names libpq 18's parameter set"
    );
}

/// The driver opts in to PostgreSQL 18's stronger cancel key by default, but
/// keeps 3.0 as the floor so an older server can negotiate the startup down.
/// Both getters are asserted: accepting these parameters and throwing their
/// values away is the exact silent failure this file exists to prevent.
#[test]
fn wire_protocol_defaults_request_3_2_and_allow_3_0_fallback() {
    let config = Config::new();
    assert_eq!(
        config.get_min_protocol_version(),
        ProtocolVersion::V3_0,
        "the default minimum must leave the PostgreSQL 16 fallback available"
    );
    assert_eq!(
        config.get_max_protocol_version(),
        ProtocolVersion::V3_2,
        "the default maximum is the version sent in the startup request"
    );
}

/// PostgreSQL 18's accepted spellings are a closed set. `latest` denotes the
/// latest version this driver implements, currently 3.2; it is not a third
/// wire version.
#[test]
fn wire_protocol_bounds_parse_every_supported_spelling() {
    for (value, expected) in [
        ("3.0", ProtocolVersion::V3_0),
        ("3.2", ProtocolVersion::V3_2),
        ("latest", ProtocolVersion::V3_2),
    ] {
        let minimum = format!("host=h min_protocol_version={value}")
            .parse::<Config>()
            .unwrap_or_else(|error| panic!("min_protocol_version={value} was refused: {error}"));
        assert_eq!(
            minimum.get_min_protocol_version(),
            expected,
            "min_protocol_version={value} was accepted but ignored"
        );

        let maximum = format!("postgresql://h/db?max_protocol_version={value}")
            .parse::<Config>()
            .unwrap_or_else(|error| panic!("max_protocol_version={value} was refused: {error}"));
        assert_eq!(
            maximum.get_max_protocol_version(),
            expected,
            "max_protocol_version={value} was accepted but ignored"
        );
    }
}

/// Programmatic configuration has the same typed bounds as a connection
/// string. Using opposite values is deliberate: it proves the two setters do
/// not alias the same field before the range check below rules on the pair.
#[test]
fn wire_protocol_bound_builders_store_distinct_values() {
    let mut config = Config::new();
    config
        .min_protocol_version(ProtocolVersion::V3_2)
        .max_protocol_version(ProtocolVersion::V3_0);

    assert_eq!(config.get_min_protocol_version(), ProtocolVersion::V3_2);
    assert_eq!(config.get_max_protocol_version(), ProtocolVersion::V3_0);
    assert_eq!(ProtocolVersion::V3_0.as_str(), "3.0");
    assert_eq!(ProtocolVersion::V3_2.as_str(), "3.2");
}

/// Values outside the versions the driver can actually speak are rejected by
/// key. In particular 3.1 is reserved, and `latest` is case-sensitive in
/// libpq's connection parser.
#[test]
fn unsupported_wire_protocol_versions_are_refused_by_key() {
    for key in ["min_protocol_version", "max_protocol_version"] {
        for value in ["", "3", "3.1", "3.3", "LATEST"] {
            let error = format!("host=h {key}='{value}'")
                .parse::<Config>()
                .expect_err("an unsupported wire protocol version parsed");
            assert!(
                a_cause_names(&error, key),
                "{key}={value:?}: the refusal did not name the parameter: {error:?}"
            );
        }
    }
}

/// An empty range is a local configuration error. The loop covers both input
/// orders because the parser must store the pair and connection validation,
/// not whichever arm happens to run last, must reject it before socket I/O.
#[compio::test]
async fn inverted_wire_protocol_range_is_refused_before_connecting() {
    for bounds in [
        "min_protocol_version=3.2 max_protocol_version=3.0",
        "max_protocol_version=3.0 min_protocol_version=3.2",
    ] {
        let config = format!("hostaddr=127.0.0.1 port=1 sslmode=disable {bounds}")
            .parse::<Config>()
            .expect("each bound is individually valid");
        let error = match config.connect(NoTls).await {
            Ok(_) => panic!("an inverted protocol range connected: {bounds}"),
            Err(error) => error,
        };
        assert!(
            a_cause_names(&error, "min_protocol_version")
                && a_cause_names(&error, "max_protocol_version"),
            "the range error must name both conflicting bounds: {error:?}"
        );
    }
}

/// A repeated key REPLACES, as libpq does; only a comma builds a host list.
///
/// This crate accepted `host=a host=b` as a two-host FAILOVER list, so a
/// connection string assembled by concatenation -- a default followed by an
/// override, which is how DSNs are usually built from layered config -- kept
/// the default and tried it FIRST. libpq resolves the same string to `b`
/// alone, so the two drivers connect to different servers and neither reports
/// anything.
///
/// MEASURED AGAINST THE REAL libpq, not against the documentation, by handing
/// psql `host=127.0.0.1 host=nonexistent.invalid`. It fails to resolve the
/// second name, which it could only do by having discarded the first. The
/// comma form is genuinely multi-host in both, and stays that way here.
///
/// This is the exact failure the header of this file exists to prevent, one
/// level down: not a parameter accepted and ignored, but a parameter accepted
/// and given different meaning.
#[test]
fn a_repeated_key_replaces_rather_than_appending() {
    let replaced = "host=first.invalid host=127.0.0.1 user=u"
        .parse::<Config>()
        .expect("a repeated host key must parse");
    assert_eq!(
        replaced.get_hosts().len(),
        1,
        "a repeated host key built a failover list instead of overriding: {:?}",
        replaced.get_hosts()
    );

    let ports = "host=h user=u port=1 port=2"
        .parse::<Config>()
        .expect("a repeated port key must parse");
    assert_eq!(
        ports.get_ports(),
        [2],
        "a repeated port key accumulated instead of overriding"
    );

    // The one-variable partner: a comma is still a list, in both drivers.
    let listed = "host=a,b user=u".parse::<Config>().expect("comma form");
    assert_eq!(
        listed.get_hosts().len(),
        2,
        "the comma form must stay multi-host"
    );

    // And a repeated key after a comma list replaces the whole list.
    let overridden = "host=a,b host=c user=u"
        .parse::<Config>()
        .expect("override");
    assert_eq!(
        overridden.get_hosts().len(),
        1,
        "an override after a list must replace it, not extend it"
    );
}

/// The keyword-string LEXER, measured against libpq rather than its docs.
///
/// The table above rules on which KEYS are accepted. Nothing had ever checked
/// how a VALUE is read, which is the other half of the same surface and the
/// one with quoting in it. Every expectation below was taken by handing the
/// string to psql and asking the server what it received
/// (`current_setting('application_name')`), so these are observations of the
/// reference implementation, not readings of the manual.
///
/// THE SWALLOW RULE IS THE SURPRISING ONE AND IT IS libpq's, NOT A BUG OF
/// OURS. `application_name= user=u` does not mean "empty, then user"; the
/// value is the next whitespace-delimited token, so it means
/// `application_name` is literally `user=u` and `user` is never set. Verified
/// against psql twice: once as `... application_name= connect_timeout=2`,
/// which reports `[app=connect_timeout=2]`, and once as `application_name=
/// user=postgres`, which fails as role "root" because the user was consumed.
/// I first read our matching behaviour as a defect; it is parity.
///
/// The one genuine divergence is a TRAILING empty value, which libpq accepts
/// as the empty string and we refuse. We differ LOUDLY there, which is the
/// safe direction -- an empty value at the end is usually an unset variable --
/// so this pins the difference rather than erasing it.
#[test]
fn the_value_lexer_matches_libpq() {
    // (connection string, what libpq makes application_name)
    const AGREED: &[(&str, &str)] = &[
        ("host=h application_name='a b'", "a b"),
        ("host=h application_name='a\\'b'", "a'b"),
        ("host=h application_name='a\\\\b'", "a\\b"),
        ("host=h application_name = spaced", "spaced"),
        ("host=h application_name=''", ""),
        // The swallow rule, both orders.
        ("host=h application_name= user=u", "user=u"),
        ("application_name= host=h", "host=h"),
    ];

    let mut wrong = Vec::new();
    for (dsn, expected) in AGREED {
        match dsn.parse::<Config>() {
            Ok(config) => {
                let ours = config.get_application_name();
                if ours != Some(*expected) {
                    wrong.push(format!("{dsn}: libpq reads {expected:?}, we read {ours:?}"));
                }
            }
            Err(error) => wrong.push(format!(
                "{dsn}: libpq reads {expected:?}, we refuse ({error})"
            )),
        }
    }
    assert!(
        wrong.is_empty(),
        "value lexing diverges from libpq:\n  {}",
        wrong.join("\n  ")
    );

    // This block pinned a KNOWN divergence -- a trailing empty value was
    // refused here and accepted by libpq -- and said to update it if the
    // divergence was ever closed. It was closed on 2026-08-25, so it now says
    // so: both of these parse, and `Config::param` decides what an empty value
    // MEANS per key.
    let named = "host=h application_name="
        .parse::<Config>()
        .expect("libpq accepts a trailing empty value");
    assert_eq!(
        named.get_application_name(),
        Some(""),
        "an empty string option keeps the empty string, as libpq does"
    );

    let credential = "host=h user="
        .parse::<Config>()
        .expect("libpq accepts a trailing empty value");
    assert_eq!(
        credential.get_user(),
        None,
        "an empty CREDENTIAL is unset, so the whoami fallback still runs -- \
         libpq likewise resolves `user=` to the operating-system user"
    );
}
/// A URL authority keeps one port PER HOST, and the query string still
/// overrides the whole list.
///
/// REGRESSION. Making a repeated `port=` keyword override rather than
/// accumulate -- correct for `port=1 port=2`, which libpq resolves to 2 --
/// also reached the URL authority, which calls the same parameter setter once
/// per host. `postgres://a:1,b:2/db` therefore kept only `[2]`, and host `a`
/// would have been dialled on host `b`'s port. The whole suite passed: nothing
/// covered a URL carrying several hosts with explicit ports.
///
/// The two rules are different because the inputs are: several hosts in one
/// authority are a LIST, and a key written twice is an OVERRIDE.
#[test]
fn a_url_authority_keeps_a_port_per_host() {
    let listed = "postgres://a:1,b:2/db"
        .parse::<Config>()
        .expect("a multi-host URL must parse");
    assert_eq!(
        listed.get_ports(),
        [1, 2],
        "the authority lost a port; every host but the last would be dialled wrong"
    );
    assert_eq!(listed.get_hosts().len(), 2);

    // Default port per host, mixed with an explicit one.
    let mixed = "postgres://a,b:2/db"
        .parse::<Config>()
        .expect("mixed ports");
    assert_eq!(mixed.get_ports(), [5432, 2]);

    // The query string overrides the whole list, as libpq does: it is parsed
    // after the authority and arrives through the repeated-key path.
    let overridden = "postgres://a:1,b:2/db?port=9"
        .parse::<Config>()
        .expect("a query port must parse");
    assert_eq!(
        overridden.get_ports(),
        [9],
        "a query-string port must replace the authority's list, not extend it"
    );
}

/// A URL authority APPENDS its hosts, and that holds on every target.
///
/// The port half of this was a live regression; the host half was the same
/// defect confined to `#[cfg(not(unix))]`, where `host_param` routed through
/// the clearing `param("host", ..)` and a multi-host URL kept only the last.
/// It could not fail here, which is exactly why it survived: the two
/// definitions had drifted and only the untestable one was wrong.
///
/// The fix collapsed them into one function whose append is compiled
/// everywhere, so this test now covers the Windows path by construction rather
/// than by inspection. That is the only claim it can honestly make -- no test
/// in this suite EXECUTES a non-Unix target.
#[test]
fn a_url_authority_appends_every_host() {
    let listed = "postgres://a,b,c/db"
        .parse::<Config>()
        .expect("a multi-host URL must parse");
    assert_eq!(
        listed.get_hosts().len(),
        3,
        "the authority dropped hosts: {:?}",
        listed.get_hosts()
    );

    // Still an override when the key is repeated, which is the rule the
    // clearing exists for.
    let overridden = "postgres://a,b/db?host=c"
        .parse::<Config>()
        .expect("a query host must parse");
    assert_eq!(
        overridden.get_hosts().len(),
        1,
        "a query-string host must replace the authority's list"
    );
}

/// The URI rules that are not about which shapes parse, but about what the
/// parts MEAN: percent-decoding, bracketed IPv6, and which of the authority
/// and the query string wins.
///
/// Every row was measured against psql 16.14 on 2026-08-25 and this crate
/// already agreed with all of them - the test exists because nothing pinned
/// them, and this file records two regressions in exactly this area (the
/// per-host port, below). A rule nothing asserts is one a refactor may quietly
/// change.
#[test]
fn uri_components_are_decoded_and_the_query_string_wins() {
    fn parsed(url: &str) -> Config {
        url.parse::<Config>()
            .unwrap_or_else(|error| panic!("libpq accepts {url}: {error}"))
    }

    fn tcp_hosts(config: &Config) -> Vec<String> {
        config
            .get_hosts()
            .iter()
            .map(|host| match host {
                Host::Tcp(host) => host.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    // Percent-decoding reaches every component, not just the host.
    assert_eq!(
        tcp_hosts(&parsed("postgresql://pct%2Ddash.invalid/db")),
        vec!["pct-dash.invalid".to_owned()]
    );
    assert_eq!(
        parsed("postgresql://u%40dom@127.0.0.1/db").get_user(),
        Some("u@dom"),
        "userinfo is percent-decoded; libpq reports `role \"u@dom\" does not exist`"
    );
    assert_eq!(
        parsed("postgresql://127.0.0.1/a%2Fb").get_dbname(),
        Some("a/b"),
        "a database name may contain an encoded slash"
    );

    // A malformed escape is an ERROR, not a literal percent sign. libpq:
    // `invalid percent-encoded token: "bad%zz.invalid"`.
    for bad in [
        "postgresql://bad%zz.invalid/db",
        "postgresql://trunc%2.invalid/db",
    ] {
        assert!(
            bad.parse::<Config>().is_err(),
            "an invalid percent-encoded token must be refused: {bad}"
        );
    }

    // A bracketed IPv6 literal keeps its brackets out of the host, with and
    // without an explicit port.
    assert_eq!(
        tcp_hosts(&parsed("postgresql://[::1]:5432/db")),
        vec!["::1".to_owned()]
    );
    assert_eq!(
        tcp_hosts(&parsed("postgresql://[fe80::1]/db")),
        vec!["fe80::1".to_owned()]
    );

    // The QUERY STRING wins over the authority, for host and for port.
    assert_eq!(
        tcp_hosts(&parsed("postgresql://h.invalid/db?host=query.invalid")),
        vec!["query.invalid".to_owned()],
        "libpq resolves query.invalid, never the authority's h.invalid"
    );
    assert_eq!(
        parsed("postgresql://127.0.0.1:1/db?port=2").get_ports(),
        [2],
        "libpq reports `port 2 failed`, so the query overrides the authority"
    );

    // And a query parameter can name a unix socket directory.
    assert!(
        matches!(
            parsed("postgresql:///db?host=/tmp").get_hosts().first(),
            Some(Host::Unix(_))
        ),
        "?host=/tmp is a socket directory, as libpq treats it"
    );
}

/// URL SHAPES libpq accepts, all of which parse here too -- and the one place
/// the two then behave differently.
///
/// Every shape below was handed to psql first; all five connect or parse
/// there. The interesting one is `postgres:///db`, which parses to NO host in
/// both implementations. libpq then connects over a compiled-in default socket
/// directory; this refuses with "both host and hostaddr are missing", because
/// half of libpq's answer there is `PGHOST` and this crate reads no process
/// configuration. That is now stated at the check in `connect.rs` rather than
/// being incidental.
#[compio::test]
async fn url_shapes_parse_like_libpq_and_an_empty_host_is_refused_at_connect() {
    // Accepted by libpq; must parse here.
    for url in [
        "postgresql://postgres@127.0.0.1:5432/postgres",
        "postgres:///postgres?user=postgres",
        "postgres://postgres@/postgres",
        "postgres://",
    ] {
        url.parse::<Config>()
            .unwrap_or_else(|error| panic!("libpq accepts {url}, we refused it: {error}"));
    }

    // A percent-encoded absolute path is a SOCKET DIRECTORY, not a hostname.
    let socket = "postgres://%2Fvar%2Frun%2Fpostgresql/postgres?user=postgres"
        .parse::<Config>()
        .expect("an encoded socket path must parse");
    assert!(
        format!("{:?}", socket.get_hosts()).contains("Unix"),
        "an encoded absolute path became a TCP host: {:?}",
        socket.get_hosts()
    );

    // The divergence, pinned. No server is contacted: the refusal happens
    // while enumerating endpoints, before any I/O.
    let error = "postgres:///postgres?user=postgres"
        .parse::<Config>()
        .expect("a hostless URL parses")
        .connect(common::suite_tls())
        .await
        .err()
        .expect("a hostless config must be refused rather than guessing a socket path");
    let chain = common::error_chain(&error);
    assert!(
        chain.contains("host") && chain.contains("missing"),
        "the refusal must name what is absent, got: {chain}"
    );
}

/// `keepalives=0` DISABLES keepalives; the default is on.
///
/// libpq treats the value as a boolean where 0 is off and anything else is on,
/// and this maps it with `keepalives != 0`. That is a truthiness mapping with a
/// default of TRUE behind it, which is the shape that regresses quietly: invert
/// it and every connection silently gains or loses TCP keepalives, with nothing
/// in a test suite to notice, because the parity table only proves the key
/// PARSES.
///
/// `connect.rs` is where the flag has its effect -- it passes
/// `Some(&config.keepalive_config)` to `connect_socket` only when
/// `get_keepalives()` is true -- so the flag below is the whole switch.
#[test]
fn keepalives_zero_means_off_and_the_default_is_on() {
    assert!(
        "host=h"
            .parse::<Config>()
            .expect("bare host")
            .get_keepalives(),
        "keepalives default to on, as in libpq"
    );
    assert!(
        !"host=h keepalives=0"
            .parse::<Config>()
            .expect("keepalives=0")
            .get_keepalives(),
        "keepalives=0 must switch them off"
    );

    // Any non-zero value is on -- the mapping is truthiness, not "== 1".
    for on in ["1", "2", "10"] {
        assert!(
            format!("host=h keepalives={on}")
                .parse::<Config>()
                .unwrap_or_else(|error| panic!("keepalives={on}: {error}"))
                .get_keepalives(),
            "keepalives={on} must be on"
        );
    }

    // A non-numeric value is refused rather than silently read as off, which
    // would be the dangerous reading of a boolean-ish option.
    "host=h keepalives=yes"
        .parse::<Config>()
        .expect_err("a non-numeric keepalives must be refused, not treated as off");
}

/// A NEGATIVE `keepalives` is accepted by libpq and means ON.
///
/// libpq parses this with `strtol` into a signed long and then tests it against
/// zero, so `keepalives=-1` is simply non-zero, i.e. enabled. Measured against
/// the live server on 2026-08-23: `keepalives=-1` connects, exactly as `1`,
/// `2` and `10` do, while `yes` and the empty string are refused with
/// `invalid integer value`.
///
/// Parsing into an UNSIGNED integer instead silently turns that acceptance
/// into a refusal, so a connection string psql accepts fails here. The
/// direction matters: this driver being STRICTER than libpq breaks working
/// DSNs, which is why it is a bug rather than a taste difference.
#[test]
fn negative_keepalives_is_accepted_and_means_on() {
    let config = "host=h keepalives=-1"
        .parse::<Config>()
        .expect("libpq accepts keepalives=-1; refusing it breaks a working DSN");
    assert!(
        config.get_keepalives(),
        "keepalives=-1 is non-zero, so keepalives are ON"
    );

    // The refusals stay refusals -- this must not become "parse anything".
    // NOT `""`: this list carried one until 2026-08-25, on the assumption that
    // an empty value is as malformed as `yes`. Probed, libpq ACCEPTS
    // `keepalives=` and keeps its default, the same as every other numeric
    // option, so the entry was asserting a refusal libpq does not make.
    for bad in ["yes", "1.5"] {
        format!("host=h keepalives={bad}")
            .parse::<Config>()
            .expect_err("libpq refuses this with `invalid integer value`");
    }

    let defaulted = "host=h keepalives="
        .parse::<Config>()
        .expect("libpq accepts an empty numeric value and keeps the default");
    assert!(
        defaulted.get_keepalives(),
        "an empty value leaves the default, which is on"
    );
}

/// An EMPTY ssl file path is refused here and accepted by libpq.
///
/// MEASURED against the live server on 2026-08-23: `sslcert=`, `sslkey=`,
/// `sslrootcert=` and `sslcrl=` with empty values all CONNECT under psql.
/// libpq treats an empty path as unset and falls back to its defaults
/// (`~/.postgresql/postgresql.crt` and friends).
///
/// This driver refuses all three of the first ones, and that is deliberate.
/// The usual argument for accepting -- "match libpq" -- does not carry here,
/// and the usual argument against accepting does not either, so both are
/// recorded:
///
/// * The SAFETY argument for refusing does NOT apply to this driver. libpq
///   would hand you its default client certificate; `tls_rustls.rs` never
///   reads those files at all (`(None, None) => with_no_client_auth()`, and
///   `sslcertmode=require` errors saying so). An empty `sslrootcert` would
///   likewise be `SslRootCert::Unset`, which adds no roots and fails
///   verification LOUDLY rather than silently trusting anything.
/// * What actually justifies refusing is that an empty path is almost always
///   a mistake -- `sslcert=${VAR}` where `VAR` is unset expands to exactly
///   this -- and a config error naming the option beats a TLS failure several
///   steps later, or silently no client authentication at all.
///
/// So this is a divergence taken on purpose. It was previously UNTESTED: a
/// mutation sweep on 2026-08-23 removed the `sslcert` guard and the entire
/// config test set stayed green (268 passed, 0 failed), including the
/// feature-gated `tls_live` target. Whichever way a future reader decides, it
/// should be by changing this test rather than by discovering the guard is
/// load-bearing for nothing.
#[test]
fn an_empty_ssl_path_is_refused_even_though_libpq_accepts_it() {
    // NOTE THE QUOTES, they are load-bearing. In keyword syntax a bare
    // trailing "key=" at end of string is rejected by the LEXER with
    // "unexpected EOF" before any option arm runs, so that spelling tests the
    // tokeniser and never reaches this guard at all -- the first version of
    // this test used it and failed for that reason. "key=''" and the URL form
    // "?key=" are what actually deliver an empty value to the arm; both were
    // measured against libpq, which accepts all of them.
    for key in ["sslcert", "sslkey", "sslrootcert"] {
        let error = format!("host=h {key}=''")
            .parse::<Config>()
            .expect_err("an empty ssl path must be refused");
        assert!(
            a_cause_names(&error, key),
            "the refusal must name {key} so the caller knows which option is \
             empty: {error}"
        );
    }

    // One variable away: the SAME keys with a real path are accepted, so the
    // refusal belongs to the emptiness and not to the key.
    for key in ["sslcert", "sslkey", "sslrootcert"] {
        format!("host=h {key}=/tmp/some-file")
            .parse::<Config>()
            .unwrap_or_else(|error| panic!("{key} with a path must parse: {error}"));
    }
}

/// `sslcrl` and `sslcrldir` ACCEPT an empty path; the other three refuse it.
///
/// Five ssl file-path options, and they deliberately split two ways. The test
/// above pins the strict three. This pins the lenient two, so the split reads
/// as a decision rather than an oversight -- it is exactly the shape that looks
/// like someone forgot a guard.
///
/// The reason they differ: `tls_rustls.rs` states "Preserve sslcrl's libpq
/// behaviour: a file OpenSSL cannot load is ignored", and an empty path is
/// simply a file that cannot be loaded, so it folds into an existing
/// deliberate leniency. The strict three have no such fallback -- this driver
/// never reads libpq's default certificate files -- so for them an empty value
/// can only be a mistake worth naming.
#[test]
fn the_crl_paths_accept_an_empty_value_unlike_the_certificate_paths() {
    for key in ["sslcrl", "sslcrldir"] {
        format!("host=h {key}=''")
            .parse::<Config>()
            .unwrap_or_else(|error| {
                panic!("{key} is lenient about an unloadable path, including empty: {error}")
            });
    }

    // The contrast, in one variable: the certificate paths refuse the same
    // spelling. If a future change made these lenient too, only this half of
    // the pair would still pass, and the split would have silently collapsed.
    for key in ["sslcert", "sslkey", "sslrootcert"] {
        format!("host=h {key}=''")
            .parse::<Config>()
            .expect_err("the certificate paths stay strict");
    }
}

/// Parameters this driver accepts that libpq does NOT.
///
/// The table above rules on libpq's surface. These are the keys we added, and
/// they need their own list for two reasons. Nothing else enumerates them, so
/// one can appear or vanish silently; and a reader looking at a DSN cannot
/// otherwise tell which keys are the reference implementation's and which are
/// ours, which is exactly the confusion the parity table exists to prevent in
/// the other direction.
///
/// Each pairs with a value libpq would consider well formed, so what is being
/// tested is the KEY and not the value parser.
const CRATE_PARAMETERS: &[(&str, &str)] = &[
    // The implicit raw-SQL prepared-statement cache; off by default.
    ("statement_cache_capacity", "32"),
    // Where a `service` is read from. libpq finds this file through the
    // environment; this driver reads no environment, so the path is named.
    ("servicefile", "/tmp/pg_service.conf"),
    // The largest single backend message to accept. libpq imposes no such
    // ceiling, so there is nothing upstream to match.
    ("max_message_size", "134217728"),
];

#[test]
fn every_crate_specific_parameter_is_accepted() {
    let mut wrong = Vec::new();

    for (key, value) in CRATE_PARAMETERS {
        if let Err(error) = format!("host=h {key}={value}").parse::<Config>() {
            wrong.push(format!("{key}: listed as ours but refused ({error})"));
        }
    }

    assert!(
        wrong.is_empty(),
        "crate-specific parameters:\n  {}",
        wrong.join("\n  ")
    );

    // THE CONTROL. "It parsed" is only evidence that the key is recognised if
    // an unrecognised one would NOT have. Without this, a parser that accepted
    // anything would pass the loop above.
    let invented = "host=h definitely_not_a_parameter=1".parse::<Config>();
    assert!(
        invented.is_err(),
        "an invented parameter was accepted, so accepting ours proves nothing"
    );
}

/// None of ours may be a name libpq already uses. If upstream adopts one, it
/// stops being ours and belongs in `LIBPQ_PARAMETERS` with a verdict - and
/// leaving it in both lists would mean two tests disagreeing about who owns
/// the key.
#[test]
fn no_crate_specific_parameter_collides_with_libpq() {
    let collisions: Vec<&str> = CRATE_PARAMETERS
        .iter()
        .map(|(key, _)| *key)
        .filter(|key| LIBPQ_PARAMETERS.iter().any(|(libpq, _, _)| libpq == key))
        .collect();

    assert!(
        collisions.is_empty(),
        "these are listed as crate-specific but libpq accepts them too: {collisions:?}"
    );
}

/// Adding a fourth without ruling on it should fail here, the same way the
/// libpq table's own count guards against a silent addition.
#[test]
fn the_crate_specific_set_is_the_one_that_was_ruled_on() {
    let mut names: Vec<&str> = CRATE_PARAMETERS.iter().map(|(key, _)| *key).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "max_message_size",
            "servicefile",
            "statement_cache_capacity"
        ],
        "the crate-specific parameter set changed without being re-ruled"
    );
}
