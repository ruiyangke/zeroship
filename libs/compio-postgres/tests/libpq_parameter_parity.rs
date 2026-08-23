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

use compio_postgres::Config;

#[allow(dead_code)]
mod common;

/// What this driver is expected to do with a libpq parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// We implement it.
    Accepted,
    /// We deliberately do not, and say so naming the key.
    Refused,
}

use Verdict::{Accepted, Refused};

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
    ("gssdelegation", "0", Refused),
    ("gssencmode", "disable", Refused),
    ("gsslib", "gssapi", Refused),
    ("hostaddr", "127.0.0.1", Accepted),
    ("keepalives", "1", Accepted),
    ("keepalives_count", "3", Accepted),
    ("keepalives_idle", "10", Accepted),
    ("keepalives_interval", "5", Accepted),
    ("krbsrvname", "postgres", Refused),
    ("load_balance_hosts", "disable", Accepted),
    ("max_protocol_version", "3.0", Refused),
    ("min_protocol_version", "3.0", Refused),
    ("oauth_client_id", "id", Refused),
    ("oauth_client_secret", "secret", Refused),
    ("oauth_issuer", "https://example.test", Refused),
    ("oauth_scope", "scope", Refused),
    // Space-free on purpose: in keyword syntax an unquoted value ends at the
    // first space, so `-c geqo=off` would test the DSN lexer, not this key.
    ("options", "-cgeqo=off", Accepted),
    ("passfile", "/tmp/pgpass", Refused),
    ("password", "p", Accepted),
    ("port", "5432", Accepted),
    ("replication", "false", Accepted),
    ("require_auth", "password", Accepted),
    ("requirepeer", "postgres", Accepted),
    ("requiressl", "0", Refused),
    ("scram_client_key", "key", Refused),
    ("scram_server_key", "key", Refused),
    ("service", "svc", Refused),
    ("ssl_max_protocol_version", "TLSv1.3", Accepted),
    ("ssl_min_protocol_version", "TLSv1.2", Accepted),
    ("sslcert", "/tmp/client.crt", Accepted),
    ("sslcertmode", "allow", Accepted),
    ("sslcompression", "0", Refused),
    ("sslcrl", "/tmp/root.crl", Accepted),
    ("sslcrldir", "/tmp/crls", Accepted),
    ("sslkey", "/tmp/client.key", Accepted),
    ("sslkeylogfile", "/tmp/keys.log", Refused),
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
                wrong.push(format!("{key}: expected to be implemented, refused ({error})"));
            }
            (Refused, Ok(_)) => wrong.push(format!(
                "{key}: accepted but unimplemented, so it is silently ignored"
            )),
        }
    }

    assert!(wrong.is_empty(), "libpq parameter parity:\n  {}", wrong.join("\n  "));
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
    assert_eq!(listed.get_hosts().len(), 2, "the comma form must stay multi-host");

    // And a repeated key after a comma list replaces the whole list.
    let overridden = "host=a,b host=c user=u".parse::<Config>().expect("override");
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
            Err(error) => wrong.push(format!("{dsn}: libpq reads {expected:?}, we refuse ({error})")),
        }
    }
    assert!(wrong.is_empty(), "value lexing diverges from libpq:\n  {}", wrong.join("\n  "));

    // The known divergence, pinned so it cannot change unnoticed in either
    // direction. libpq yields the empty string for both of these.
    for trailing in ["host=h application_name=", "host=h user="] {
        assert!(
            trailing.parse::<Config>().is_err(),
            "a trailing empty value is refused here and accepted by libpq; if this \
             now parses, the divergence was closed and this test should say so"
        );
    }
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
    let mixed = "postgres://a,b:2/db".parse::<Config>().expect("mixed ports");
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
    use compio_postgres::NoTls;

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
        .connect(NoTls)
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
    for bad in ["yes", "", "1.5"] {
        format!("host=h keepalives={bad}")
            .parse::<Config>()
            .expect_err("libpq refuses this with `invalid integer value`");
    }
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
