//! Render the zeroship-owned half of `docs/reference/env-vars.md`.
//!
//! THE SOURCE IS THE COMPILED CONTRACT. Every row below comes from a
//! [`ConfigSpec`] that the `zeroship_config` proc macro emitted and rustc
//! linked into this binary - not from a scan of the source text, and not from
//! a hand-maintained list. A name in this document therefore cannot be
//! anything other than the name the declaring binary parses, because both are
//! the same compiled value.
//!
//! The syn-based [`crate::inventory`] scan stays, and its job is to DISAGREE.
//! It re-derives the same projections from the source text with its own parser
//! and its own `project_flag`/`project_env` implementations
//! (`inventory.rs:704-720`), so the `audit` subcommand comparing the two sets
//! is two independent derivations of one answer rather than one code path
//! agreeing with itself. They share the source files on disk and the `syn`
//! crate; they share no parsing, no projection and no registry code.
//!
//! WHAT THIS DOES NOT GENERATE, deliberately: everything zeroship does not
//! declare. Stripe, Supabase, GoTrue, Lago and OpenMeter names, the Compose
//! `.env` interpolation surface, the JS packages' `process.env` reads and the
//! test-only names are real and are not in any `ConfigSpec`, so they live in a
//! hand-maintained section outside the markers and this module never touches
//! them. Splicing is bounded by [`BEGIN_MARKER`] and [`END_MARKER`] for exactly
//! that reason.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use thiserror::Error;
use zeroship_core::config::{ConfigSpec, SupplyClass};

/// Opening fence of the generated region.
pub const BEGIN_MARKER: &str = "<!-- BEGIN GENERATED CONFIGURATION CONTRACT -->";
/// Closing fence of the generated region.
pub const END_MARKER: &str = "<!-- END GENERATED CONFIGURATION CONTRACT -->";

/// A failure to splice or verify the generated region.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DocError {
    /// The document carries no generated region.
    #[error("{path} has no {marker} line; the generated region cannot be located")]
    MissingMarker {
        /// Document path.
        path: String,
        /// The marker that is absent.
        marker: &'static str,
    },
    /// The markers are present but in the wrong order.
    #[error("{path} closes the generated region before it opens it")]
    InvertedMarkers {
        /// Document path.
        path: String,
    },
    /// The committed region differs from what the contract renders now.
    #[error(
        "{path} is stale: the committed contract region differs from the compiled \
         contract. Regenerate with `cargo run -p zeroship-config-contract -- env-vars-doc`"
    )]
    Stale {
        /// Document path.
        path: String,
    },
    /// Rendering produced nothing, so a comparison would be vacuous.
    #[error("the compiled contract rendered zero settings; nothing was checked")]
    EmptyRender,
}

/// One canonical identity, merged across every binary that declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    /// Canonical identity.
    pub canonical: String,
    /// Wrapper-selected supply class.
    pub class: SupplyClass,
    /// Canonical environment projection, when the class has an env tier.
    pub env: Option<String>,
    /// Canonical overlay path, when the class has an overlay tier.
    pub toml: Option<String>,
    /// Resolved inner Rust type.
    pub rust_type: String,
    /// Compiled default, when one exists.
    pub default: Option<String>,
    /// Every consumer, with the flag that consumer spells.
    pub consumers: Vec<(String, String)>,
}

impl Setting {
    /// The section this setting is filed under.
    ///
    /// A dotted canonical name is owned by its prefix; a bare one is shared by
    /// construction, because the only reason to leave the scope off is that
    /// more than one binary reads it.
    #[must_use]
    pub fn group(&self) -> &str {
        self.canonical
            .split_once('.')
            .map_or("shared", |(prefix, _)| prefix)
    }
}

/// Human label for a supply class, in the vocabulary the reference uses.
const fn class_label(class: SupplyClass) -> &'static str {
    match class {
        SupplyClass::Operational => "operational",
        SupplyClass::Secret => "secret",
        SupplyClass::Bootstrap => "bootstrap control",
        SupplyClass::Command => "command control",
    }
}

