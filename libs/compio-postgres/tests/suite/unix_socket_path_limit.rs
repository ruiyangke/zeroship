//! A Unix socket path that does not fit must be REFUSED, not truncated.
//!
//! `sockaddr_un.sun_path` is a fixed array - 108 bytes on Linux - and the
//! failure mode that matters is not the error, it is the absence of one. A
//! path silently cut to fit still names a valid socket, just a different one,
//! so the driver would connect to whatever happens to answer there and report
//! success. Nothing downstream could tell.
//!
//! This is reachable in ordinary use: a socket directory under a deep
//! temporary path exceeds the limit easily. The fixture this crate ships for
//! Unix-socket testing lives at such a path, which is how the case was found.

#[allow(unused_imports)]
use crate::common;
use compio_postgres::{Config, NoTls};

/// Longer than `sun_path` on every platform that has one, and made of a
/// component that could not exist, so a truncation cannot accidentally match
/// a real socket during the test.
fn overlong_socket_dir() -> String {
    format!("/tmp/{}", "zz_overlong_socket_dir_segment/".repeat(8))
}

#[compio::test]
async fn an_overlong_socket_path_is_refused_rather_than_truncated() {
    let dir = overlong_socket_dir();
    assert!(
        dir.len() > 108,
        "this case needs a path past sun_path; got {} bytes",
        dir.len()
    );

    let dsn = format!("host={dir} user=postgres dbname=postgres");
    let config: Config = dsn
        .parse()
        .expect("an absolute host path parses as a socket dir");
    match config.get_hosts().first() {
        Some(compio_postgres::config::Host::Unix(path)) => {
            assert_eq!(
                path.to_string_lossy(),
                dir,
                "the path was altered while parsing"
            );
        }
        other => panic!("an absolute host must parse as a Unix socket dir, got {other:?}"),
    }

    let error = match compio_postgres::connect(&dsn, NoTls).await {
        Err(error) => error,
        Ok(_) => panic!("a socket path past sun_path was connected"),
    };

    // The point is that it FAILED. Naming the limit is what makes the failure
    // actionable rather than a puzzle, so check that too.
    let mut chain = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        chain.push_str(" | ");
        chain.push_str(&cause.to_string());
        source = std::error::Error::source(cause);
    }
    assert!(
        chain.to_lowercase().contains("sun_len")
            || chain.to_lowercase().contains("too long")
            || chain.to_lowercase().contains("shorter"),
        "the refusal does not explain that the path is too long, so a caller \
         cannot tell it from an ordinary connection failure: {chain}"
    );
}

/// THE CONTROL. A SHORT path that simply has no socket must fail differently -
/// with a not-found, not a length complaint. Without this the test above would
/// pass for a driver that refused every Unix path.
#[compio::test]
async fn a_short_socket_path_fails_for_a_different_reason() {
    let dsn = "host=/tmp/zz_no_socket_here user=postgres dbname=postgres";
    let error = match compio_postgres::connect(dsn, NoTls).await {
        Err(error) => error,
        Ok(_) => panic!("there is no socket at that path, yet it connected"),
    };

    let mut chain = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        chain.push_str(" | ");
        chain.push_str(&cause.to_string());
        source = std::error::Error::source(cause);
    }
    assert!(
        !chain.to_lowercase().contains("sun_len") && !chain.to_lowercase().contains("shorter"),
        "a short path was rejected for length, so the length check is not \
         measuring length: {chain}"
    );
}
