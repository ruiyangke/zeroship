//! libpq password-file (`~/.pgpass`) lookup.
//!
//! When a connection supplies no password, libpq reads one from a file of
//! `hostname:port:database:username:password` lines. This is how `psql` users
//! keep credentials out of connection strings, so a driver that ignores the
//! file rejects a setup its users reasonably expect to work.
//!
//! The rules below were DERIVED BY PROBING the review container's libpq -
//! version 18.4, though `psql` there reports 16.14; see
//! `docs/runbooks/compio-postgres-libpq-parameter-probing.md`. Not read off the
//! documentation, because the interesting one is not what the format
//! description implies - see `match_field`.
//!
//! The file is consulted only when the config carries no password, and the
//! first matching line wins.

use std::path::Path;

/// The connection identity a passfile line is matched against.
///
/// `host` is the hostname as the passfile spells it. An explicitly configured
/// Unix socket uses its directory (or `@name` on Linux); `localhost` is only
/// libpq's spelling for an implicit or compiled-default socket directory,
/// neither of which this driver infers. It remains bytes because Unix socket
/// paths are not required to be UTF-8. `port` is stringified because the file
/// matches it as text.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PassfileKey<'a> {
    pub(crate) host: &'a [u8],
    pub(crate) port: &'a str,
    pub(crate) dbname: &'a str,
    pub(crate) user: &'a str,
}

// THE PATH COMES FROM THE CALLER, and this module does not go looking.
//
// libpq resolves an unset `passfile` from `$PGPASSFILE` and then
// `$HOME/.pgpass`. This crate is a standalone, publishable driver
// (`AGENTS.md`, the `libs/` boundary) and a published library takes resolved
// options from its caller rather than reading process configuration - a rule
// the workspace enforces mechanically, in `crates/core/tests/
// config_env_access_gate.rs`, with an exemption list that is deliberately
// empty. Reading `$PGPASSFILE` here was exactly the thing it forbids.
//
// So an application that wants libpq's default locations resolves them itself
// and passes the result to `Config::passfile`. That keeps the decision to read
// a user's environment where it belongs - in the program that has one - and
// keeps this driver's behaviour a function of its arguments.

/// Whether this file's permissions let libpq use it.
///
/// MEASURED: modes 0644, 0640 and 0604 all make libpq ignore the file
/// silently - any group or world bit disqualifies it, so the mask is 0o077 and
/// not merely "world-readable". A directory is refused for the same reason
/// libpq refuses it: it is not a password file.
#[cfg(unix)]
fn permissions_allow_use(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.is_file() && metadata.permissions().mode() & 0o077 == 0
}

/// Non-Unix platforms have no mode bits to check; libpq skips the check there.
#[cfg(not(unix))]
fn permissions_allow_use(metadata: &std::fs::Metadata) -> bool {
    metadata.is_file()
}

/// Match one field of a passfile line against `token`, returning the offset
/// just past the field's terminating `:`.
///
/// THIS IS NOT A SPLIT ON `:`, and that is the whole point of implementing it
/// by hand. libpq compares the line against the token character by character
/// and ends the field at a `:` only once the token is ALREADY EXHAUSTED. So an
/// unescaped colon in the file can legitimately match a colon inside the
/// value: with a role named `od:d`, the line
///
/// ```text
/// 10.0.0.1:5432:db:od:d:secret
/// ```
///
/// matches that role and yields `secret` - MEASURED against the review container's libpq by
/// setting the role's real password to each candidate and seeing which one
/// authenticated. Splitting on the fourth colon would instead read the user as
/// `od` and the password as `d:secret`, match nothing, and send no password.
///
/// A backslash escapes the following character, so `od\:d` spells the same
/// field explicitly and also matches.
///
/// A field consisting of exactly `*` matches anything. It is recognised as
/// `*` immediately followed by `:`, so `*x` is a literal, not a pattern -
/// also measured.
fn match_field(buf: &[u8], token: &[u8]) -> Option<usize> {
    if buf.first() == Some(&b'*') && buf.get(1) == Some(&b':') {
        return Some(2);
    }

    let mut i = 0;
    let mut t = 0;
    let mut escaped = false;

    while i < buf.len() {
        if buf[i] == b'\\' && !escaped {
            i += 1;
            escaped = true;
            if i >= buf.len() {
                return None;
            }
        }
        if buf[i] == b':' && t == token.len() && !escaped {
            return Some(i + 1);
        }
        escaped = false;
        if t == token.len() {
            return None;
        }
        if buf[i] == token[t] {
            i += 1;
            t += 1;
        } else {
            return None;
        }
    }
    None
}

