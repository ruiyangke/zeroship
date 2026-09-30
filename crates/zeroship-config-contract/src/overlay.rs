//! Every declared overlay path must be a leaf the shared typed overlay accepts.
//!
//! THE SOURCE IS THE COMPILED CONTRACT, as in [`crate::docs`]. The expectation
//! is not a list of setting names kept beside the overlay structs; it is
//! [`ConfigSpec::toml_path`] on every declaration rustc linked into this
//! binary. A hand-maintained list is how the `workflow.capacity_*` family
//! drifted out of [`FileConfig`] in the first place, so a check restating the
//! names would rot the same way.
//!
//! WHY A MISSING FIELD IS NOT MERELY UNTIDY. Every section of [`FileConfig`] is
//! `#[serde(deny_unknown_fields)]`, so the typed overlay does not ignore a key
//! it has no field for - it REFUSES THE PARSE, and the process fails to boot.
//! Declaring a setting in `zeroship_config` is therefore not enough to make it
//! supplyable from the shared TOML: the resolver walks the overlay by canonical
//! path, but the typed section has to admit the key first.
//!
//! WHAT THIS DOES NOT CHECK, deliberately: the other direction. A field on a
//! section with no declaration behind it is inert rather than fatal, and
//! [`crate::contract::validate_contract`] already owns declared-but-unread.
//! Bootstrap and command controls have no overlay tier at all
//! ([`ConfigSpec::toml_path`] returns `None`), so they are absent from the
//! population by construction rather than by exemption.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use zeroship_core::config::{ConfigSpec, FileConfig};

/// The substring `deny_unknown_fields` puts in its rejection.
///
/// This is `serde::de::Error::unknown_field`'s own wording as the TOML
/// deserializer renders it, not a spelling searched for in source text: the
/// probe runs a real parse and classifies the real diagnostic. The module's
/// unit tests pin all three verdicts against live parses, so a wording change
/// fails the control rather than silently reclassifying every gap as benign.
const UNKNOWN_FIELD: &str = "unknown field";

/// A declared setting the shared typed overlay would refuse.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum OverlayError {
    /// No declarations were extracted, so the walk below would inspect nothing.
    #[error("overlay audit extracted zero ConfigSpec declarations")]
    EmptySpecs,
    /// Declarations exist but none has an overlay tier, so nothing was probed.
    #[error("no declaration carries an overlay path; the audit checked nothing")]
    NoOverlayPaths,
    /// A declared overlay path is not a field of the typed section it names.
    #[error(
        "the shared typed overlay refuses `{path}`, declared by {consumers}: \
         {reason}. An overlay naming it does not ignore it - the process \
         refuses to boot. Add the field to the matching section of \
         zeroship-core/src/config/file.rs"
    )]
    Refused {
        /// Canonical overlay path, as `ConfigSpec::toml_path` projects it.
        path: String,
        /// Every binary declaring the setting, comma separated.
        consumers: String,
        /// The deserializer's own rejection, without its `expected one of` tail.
        reason: String,
    },
}

/// How [`FileConfig`] answered a minimal overlay naming exactly one setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The overlay deserialized: the section admits the key.
    Accepted,
    /// `deny_unknown_fields` rejected the key. No field carries this setting.
    UnknownField(String),
    /// The key was admitted and only the synthesised VALUE was refused.
    ///
    /// This counts as present. [`literal`] picks a value from the declared Rust
    /// type rather than from the field, so a type it does not recognise - or an
    /// enum whose variants it cannot know - must not be reported as a missing
    /// field. Erring this way keeps the check free of a type table that would
    /// need maintaining alongside the one thing it is auditing.
    ValueRejected(String),
}

/// A TOML literal of a shape the declared Rust type can hold.
///
/// Only the shape matters, never the value: the probe asks whether the key is
/// admitted. An unrecognised type falls through to a string, and a wrong guess
/// surfaces as [`Verdict::ValueRejected`] rather than as a gap.
fn literal(rust_type: &str) -> &'static str {
    // `stringify!` on the declaration's interpolated tokens spaces path
    // separators and generics apart, so `std :: path :: PathBuf` and
    // `Vec < String >` both arrive here.
    let compact = rust_type
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    let outer = compact
        .split_once('<')
        .map_or(compact.as_str(), |(head, _)| head);
    match outer.rsplit("::").next().unwrap_or(outer) {
        "Vec" => "[]",
        "bool" => "true",
        "u8" | "u16" | "u32" | "u64" | "usize" | "i8" | "i16" | "i32" | "i64" | "isize" => "0",
        _ => "\"\"",
    }
}

