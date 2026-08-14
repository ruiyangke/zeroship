//! Every error banner in the auth UI must be an announced, addressable alert.
//!
//! `ui::login::tests::login_page_error_is_announced_and_describes_the_fields`
//! pins the RENDERED contract for the login page, including the conditional
//! `aria-invalid` wiring, which is the part that actually needs a render to
//! check. This file is the cheap breadth companion: it scans every template so
//! the other ten banners cannot quietly drift back to a bare `<div>`.
//!
//! WHY IT MATTERS. Measured in real Chromium against a live auth server, a
//! failed sign-in produced this accessibility tree:
//!
//! ```text
//! - generic [ref=e7]: invalid email or password
//! - textbox "Email" [active] [ref=e10]
//! - textbox "Password" [ref=e12]
//! ```
//!
//! A bare `generic` node is not announced. A screen-reader user who submitted a
//! wrong password got no signal that anything had failed.
//!
//! WHAT THIS DOES NOT CATCH, and it is most of what could go wrong:
//!
//! - Whether a real screen reader voices a `role="alert"` present at first
//!   paint. That varies by AT and browser and no source scan can decide it.
//!   The browser-tier spec in `tests/e2e_auth_ui/specs/a11y.spec.ts` is where
//!   that behaviour is exercised.
//! - An error rendered by Rust handler code rather than a template.
//! - Whether `id="form-error"` is actually REFERENCED by a field on each page.
//!   Only login/signup/forgot/reset wire `aria-describedby` today; the
//!   remaining banners are informational pages with no field to blame.
//! - Duplicate ids, if a page ever renders two banners at once.

use std::fs;
use std::path::Path;

/// The banner shape every template must use.
const REQUIRED: [&str; 2] = [r#"role="alert""#, r#"id="form-error""#];

#[test]
fn every_error_banner_is_an_addressable_alert() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui/templates");
    let mut checked = Vec::new();
    let mut offenders = Vec::new();

    for entry in fs::read_dir(&dir).expect("read templates dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let body = fs::read_to_string(&path).expect("read template");
        if !body.contains(r#"class="error""#) {
            continue;
        }
        let name = path.file_name().expect("file name").to_string_lossy().to_string();
        checked.push(name.clone());
        for needle in REQUIRED {
            if !body.contains(needle) {
                offenders.push(format!("{name} is missing {needle}"));
            }
        }
    }

    // A scan that matched nothing would pass silently, which is the one way this
    // guard could rot into a no-op: a renamed class, a moved directory, a
    // refactor that stops using `class="error"`. Require that it FOUND banners
    // before believing that it approved of them.
    assert!(
        checked.len() >= 10,
        "expected to scan at least 10 templates carrying an error banner, found {}: {checked:?}",
        checked.len()
    );
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
}