/// Read the password out of the tail of a matched line.
///
/// The password ends at the first UNESCAPED `:` - anything after that is not
/// part of it - and `\` escapes the character it precedes.
fn unescape_password(buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == b'\\' && i + 1 < buf.len() {
            out.push(buf[i + 1]);
            i += 2;
            continue;
        }
        if buf[i] == b':' {
            break;
        }
        out.push(buf[i]);
        i += 1;
    }
    out
}

/// Find `key`'s password in the already-read contents of a password file.
///
/// Separated from the file IO so the matching rules can be tested without a
/// filesystem, and so the permission rule is tested against real files rather
/// than mocked.
pub(crate) fn lookup_in_contents(contents: &[u8], key: PassfileKey<'_>) -> Option<Vec<u8>> {
    for line in contents.split(|byte| *byte == b'\n') {
        // `pg_strip_crlf` removes every trailing CR/LF byte, not just one.
        // `split` consumed the LFs; remove all CRs that immediately preceded
        // them (or ended the final line) without trimming significant spaces.
        let mut line = line;
        while let Some(stripped) = line.strip_suffix(b"\r") {
            line = stripped;
        }
        if line.is_empty() || line.first() == Some(&b'#') {
            continue;
        }

        let Some(after_host) = match_field(line, key.host) else {
            continue;
        };
        let Some(after_port) = match_field(&line[after_host..], key.port.as_bytes()) else {
            continue;
        };
        let after_port = after_host + after_port;
        let Some(after_db) = match_field(&line[after_port..], key.dbname.as_bytes()) else {
            continue;
        };
        let after_db = after_port + after_db;
        let Some(after_user) = match_field(&line[after_db..], key.user.as_bytes()) else {
            continue;
        };
        let after_user = after_db + after_user;

        return Some(unescape_password(&line[after_user..]));
    }
    None
}