/// Merge a flat spec list into one row per canonical identity.
///
/// Consumers of a shared identity are merged rather than repeated. The
/// properties they must agree on are checked by
/// [`crate::contract::validate_contract`], so this takes the first spec's word
/// for them; a disagreement fails that gate, not this renderer.
#[must_use]
pub fn collect(specs: &[ConfigSpec]) -> Vec<Setting> {
    let mut merged: BTreeMap<String, Setting> = BTreeMap::new();
    for spec in specs {
        let canonical = spec.canonical().as_str().to_owned();
        let entry = merged.entry(canonical.clone()).or_insert_with(|| Setting {
            canonical,
            class: spec.class(),
            env: spec.env_name(),
            toml: spec.toml_path().map(ToOwned::to_owned),
            rust_type: spec.rust_type().to_owned(),
            default: spec.default().map(ToOwned::to_owned),
            consumers: Vec::new(),
        });
        for consumer in spec.consumers() {
            let flag = spec
                .flag_name(*consumer)
                .map_or_else(|| "-".to_owned(), |flag| format!("--{flag}"));
            entry
                .consumers
                .push((consumer.target().to_owned(), flag));
        }
    }
    for setting in merged.values_mut() {
        setting.consumers.sort();
        setting.consumers.dedup();
    }
    merged.into_values().collect()
}

/// A markdown cell that never renders as an empty table column.
fn cell(value: Option<&String>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| format!("`{value}`"))
}

/// Render a compiled default as the VALUE an operator would supply.
///
/// The registry stores the default as the stringified Rust expression the
/// declaration wrote, so `blob_store` arrives as `"./bundles".to_owned()` and
/// `CheckFormat::Text` arrives with the spaces `quote!` puts around `::`. This
/// undoes exactly those two presentational artifacts and nothing else; an
/// expression it does not recognise is printed verbatim rather than guessed at,
/// because a wrong default in a reference is worse than an ugly one.
fn default_cell(value: Option<&String>) -> String {
    let Some(value) = value else {
        return "-".to_owned();
    };
    let value = value.replace(" :: ", "::");
    let trimmed = value
        .strip_suffix(".to_owned()")
        .or_else(|| value.strip_suffix(".to_string()"))
        .unwrap_or(&value)
        .trim()
        .to_owned();
    match trimmed.as_str() {
        "String::new()" | "Vec::new()" | "PathBuf::new()" => "empty".to_owned(),
        _ => {
            let unquoted = trimmed
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .unwrap_or(&trimmed);
            if unquoted.is_empty() {
                "empty".to_owned()
            } else {
                format!("`{unquoted}`")
            }
        }
    }
}

/// Render the generated region, markers included.
///
/// The output is deterministic: settings are keyed by canonical name in a
/// `BTreeMap` and consumers are sorted, so a rebuild on another machine
/// produces byte-identical text and the staleness check means something.
#[must_use]
pub fn render(settings: &[Setting]) -> String {
    let mut groups: BTreeMap<&str, Vec<&Setting>> = BTreeMap::new();
    for setting in settings {
        groups.entry(setting.group()).or_default().push(setting);
    }

    let mut out = String::new();
    out.push_str(BEGIN_MARKER);
    out.push('\n');
    out.push_str(
        "<!--\n\
         DO NOT EDIT THIS REGION BY HAND. It is rendered from the COMPILED\n\
         ConfigSpec registries of the six declaring binaries by\n\
         `cargo run -p zeroship-config-contract -- env-vars-doc`, and\n\
         tests/config_name_alignment_gate.sh fails when it drifts. Everything\n\
         OUTSIDE these two markers is hand-maintained and is never rewritten\n\
         by the generator.\n\
         -->\n\n",
    );

    let mut operational = 0usize;
    let mut secret = 0usize;
    let mut bootstrap = 0usize;
    let mut command = 0usize;
    for setting in settings {
        match setting.class {
            SupplyClass::Operational => operational += 1,
            SupplyClass::Secret => secret += 1,
            SupplyClass::Bootstrap => bootstrap += 1,
            SupplyClass::Command => command += 1,
        }
    }
    let _ = writeln!(
        out,
        "**{} canonical settings**: {operational} operational, {secret} secret, \
         {bootstrap} bootstrap controls, {command} command controls. Every \
         environment name below is `ZEROSHIP_<CANONICAL>` and every overlay path \
         is the canonical name itself, because both are computed from the one \
         declaration rather than spelled twice.\n",
        settings.len(),
    );

    // `shared` first because a name with no scope prefix is one several
    // binaries read, and that is the group a reader needs before the rest.
    let mut order: Vec<&str> = groups.keys().copied().collect();
    order.sort_by_key(|group| (*group != "shared", *group));

    for group in order {
        let Some(rows) = groups.get(group) else {
            continue;
        };
        let heading = if group == "shared" {
            "shared (no scope prefix: read by more than one binary)".to_owned()
        } else {
            group.replace('_', "-")
        };
        let _ = writeln!(out, "### {heading}\n");
        out.push_str("| Canonical | Class | Environment | Overlay path | Flag by binary | Default |\n");
        out.push_str("| --- | --- | --- | --- | --- | --- |\n");
        for row in rows {
            let consumers = row
                .consumers
                .iter()
                .map(|(target, flag)| format!("{target} `{flag}`"))
                .collect::<Vec<_>>()
                .join("<br>");
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} | {} | {} |",
                row.canonical,
                class_label(row.class),
                cell(row.env.as_ref()),
                cell(row.toml.as_ref()),
                consumers,
                default_cell(row.default.as_ref()),
            );
        }
        out.push('\n');
    }

    out.push_str(END_MARKER);
    out.push('\n');
    out
}