/// The smallest overlay document naming exactly `path`.
///
/// A dotted path becomes a table header plus its leaf, so `workflow.listen`
/// exercises the `[workflow]` section and a bare path exercises the root of
/// [`FileConfig`]. A path with more than one dot yields a dotted header, which
/// is the same traversal TOML gives the resolver.
fn overlay(path: &str, rust_type: &str) -> String {
    let value = literal(rust_type);
    match path.rsplit_once('.') {
        Some((table, leaf)) => format!("[{table}]\n{leaf} = {value}\n"),
        None => format!("{path} = {value}\n"),
    }
}

/// The deserializer's rejection without its `expected one of` tail.
///
/// The tail names every field of the section, which for [`FileConfig`] is long
/// enough to bury the rest of a multi-setting failure. The kept half carries
/// the refused key, which is the part a reader acts on.
fn reason(error: &toml::de::Error) -> String {
    let rendered = error.to_string();
    let line = rendered
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(&rendered)
        .trim();
    line.split_once(", expected")
        .map_or(line, |(head, _)| head)
        .to_owned()
}

/// Deserialize a minimal overlay naming `path` and classify the answer.
#[must_use]
pub fn probe(path: &str, rust_type: &str) -> Verdict {
    match toml::from_str::<FileConfig>(&overlay(path, rust_type)) {
        Ok(_) => Verdict::Accepted,
        Err(error) => {
            let reason = reason(&error);
            if reason.contains(UNKNOWN_FIELD) {
                Verdict::UnknownField(reason)
            } else {
                Verdict::ValueRejected(reason)
            }
        }
    }
}

/// Every declared overlay path, merged across the binaries that declare it.
///
/// Keyed by path so a setting several binaries share is probed once. The Rust
/// type is taken from the first declaration; consumers of one identity must
/// already agree on it, and [`crate::contract::validate_contract`] is the gate
/// that says so.
fn declared_overlay_paths(specs: &[ConfigSpec]) -> BTreeMap<&str, (&str, BTreeSet<&str>)> {
    let mut paths: BTreeMap<&str, (&str, BTreeSet<&str>)> = BTreeMap::new();
    for spec in specs {
        let Some(path) = spec.toml_path() else {
            continue;
        };
        let entry = paths
            .entry(path)
            .or_insert_with(|| (spec.rust_type(), BTreeSet::new()));
        for consumer in spec.consumers() {
            entry.1.insert(consumer.target());
        }
    }
    paths
}

