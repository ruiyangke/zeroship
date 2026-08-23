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
