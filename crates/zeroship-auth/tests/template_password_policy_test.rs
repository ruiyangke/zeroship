//! The `minlength` a password form advertises must equal the minimum the
//! server enforces.
//!
//! `MIN_PASSWORD_CHARS` is the one value, and both handlers read it. A template
//! cannot import a constant, so the two `minlength` attributes are the only
//! places the policy is still written out by hand - which makes them the only
//! places it can drift.
//!
//! Drift is not cosmetic in either direction. Lower attribute than check: the
//! form happily submits an 8-character password and the server answers "at
//! least 15 characters", so the page contradicts itself in the one interaction
//! where a user is already unsure what is wanted. Higher: the advertised policy
//! is stricter than the one actually enforced, so the promise is false.
//!
//! WHAT THIS DOES NOT COVER:
//!
//! - That the ERROR TEXT quotes the same number. Both handlers currently write
//!   the sentence out by hand, so a change to `MIN_PASSWORD_CHARS` alone would
//!   leave them saying "15" while enforcing something else. Asserting that
//!   would mean either parsing prose or formatting the message from the
//!   constant; the latter is the real fix and is not done here.
//! - Any password rule other than length.
//! - That `minlength` is honoured by the browser. It is HTML validation and
//!   trivially bypassed, which is exactly why the server check exists and is
//!   the thing under test on the Rust side.

use std::fs;
use std::path::Path;

use zeroship_auth::identity::password::MIN_PASSWORD_CHARS;

/// Templates that collect a NEW password. Both must advertise the policy.
const PASSWORD_FORMS: [&str; 2] = ["signup.html", "reset.html"];

#[test]
fn password_forms_advertise_the_enforced_minimum() {
    let templates = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui/templates");
    let expected = format!(r#"minlength="{MIN_PASSWORD_CHARS}""#);

    for name in PASSWORD_FORMS {
        let path = templates.join(name);
        let body = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));

        // The form must actually collect a new password - otherwise the
        // assertion below could pass on a file that had quietly stopped being a
        // password form at all.
        assert!(
            body.contains(r#"autocomplete="new-password""#),
            "{name} is listed as a password form but collects no new password"
        );
        assert!(
            body.contains(&expected),
            "{name} must advertise {expected}, the minimum \
             identity::password::MIN_PASSWORD_CHARS enforces"
        );
    }
}