/// Assert that the shared typed overlay admits every declared overlay path.
///
/// # Errors
///
/// Returns the two anti-vacuity errors, then one [`OverlayError::Refused`] per
/// declared path the typed overlay has no field for.
pub fn validate_overlay_paths(specs: &[ConfigSpec]) -> Result<(), Vec<OverlayError>> {
    if specs.is_empty() {
        return Err(vec![OverlayError::EmptySpecs]);
    }
    let paths = declared_overlay_paths(specs);
    if paths.is_empty() {
        return Err(vec![OverlayError::NoOverlayPaths]);
    }

    let mut errors = Vec::new();
    for (path, (rust_type, consumers)) in paths {
        if let Verdict::UnknownField(reason) = probe(path, rust_type) {
            errors.push(OverlayError::Refused {
                path: path.to_owned(),
                consumers: consumers.into_iter().collect::<Vec<_>>().join(", "),
                reason,
            });
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::{literal, probe, validate_overlay_paths, OverlayError, Verdict};
    use zeroship_core::config::{CanonicalName, ConfigSpec, Consumer};

    const WORKFLOW: Consumer = Consumer::new("zeroship-workflow-server", "workflow");

    #[test]
    fn the_classifier_separates_a_missing_field_from_a_refused_value() {
        // THE CONTROL, and the reason it is three arms rather than one: the
        // audit reports a gap only for `UnknownField`, so a classifier that
        // collapsed a type error into that arm would invent gaps, and one that
        // collapsed a missing field into `ValueRejected` would report none at
        // all. Both failures print an empty error list, which is what a passing
        // run looks like. Each arm is a live parse of a real overlay.
        assert_eq!(
            probe("workflow.http_threads", "usize"),
            Verdict::Accepted,
            "an existing leaf with a well-shaped value must be accepted"
        );

        let missing = probe("workflow.no_such_setting", "String");
        assert!(
            matches!(missing, Verdict::UnknownField(ref reason) if reason.contains("no_such_setting")),
            "deny_unknown_fields must reject a leaf no field carries: {missing:?}"
        );

        let wrong_shape = probe("workflow.http_threads", "String");
        assert!(
            matches!(wrong_shape, Verdict::ValueRejected(_)),
            "an existing leaf given the wrong value shape is PRESENT, not \
             missing: {wrong_shape:?}"
        );
    }

    #[test]
    fn an_absent_section_is_reported_as_a_refusal_too() {
        // A component with no section at all fails at the table rather than at
        // the leaf. It must still surface, or a whole component's settings could
        // be unreachable while every leaf inside it went unprobed.
        let missing = probe("no_such_component.setting", "String");
        assert!(
            matches!(missing, Verdict::UnknownField(ref reason) if reason.contains("no_such_component")),
            "an overlay naming an absent section must be refused: {missing:?}"
        );
    }

    #[test]
    fn a_root_setting_is_probed_at_the_overlay_root() {
        // A canonical name with no scope prefix projects to a root key, not to a
        // section. Building `[blob_store]` instead would probe a table that does
        // not exist and report every shared setting as a gap.
        assert_eq!(probe("blob_store", "String"), Verdict::Accepted);
        assert!(matches!(
            probe("no_such_root_setting", "String"),
            Verdict::UnknownField(_)
        ));
    }

    #[test]
    fn the_literal_shape_follows_the_declared_type_through_macro_spacing() {
        assert_eq!(literal("usize"), "0");
        assert_eq!(literal("i64"), "0");
        assert_eq!(literal("bool"), "true");
        assert_eq!(literal("Vec < String >"), "[]");
        assert_eq!(literal("std :: path :: PathBuf"), "\"\"");
        assert_eq!(literal("String"), "\"\"");
        // An unknown type falls through to a string rather than to a gap.
        assert_eq!(literal("OriginScheme"), "\"\"");
        // The collection literal has to be admitted by a real struct field, or
        // every `Vec` setting would read as refused.
        assert_eq!(
            probe("trusted_origins", "Vec < TrustedOrigin >"),
            Verdict::Accepted
        );
    }

    #[test]
    fn an_audit_over_nothing_fails_rather_than_passing() {
        assert_eq!(
            validate_overlay_paths(&[]),
            Err(vec![OverlayError::EmptySpecs])
        );
        // Controls have no overlay tier, so a registry of nothing but controls
        // probes zero paths. Passing on that would make the audit vacuous for
        // any binary whose overlay settings were all deleted.
        let control = ConfigSpec::command(
            CanonicalName::from_static("check_config"),
            &[WORKFLOW],
            "check_config",
            "check_config",
            "bool",
            None,
        );
        assert_eq!(
            validate_overlay_paths(&[control]),
            Err(vec![OverlayError::NoOverlayPaths])
        );
    }

    #[test]
    fn a_declared_path_with_no_field_is_named_with_its_declaring_binary() {
        let orphan = ConfigSpec::operational(
            CanonicalName::from_static("workflow.no_such_setting"),
            &[WORKFLOW],
            "no_such_setting",
            "no_such_setting",
            "String",
            Some("String::new()"),
        );
        let errors = validate_overlay_paths(&[orphan]).expect_err("an orphan path must fail");
        assert_eq!(errors.len(), 1);
        let OverlayError::Refused {
            path, consumers, ..
        } = &errors[0]
        else {
            panic!("expected a refusal, got {:?}", errors[0]);
        };
        assert_eq!(path, "workflow.no_such_setting");
        assert_eq!(consumers, "zeroship-workflow-server");
    }
}
