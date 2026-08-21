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
    ("requirepeer", "postgres", Refused),
    ("requiressl", "0", Refused),
    ("scram_client_key", "key", Refused),
    ("scram_server_key", "key", Refused),
    ("service", "svc", Refused),
    ("ssl_max_protocol_version", "TLSv1.3", Accepted),
    ("ssl_min_protocol_version", "TLSv1.2", Accepted),
    ("sslcert", "/tmp/client.crt", Accepted),
    ("sslcertmode", "allow", Refused),
    ("sslcompression", "0", Refused),
    ("sslcrl", "/tmp/root.crl", Accepted),
    ("sslcrldir", "/tmp/crls", Accepted),
    ("sslkey", "/tmp/client.key", Accepted),
    ("sslkeylogfile", "/tmp/keys.log", Refused),
    ("sslmode", "prefer", Accepted),
    ("sslnegotiation", "postgres", Accepted),
    ("sslpassword", "pw", Accepted),
    ("sslrootcert", "/tmp/root.crt", Accepted),
    ("sslsni", "1", Refused),
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