/// Locate the generated region in `document`.
fn region(path: &str, document: &str) -> Result<(usize, usize), DocError> {
    let begin = document
        .find(BEGIN_MARKER)
        .ok_or_else(|| DocError::MissingMarker {
            path: path.to_owned(),
            marker: BEGIN_MARKER,
        })?;
    let end = document
        .find(END_MARKER)
        .ok_or_else(|| DocError::MissingMarker {
            path: path.to_owned(),
            marker: END_MARKER,
        })?;
    if end < begin {
        return Err(DocError::InvertedMarkers {
            path: path.to_owned(),
        });
    }
    Ok((begin, end + END_MARKER.len() + 1))
}

/// Replace the generated region of `document`, leaving everything else byte-identical.
///
/// # Errors
///
/// Returns [`DocError::MissingMarker`] or [`DocError::InvertedMarkers`] when the
/// region cannot be located, and [`DocError::EmptyRender`] when the contract
/// produced nothing to write.
pub fn splice(path: &str, document: &str, generated: &str) -> Result<String, DocError> {
    // Count TABLE ROWS, not lines. The region's markers, do-not-edit note,
    // summary sentence and column headers are emitted unconditionally, so a
    // line count is satisfied by a render carrying no settings at all - which
    // is the exact state that would make every later `check` pass on nothing.
    if !generated.lines().any(|line| line.starts_with("| `")) {
        return Err(DocError::EmptyRender);
    }
    let (begin, end) = region(path, document)?;
    let mut out = String::with_capacity(document.len() + generated.len());
    out.push_str(&document[..begin]);
    out.push_str(generated);
    out.push_str(document.get(end..).unwrap_or(""));
    Ok(out)
}

