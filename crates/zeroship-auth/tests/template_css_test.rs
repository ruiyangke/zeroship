//! Every class an auth template writes must exist in the auth stylesheet.
//!
//! WHY. `consent.html` has written `class="scope-desc"` on the sentence that
//! tells a user what an app is asking permission to do, and `style.css` never
//! had a rule for it, so the description ran inline straight after the scope
//! label: "See invoices See invoices and plan." on one line. Nothing failed.
//! The neighbouring `.scope-tag`, which only marks a scope "(unrecognized)",
//! WAS styled - the less important of the two was the one that got a rule.
//!
//! A missing CSS class produces no error anywhere. The browser drops the
//! selector, the page still renders, and the only signal is that it looks
//! slightly wrong on a screen most reviewers never reach (the consent screen
//! needs a real OAuth client mid-flow).
//!
//! WHAT THIS DOES NOT CATCH, and it is a lot:
//!
//! - **Context-restricted rules.** This asks only whether `.name` appears
//!   somewhere in the sheet. `.buttons button.primary` satisfies a check for
//!   `primary` while NOT matching a `class="primary"` button outside
//!   `.buttons` - which is exactly the bug that was live in `logout.html` and
//!   `device.html`. Only a browser computing styles can see that; the
//!   `tests/e2e_auth_ui/` tier is where a computed value gets asserted.
//! - Classes built by string interpolation in Rust handler code.
//! - Whether a rule that exists is the RIGHT rule.
//! - Dead rules in the sheet that no template uses (the other direction).

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Classes deliberately written with no rule of their own.
///
/// Each is a semantic hook whose appearance comes from somewhere else. They are
/// listed rather than tolerated silently, so "no rule" reads as a decision:
///
/// - `auth-shell` - the page wrapper. The layout (max-width, centring, padding)
///   lives on `body`, so a duplicate rule here would be a second place to
///   change the same thing.
/// - `oauth-google`, `oauth-github` - per-provider hooks on buttons that
///   `.oauth-button` already styles completely. The neutral treatment is
///   deliberate: each button carries its provider's own SVG mark and label, and
///   the shared style is theme-aware, which hardcoded brand colours would not
///   be in dark mode.
const UNSTYLED_HOOKS: [&str; 3] = ["auth-shell", "oauth-google", "oauth-github"];

#[test]
fn every_template_class_has_a_stylesheet_rule() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let css = fs::read_to_string(crate_dir.join("static/style.css")).expect("read style.css");
    let templates = crate_dir.join("src/ui/templates");

    let mut used: BTreeSet<String> = BTreeSet::new();
    for entry in fs::read_dir(&templates).expect("read templates dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let body = fs::read_to_string(&path).expect("read template");
        for (_, rest) in body.match_indices(r#"class=""#).map(|(i, m)| (i, &body[i + m.len()..])) {
            let Some(value) = rest.split('"').next() else { continue };
            // Skip Askama-interpolated values: the class is decided at runtime
            // and there is no literal name to look for.
            if value.contains('{') {
                continue;
            }
            used.extend(value.split_whitespace().map(str::to_owned));
        }
    }

    // A scan that found nothing would pass silently, which is the one way this
    // guard could rot into a no-op. Require that it saw the templates first.
    assert!(
        used.len() >= 15,
        "expected to find at least 15 distinct classes, found {}: {used:?}",
        used.len()
    );

    let missing: Vec<&String> = used
        .iter()
        .filter(|c| !UNSTYLED_HOOKS.contains(&c.as_str()))
        .filter(|c| !css.contains(&format!(".{c}")))
        .collect();
    assert!(
        missing.is_empty(),
        "classes used in a template with no rule in static/style.css: {missing:?}\n\
         Add a rule, or add the name to UNSTYLED_HOOKS with the reason it needs none."
    );
}