/// Look `key` up in the password file at `path`.
///
/// Every failure to produce a password is `None`, never an error: libpq treats
/// an absent, unreadable or too-permissive file as "no password here" and
/// carries on to fail (or succeed) at authentication. Turning any of those
/// into a connection error would reject setups libpq accepts.
pub(crate) fn lookup(path: &Path, key: PassfileKey<'_>) -> Option<Vec<u8>> {
    let metadata = std::fs::metadata(path).ok()?;
    if !permissions_allow_use(&metadata) {
        return None;
    }
    let contents = std::fs::read(path).ok()?;
    lookup_in_contents(&contents, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key<'a>(host: &'a str, port: &'a str, dbname: &'a str, user: &'a str) -> PassfileKey<'a> {
        PassfileKey {
            host: host.as_bytes(),
            port,
            dbname,
            user,
        }
    }

    fn find(contents: &str, key: PassfileKey<'_>) -> Option<String> {
        lookup_in_contents(contents.as_bytes(), key)
            .map(|password| String::from_utf8(password).expect("test passwords are UTF-8"))
    }

    #[test]
    fn an_exact_line_yields_its_password() {
        let found = find(
            "10.0.0.1:5432:db:alice:secret\n",
            key("10.0.0.1", "5432", "db", "alice"),
        );
        assert_eq!(found.as_deref(), Some("secret"));
    }

    /// The control for every match test below: the same file, one field
    /// changed, must NOT match. Without it a matcher that accepts everything
    /// passes the whole suite.
    #[test]
    fn a_line_whose_host_differs_does_not_match() {
        let found = find(
            "10.0.0.1:5432:db:alice:secret\n",
            key("10.0.0.2", "5432", "db", "alice"),
        );
        assert_eq!(found, None);
    }

    #[test]
    fn a_star_field_matches_anything() {
        let found = find("*:*:*:*:secret\n", key("anywhere", "1", "any", "anyone"));
        assert_eq!(found.as_deref(), Some("secret"));
    }

    /// MEASURED against libpq: `*` is a wildcard only as a whole field, so a
    /// field beginning with `*` is a literal and matches nothing else.
    #[test]
    fn a_star_is_not_a_glob() {
        let found = find(
            "*x:5432:db:alice:secret\n",
            key("host", "5432", "db", "alice"),
        );
        assert_eq!(found, None);
    }

    /// The rule a split-on-colon implementation gets wrong. Probed against
    /// the review container's libpq with a real role named `od:d`.
    #[test]
    fn an_unescaped_colon_can_match_a_colon_inside_the_value() {
        let found = find(
            "10.0.0.1:5432:db:od:d:secret\n",
            key("10.0.0.1", "5432", "db", "od:d"),
        );
        assert_eq!(found.as_deref(), Some("secret"));
    }

    /// The same line is AMBIGUOUS, and libpq resolves it both ways: it also
    /// serves the shorter user `od`, with the password `d`.
    ///
    /// This is not a quirk of the implementation - it was measured. With a
    /// role `od` whose real password is `d`, that line connects; change the
    /// role's password and libpq reports authenticating from the file and
    /// failing. A reader who expects one line to name one account will find
    /// this surprising, which is exactly why it is pinned.
    #[test]
    fn an_ambiguous_line_also_serves_the_shorter_user() {
        let found = find(
            "10.0.0.1:5432:db:od:d:secret\n",
            key("10.0.0.1", "5432", "db", "od"),
        );
        assert_eq!(found.as_deref(), Some("d"));
    }

    #[test]
    fn an_escaped_colon_spells_the_same_field() {
        let found = find(
            "10.0.0.1:5432:db:od\\:d:secret\n",
            key("10.0.0.1", "5432", "db", "od:d"),
        );
        assert_eq!(found.as_deref(), Some("secret"));
    }

    #[test]
    fn a_backslash_in_a_password_is_unescaped() {
        let found = find(
            "10.0.0.1:5432:db:alice:se\\\\cret\n",
            key("10.0.0.1", "5432", "db", "alice"),
        );
        assert_eq!(found.as_deref(), Some("se\\cret"));
    }

    #[test]
    fn a_password_ends_at_an_unescaped_colon_but_keeps_an_escaped_one() {
        let plain = find(
            "10.0.0.1:5432:db:alice:se:cret\n",
            key("10.0.0.1", "5432", "db", "alice"),
        );
        assert_eq!(plain.as_deref(), Some("se"));
        let escaped = find(
            "10.0.0.1:5432:db:alice:se\\:cret\n",
            key("10.0.0.1", "5432", "db", "alice"),
        );
        assert_eq!(escaped.as_deref(), Some("se:cret"));
    }

    #[test]
    fn comments_and_blank_lines_are_skipped_and_the_first_match_wins() {
        let contents = "# a comment\n\n\
             10.0.0.1:5432:other:alice:wrong\n\
             10.0.0.1:5432:db:alice:first\n\
             10.0.0.1:5432:db:alice:second\n";
        let found = find(contents, key("10.0.0.1", "5432", "db", "alice"));
        assert_eq!(found.as_deref(), Some("first"));
    }

    #[test]
    fn a_crlf_line_ending_is_not_part_of_the_password() {
        let found = find(
            "10.0.0.1:5432:db:alice:secret\r\n",
            key("10.0.0.1", "5432", "db", "alice"),
        );
        assert_eq!(found.as_deref(), Some("secret"));
    }

    #[test]
    fn every_trailing_carriage_return_is_stripped() {
        let found = find(
            "10.0.0.1:5432:db:alice:secret\r\r\n",
            key("10.0.0.1", "5432", "db", "alice"),
        );
        assert_eq!(found.as_deref(), Some("secret"));
    }

    #[test]
    fn a_file_with_no_matching_line_yields_nothing() {
        let found = find(
            "10.0.0.1:5432:db:bob:secret\n",
            key("10.0.0.1", "5432", "db", "alice"),
        );
        assert_eq!(found, None);
    }

    /// A line missing its password field has no fifth field to return. It must
    /// not match with an empty password, which would send one the user never
    /// wrote.
    #[test]
    fn a_truncated_line_does_not_match() {
        let found = find(
            "10.0.0.1:5432:db:alice\n",
            key("10.0.0.1", "5432", "db", "alice"),
        );
        assert_eq!(found, None);
    }

    #[test]
    fn a_trailing_backslash_does_not_run_off_the_line() {
        let found = find(
            "10.0.0.1:5432:db:alice\\",
            key("10.0.0.1", "5432", "db", "alice"),
        );
        assert_eq!(found, None);
    }
}