/// Verify that the committed region equals what the contract renders now.
///
/// # Errors
///
/// Returns [`DocError::Stale`] on any difference, plus the location and
/// anti-vacuity errors [`splice`] returns.
pub fn check(path: &str, document: &str, generated: &str) -> Result<(), DocError> {
    let spliced = splice(path, document, generated)?;
    if spliced == document {
        Ok(())
    } else {
        Err(DocError::Stale {
            path: path.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{check, collect, render, splice, DocError, BEGIN_MARKER, END_MARKER};
    use zeroship_core::config::{CanonicalName, ConfigSpec, Consumer, SupplyClass};

    const CONTROL: Consumer = Consumer::new("zeroship-control", "control");
    const GATEWAY: Consumer = Consumer::new("zeroship-gate", "gateway");

    fn shared_secret() -> ConfigSpec {
        ConfigSpec::secret(
            CanonicalName::from_static("control_key"),
            &[CONTROL, GATEWAY],
            "control_key",
            "control_key",
            "String",
        )
    }

    fn scoped_operational() -> ConfigSpec {
        ConfigSpec::operational(
            CanonicalName::from_static("control.port"),
            &[CONTROL],
            "port",
            "port",
            "u16",
            Some("9090"),
        )
    }

    #[test]
    fn a_shared_identity_merges_into_one_row_carrying_both_consumers() {
        // What this measures: that two declarations of ONE canonical name
        // produce one documented setting, not two. A reader who sees the same
        // name twice cannot tell a shared secret from a collision.
        let settings = collect(&[shared_secret(), scoped_operational()]);
        assert_eq!(settings.len(), 2, "one row per canonical identity");
        let shared = settings
            .iter()
            .find(|setting| setting.canonical == "control_key")
            .expect("shared row");
        assert_eq!(shared.consumers.len(), 2);
        assert_eq!(shared.group(), "shared");
        // The secret's only flag is the path flag, in both binaries.
        assert!(shared
            .consumers
            .iter()
            .all(|(_, flag)| flag == "--control-key-file"));

        let scoped = settings
            .iter()
            .find(|setting| setting.canonical == "control.port")
            .expect("scoped row");
        assert_eq!(scoped.group(), "control");
        assert_eq!(scoped.consumers, vec![(
            "zeroship-control".to_owned(),
            "--port".to_owned()
        )]);
    }

    #[test]
    fn rendering_is_deterministic_and_carries_every_projection() {
        let settings = collect(&[shared_secret(), scoped_operational()]);
        let first = render(&settings);
        assert_eq!(first, render(&settings), "render must be reproducible");
        assert!(first.contains("ZEROSHIP_CONTROL_KEY"));
        assert!(first.contains("ZEROSHIP_CONTROL_PORT"));
        assert!(first.contains("`control.port`"));
        assert!(first.starts_with(BEGIN_MARKER));
        assert!(first.trim_end().ends_with(END_MARKER));
        // `shared` sorts before `control`, so a reader meets the multi-binary
        // names first.
        let shared_at = first.find("### shared").expect("shared heading");
        let control_at = first.find("### control").expect("control heading");
        assert!(shared_at < control_at);
    }

    #[test]
    fn splice_touches_only_the_region_and_check_reports_a_stale_document() {
        let generated = render(&collect(&[shared_secret(), scoped_operational()]));
        let document = format!(
            "# head\n\nhand-written prose\n\n{BEGIN_MARKER}\nstale\n{END_MARKER}\n\n\
             ## hand-maintained external names\n\n`STRIPE_SECRET_KEY`\n"
        );
        // The hand-maintained halves survive byte-for-byte. This is the
        // property the whole marker scheme exists for: a generator that ate the
        // external section would destroy names no registry can re-derive.
        let spliced = splice("doc.md", &document, &generated).expect("splice");
        assert!(spliced.starts_with("# head\n\nhand-written prose\n\n"));
        assert!(spliced.ends_with("## hand-maintained external names\n\n`STRIPE_SECRET_KEY`\n"));
        assert!(!spliced.contains("stale"));

        assert_eq!(
            check("doc.md", &document, &generated),
            Err(DocError::Stale {
                path: "doc.md".to_owned()
            }),
            "a document with a stale region must FAIL, or the gate is decoration"
        );
        assert_eq!(check("doc.md", &spliced, &generated), Ok(()));
    }

    #[test]
    fn a_document_without_markers_fails_rather_than_appending() {
        let generated = render(&collect(&[scoped_operational()]));
        let error = splice("doc.md", "# no markers here\n", &generated)
            .expect_err("a document with no region must not be rewritten");
        assert!(matches!(error, DocError::MissingMarker { .. }));
    }

    #[test]
    fn an_empty_contract_refuses_to_render_a_passing_document() {
        // The vacuity case, and the reason the guard counts table rows rather
        // than lines: with zero specs the region still carries its markers, its
        // do-not-edit note, a summary sentence and no rows at all. Splicing
        // that in would make every later `check` pass while documenting
        // nothing. A line-count guard passed this and had to be replaced.
        let generated = render(&collect(&[]));
        assert!(
            generated.lines().count() > 10,
            "an empty render is not SHORT, which is why counting lines missed it"
        );
        assert_eq!(
            splice("doc.md", "x", &generated),
            Err(DocError::EmptyRender)
        );
    }

    #[test]
    fn a_default_renders_as_the_value_an_operator_supplies() {
        use super::default_cell;
        // The registry stores the DECLARATION's expression, not its value.
        assert_eq!(
            default_cell(Some(&"\"./bundles\".to_owned()".to_owned())),
            "`./bundles`"
        );
        assert_eq!(
            default_cell(Some(&"CheckFormat :: Text".to_owned())),
            "`CheckFormat::Text`"
        );
        assert_eq!(default_cell(Some(&"String :: new()".to_owned())), "empty");
        assert_eq!(default_cell(Some(&"5".to_owned())), "`5`");
        assert_eq!(default_cell(None), "-");
        // An expression outside the two known artifacts is printed as written.
        // Guessing here would put a default in the reference that no code has.
        assert_eq!(
            default_cell(Some(&"Duration::from_secs(30)".to_owned())),
            "`Duration::from_secs(30)`"
        );
    }

    #[test]
    fn a_class_with_no_env_or_overlay_tier_renders_a_dash_not_a_blank() {
        let command = ConfigSpec::command(
            CanonicalName::from_static("check_config"),
            &[CONTROL],
            "check_config",
            "check_config",
            "bool",
            None,
        );
        let settings = collect(&[command]);
        assert_eq!(settings[0].class, SupplyClass::Command);
        assert_eq!(settings[0].env, None);
        assert_eq!(settings[0].toml, None);
        let rendered = render(&settings);
        assert!(
            rendered.contains("| `check_config` | command control | - | - |"),
            "absent tiers must be visible as `-`, not an empty cell: {rendered}"
        );
    }
}
