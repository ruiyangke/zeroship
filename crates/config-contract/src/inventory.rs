//! Mechanical enumeration of every configuration name the workspace declares.
//!
//! Step 3 of `docs/proposals/2026-08-11-config-name-alignment.md` closes only
//! when "every row must be converted or explicitly classified". That sentence
//! needs a ROW SET, and until this module existed there was none: the compiled
//! registry in [`crate::contract`] can only see declarations that have already
//! been converted AND that the checker links, so using it as the worklist would
//! have reported the surface as complete the moment the last converted field
//! agreed with itself. An audit that cannot enumerate its own surface
//! under-reports silently.
//!
//! So this is a SOURCE scan, not a compiled one, and that choice is what makes
//! it usable BEFORE the conversion it measures. It reads every Rust file in the
//! tracked crate tree, finds each clap command struct and each
//! `#[zeroship_config]` declaration, and emits one row per value-bearing field.
//!
//! Three properties keep the result from being quietly empty or quietly short:
//!
//!   * an empty file set, or a file set containing no command struct at all, is
//!     an ERROR rather than a clean zero-row report;
//!   * a file that does not parse as Rust is an ERROR, not a skipped file;
//!   * a field whose `#[arg(...)]` attribute cannot be parsed still emits a row,
//!     carrying an explicit `<unparsed>` marker, so a shape this scanner does
//!     not understand shows up as a visible row rather than as an absence.
//!
//! What it does NOT do: it does not prove a declared name is READ. That is the
//! compiled registry's job ([`crate::contract::validate_contract`]), and the two
//! answer different questions. It also cannot see a flag built by a macro that
//! generates clap attributes other than `zeroship_config`; there is none today,
//! and one added later would appear as a struct with no rows.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Attribute, Expr, Fields, ItemStruct, Lit, Meta, Token, Type};
use thiserror::Error;

/// The shared-identity table, included from the proc-macro crate that owns it.
///
/// It is INCLUDED rather than copied because the whole point of that table is
/// that a shared canonical name is spelled exactly once (see its module docs).
/// A second copy here would be the very "second spelling that can be edited to
/// agree with a typo" the proposal rejects in Section 6. A proc-macro crate can
/// export only macros, so `include!` is the one mechanism that shares the data
/// without duplicating it.
#[allow(dead_code)]
#[path = "../../config-macros/src/shared.rs"]
mod shared_table;

/// What a row's supply class is, as declared today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowClass {
    /// A converted `Operational<T>` declaration.
    Operational,
    /// A converted `Secret<T>` declaration.
    Secret,
    /// A converted `BootstrapControl<T>` declaration.
    Bootstrap,
    /// A converted `CommandControl<T>` declaration.
    Command,
    /// A hand-spelled clap field that has not been converted.
    Unconverted,
}

impl RowClass {
    /// The TSV spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Operational => "operational",
            Self::Secret => "secret",
            Self::Bootstrap => "bootstrap",
            Self::Command => "command",
            Self::Unconverted => "unconverted",
        }
    }

    /// Whether this row has been moved onto the generated declaration.
    #[must_use]
    pub const fn is_converted(self) -> bool {
        !matches!(self, Self::Unconverted)
    }
}

/// How much authority the TOML column carries for one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TomlEvidence {
    /// Derived from the canonical identity by the production transform.
    Projection,
    /// Joined to a current overlay leaf by matching the final segment to the
    /// field name. A HEURISTIC: the current overlay tiers are merged by hand in
    /// each `main.rs`, so nothing in the source states the pairing.
    LeafNameMatch,
    /// No overlay tier at all.
    None,
}

impl TomlEvidence {
    /// The TSV spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Projection => "projection",
            Self::LeafNameMatch => "leaf-name-match",
            Self::None => "none",
        }
    }
}

/// One configuration name, as it exists in the source right now.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct InventoryRow {
    /// Exact binary target that consumes it, or `-` when the struct names none.
    pub consumer: String,
    /// Cargo package directory the declaration lives in.
    pub package: String,
    /// Declaring struct.
    pub struct_name: String,
    /// Rust field name.
    pub field: String,
    /// Supply class as declared today.
    pub class: RowClass,
    /// Current long-flag spelling, with leading dashes.
    pub flag: String,
    /// Current environment spelling, or `-`.
    pub env: String,
    /// Current TOML path, or `-`.
    pub toml: String,
    /// How the TOML column was determined.
    pub toml_evidence: TomlEvidence,
    /// Canonical identity, once converted; `-` while it is not.
    pub canonical: String,
    /// `file:line` of the field declaration.
    pub location: String,
}

/// Counts a caller can compare across a conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InventorySummary {
    /// Rust files scanned.
    pub files: usize,
    /// Command structs found.
    pub structs: usize,
    /// Total rows.
    pub rows: usize,
    /// Rows carrying a generated declaration.
    pub converted: usize,
    /// Rows still hand-spelled.
    pub unconverted: usize,
}

/// A failure that makes an inventory result untrustworthy.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum InventoryError {
    /// No source was handed in, so a zero-row report would be vacuous.
    #[error("configuration inventory scanned zero source files")]
    EmptyScan,
    /// Source was scanned but contained no command struct at all.
    #[error("configuration inventory found zero command structs in {0} files")]
    NoCommandStructs(usize),
    /// A file did not parse, so its declarations are invisible.
    #[error("{path}: could not parse Rust source: {message}")]
    Parse {
        /// File that failed.
        path: String,
        /// Syn diagnostic.
        message: String,
    },
    /// A `shared = SYMBOL` names an identity the table does not carry.
    #[error("{location}: unknown shared identity {symbol}")]
    UnknownShared {
        /// `file:line`.
        location: String,
        /// Symbol written in the declaration.
        symbol: String,
    },
    /// A shared identity has exactly one declaring consumer.
    ///
    /// `shared = SYMBOL` exists to stop an identity declared in SEVERAL
    /// binaries from de-sharing on a typo. With one consumer it buys nothing
    /// and costs the thing the design is most wary of: the canonical name moves
    /// out of the declaration and into the proc-macro crate's table.
    #[error("shared identity {canonical} has one consumer ({consumer}); declare it with `name`")]
    SingleConsumerShared {
        /// Canonical identity.
        canonical: String,
        /// Its only consumer.
        consumer: String,
    },
}

/// A scan result: the rows, the counts, and every non-fatal finding.
///
/// Findings do NOT suppress the rows. The proposal's use for this command is
/// "the before/after output becomes the per-PR migration checklist", and a
/// checklist that prints nothing the moment one declaration is mid-edit is
/// unusable for exactly the window it exists to serve. Fatal problems - an
/// empty file set, a file that will not parse - still abort, because those make
/// the ROW SET itself untrustworthy rather than one row wrong.
#[derive(Debug, Clone)]
pub struct InventoryReport {
    /// Every enumerated row, sorted.
    pub rows: Vec<InventoryRow>,
    /// Counts across the whole scan.
    pub summary: InventorySummary,
    /// Non-fatal problems found while enumerating.
    pub findings: Vec<InventoryError>,
}

/// Every current overlay leaf, for the heuristic TOML join.
///
/// Derived from the `FileConfig` section structs rather than restated, so a
/// section added there appears here without editing this file.
#[derive(Debug, Clone, Default)]
pub struct OverlayLeaves {
    leaves: Vec<String>,
}

impl OverlayLeaves {
    /// Parse `crates/core/src/config/file.rs` into dotted leaf paths.
    ///
    /// # Errors
    ///
    /// Returns a parse error when the source is not valid Rust.
    pub fn from_source(path: &str, source: &str) -> Result<Self, InventoryError> {
        let file = syn::parse_file(source).map_err(|error| InventoryError::Parse {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        let mut sections: Vec<(String, Vec<String>)> = Vec::new();
        let mut root: Vec<(String, Option<String>)> = Vec::new();
        for item in &file.items {
            let syn::Item::Struct(item_struct) = item else {
                continue;
            };
            let Fields::Named(named) = &item_struct.fields else {
                continue;
            };
            let struct_name = item_struct.ident.to_string();
            let fields = named
                .named
                .iter()
                .filter_map(|field| field.ident.as_ref().map(ToString::to_string))
                .collect::<Vec<_>>();
            if struct_name == "FileConfig" {
                root = named
                    .named
                    .iter()
                    .filter_map(|field| {
                        let ident = field.ident.as_ref()?.to_string();
                        Some((ident, section_type(&field.ty)))
                    })
                    .collect();
            } else {
                sections.push((struct_name, fields));
            }
        }

        let mut leaves = Vec::new();
        for (field, section) in root {
            match section.and_then(|name| {
                sections
                    .iter()
                    .find(|(struct_name, _)| *struct_name == name)
                    .cloned()
            }) {
                Some((_, section_fields)) => {
                    for leaf in section_fields {
                        leaves.push(format!("{field}.{leaf}"));
                    }
                }
                None => leaves.push(field),
            }
        }
        leaves.sort();
        leaves.dedup();
        Ok(Self { leaves })
    }

    /// Every discovered leaf path.
    #[must_use]
    pub fn paths(&self) -> &[String] {
        &self.leaves
    }

    /// The unique leaf whose final segment equals `field`, if exactly one does.
    #[must_use]
    pub fn join_by_field(&self, field: &str) -> Option<&str> {
        let mut matches = self
            .leaves
            .iter()
            .filter(|leaf| leaf.rsplit('.').next() == Some(field));
        let first = matches.next()?;
        matches.next().is_none().then_some(first.as_str())
    }
}

/// A `Section`-typed field on `FileConfig`, unwrapping nothing else.
fn section_type(ty: &Type) -> Option<String> {
    let Type::Path(path) = ty else {
        return None;
    };
    let last = path.path.segments.last()?;
    let name = last.ident.to_string();
    name.ends_with("Section").then_some(name)
}

/// Scan a set of `(path, source)` pairs into inventory rows.
///
/// # Errors
///
/// Returns [`InventoryError::EmptyScan`] for an empty set,
/// [`InventoryError::NoCommandStructs`] when nothing declares configuration,
/// and a parse error per unparsable file.
pub fn scan_sources(
    sources: &[(String, String)],
    overlay: &OverlayLeaves,
) -> Result<InventoryReport, Vec<InventoryError>> {
    if sources.is_empty() {
        return Err(vec![InventoryError::EmptyScan]);
    }
    let mut rows = Vec::new();
    let mut fatal = Vec::new();
    let mut findings = Vec::new();
    let mut structs = 0usize;
    let flattened = flattened_owners(sources);

    for (path, source) in sources {
        let file = match syn::parse_file(source) {
            Ok(file) => file,
            Err(error) => {
                fatal.push(InventoryError::Parse {
                    path: path.clone(),
                    message: error.to_string(),
                });
                continue;
            }
        };
        let package = package_of(path);
        for item in &file.items {
            let syn::Item::Struct(item_struct) = item else {
                continue;
            };
            let Some(kind) = struct_kind(item_struct) else {
                continue;
            };
            structs += 1;
            match kind {
                StructKind::Generated { binary } => {
                    generated_rows(item_struct, path, &package, &binary, &mut rows, &mut findings);
                }
                StructKind::Clap { consumer } => {
                    // A `#[derive(Args)]` container carries no `#[command(name)]`
                    // of its own, so without this its rows would be attributed to
                    // "-" and silently drop out of every per-binary count.
                    let consumer = if consumer == UNKNOWN_CONSUMER {
                        flattened
                            .get(&item_struct.ident.to_string())
                            .cloned()
                            .unwrap_or(consumer)
                    } else {
                        consumer
                    };
                    clap_rows(item_struct, path, &package, &consumer, overlay, &mut rows);
                }
            }
        }
    }

    if !fatal.is_empty() {
        return Err(fatal);
    }
    if structs == 0 {
        return Err(vec![InventoryError::NoCommandStructs(sources.len())]);
    }
    rows.sort();
    findings.extend(single_consumer_shared(&rows));
    findings.sort_by_key(ToString::to_string);
    findings.dedup();
    let converted = rows.iter().filter(|row| row.class.is_converted()).count();
    let summary = InventorySummary {
        files: sources.len(),
        structs,
        rows: rows.len(),
        converted,
        unconverted: rows.len() - converted,
    };
    Ok(InventoryReport {
        rows,
        summary,
        findings,
    })
}

/// Report every shared-table identity that exactly one binary declares.
///
/// This is the check the shared table cannot make about itself: a proc macro
/// sees one declaration at a time and has no idea how many binaries name the
/// same symbol. The whole-tree scan does.
fn single_consumer_shared(rows: &[InventoryRow]) -> Vec<InventoryError> {
    let mut consumers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for row in rows.iter().filter(|row| row.class.is_converted()) {
        if shared_table::by_canonical(&row.canonical).is_some() {
            consumers
                .entry(row.canonical.as_str())
                .or_default()
                .insert(row.consumer.as_str());
        }
    }
    consumers
        .into_iter()
        .filter_map(|(canonical, targets)| {
            let only = targets.iter().copied().next()?;
            // Fixture declarations are not real consumers and must not make a
            // genuinely shared identity look shared, nor a single-consumer one
            // look multi-consumer.
            (targets.len() == 1 && !only.starts_with("zeroship-fixture-")).then(|| {
                InventoryError::SingleConsumerShared {
                    canonical: canonical.to_owned(),
                    consumer: (*only).to_owned(),
                }
            })
        })
        .collect()
}

enum StructKind {
    /// Carries `#[zeroship_config(binary = "...")]`.
    Generated { binary: String },
    /// Derives clap's `Parser` or `Args`.
    Clap { consumer: String },
}

/// The consumer written when nothing names the binary.
const UNKNOWN_CONSUMER: &str = "-";

/// Map each `#[command(flatten)]`-ed struct TYPE to the binary that flattens it.
///
/// A `#[derive(Args)]` container is a normal clap idiom for grouping arguments,
/// and it never carries `#[command(name = "...")]` - the parent's name is the
/// command. Attributing its fields by type is what keeps them inside their
/// binary's row count instead of landing in an unattributed "-" bucket.
///
/// Ambiguity fails SAFE rather than silently picking one: a type flattened by
/// two different binaries is left unattributed, because guessing would put real
/// rows under a binary that may not declare them.
fn flattened_owners(sources: &[(String, String)]) -> BTreeMap<String, String> {
    let mut owners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (_, source) in sources {
        let Ok(file) = syn::parse_file(source) else {
            continue;
        };
        for item in &file.items {
            let syn::Item::Struct(item_struct) = item else {
                continue;
            };
            let Some(StructKind::Clap { consumer }) = struct_kind(item_struct) else {
                continue;
            };
            if consumer == UNKNOWN_CONSUMER {
                continue;
            }
            let Fields::Named(named) = &item_struct.fields else {
                continue;
            };
            for field in &named.named {
                if !field.attrs.iter().any(is_container_attr) {
                    continue;
                }
                if let Type::Path(path) = &field.ty
                    && let Some(segment) = path.path.segments.last()
                {
                    owners
                        .entry(segment.ident.to_string())
                        .or_default()
                        .insert(consumer.clone());
                }
            }
        }
    }
    owners
        .into_iter()
        .filter_map(|(ty, consumers)| {
            (consumers.len() == 1).then(|| {
                let only = consumers.into_iter().next().expect("one consumer");
                (ty, only)
            })
        })
        .collect()
}

fn struct_kind(item: &ItemStruct) -> Option<StructKind> {
    if let Some(attr) = item
        .attrs
        .iter()
        .find(|attr| attr.path().is_ident("zeroship_config"))
    {
        let binary = string_arg(attr, "binary").unwrap_or_else(|| "-".to_owned());
        return Some(StructKind::Generated { binary });
    }
    let derives_command = item.attrs.iter().any(|attr| {
        attr.path().is_ident("derive")
            && attr
                .parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated)
                .is_ok_and(|paths| {
                    paths.iter().any(|path| {
                        path.segments
                            .last()
                            .is_some_and(|segment| segment.ident == "Parser" || segment.ident == "Args")
                    })
                })
    });
    if !derives_command {
        return None;
    }
    let consumer = item
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("command"))
        .find_map(|attr| string_arg(attr, "name"))
        .unwrap_or_else(|| "-".to_owned());
    Some(StructKind::Clap { consumer })
}

/// Read `key = "value"` out of an attribute's argument list.
fn string_arg(attr: &Attribute, key: &str) -> Option<String> {
    let metas = attr
        .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        .ok()?;
    metas.iter().find_map(|meta| {
        let Meta::NameValue(name_value) = meta else {
            return None;
        };
        if !name_value.path.is_ident(key) {
            return None;
        }
        literal_string(&name_value.value)
    })
}

fn literal_string(expr: &Expr) -> Option<String> {
    let Expr::Lit(lit) = expr else {
        return None;
    };
    let Lit::Str(value) = &lit.lit else {
        return None;
    };
    Some(value.value())
}

fn package_of(path: &str) -> String {
    let mut parts = path.split('/');
    while let Some(part) = parts.next() {
        if part == "crates" || part == "libs" {
            return parts.next().unwrap_or("-").to_owned();
        }
    }
    "-".to_owned()
}

fn location(path: &str, span: proc_macro2::Span) -> String {
    format!("{path}:{}", span.start().line)
}

fn generated_rows(
    item: &ItemStruct,
    path: &str,
    package: &str,
    binary: &str,
    rows: &mut Vec<InventoryRow>,
    findings: &mut Vec<InventoryError>,
) {
    let Fields::Named(named) = &item.fields else {
        return;
    };
    let struct_name = item.ident.to_string();
    for field in &named.named {
        let Some(ident) = field.ident.as_ref() else {
            continue;
        };
        let Some(attr) = field.attrs.iter().find(|attr| attr.path().is_ident("config")) else {
            continue;
        };
        let where_ = location(path, field.span());
        let class = wrapper_class(&field.ty);
        let (canonical, class) = match config_identity(attr) {
            ConfigIdentity::Name(name) => (name, class),
            ConfigIdentity::Shared(symbol) => match shared_table::by_symbol(&symbol) {
                Some(identity) => (identity.canonical.to_owned(), class),
                None => {
                    // A FINDING, not an abort: the row is still real and still
                    // belongs in the checklist, it just cannot be projected.
                    findings.push(InventoryError::UnknownShared {
                        location: where_.clone(),
                        symbol: symbol.clone(),
                    });
                    (format!("<unknown-shared:{symbol}>"), RowClass::Unconverted)
                }
            },
            ConfigIdentity::Missing => ("-".to_owned(), RowClass::Unconverted),
        };
        let env_disabled = config_env_disabled(attr);
        let scope = string_arg(
            item.attrs
                .iter()
                .find(|attr| attr.path().is_ident("zeroship_config"))
                .expect("generated struct carries its attribute"),
            "scope",
        )
        .unwrap_or_else(|| binary.to_owned());

        let has_env = matches!(class, RowClass::Operational | RowClass::Secret)
            || (class == RowClass::Bootstrap && !env_disabled);
        let has_toml = matches!(class, RowClass::Operational | RowClass::Secret);
        rows.push(InventoryRow {
            consumer: binary.to_owned(),
            package: package.to_owned(),
            struct_name: struct_name.clone(),
            field: ident.to_string(),
            class,
            flag: format!("--{}", project_flag(&canonical, &scope, class)),
            env: if has_env {
                project_env(&canonical)
            } else {
                "-".to_owned()
            },
            toml: if has_toml {
                canonical.clone()
            } else {
                "-".to_owned()
            },
            toml_evidence: if has_toml {
                TomlEvidence::Projection
            } else {
                TomlEvidence::None
            },
            canonical,
            location: where_,
        });
    }
}

enum ConfigIdentity {
    Name(String),
    Shared(String),
    Missing,
}

fn config_identity(attr: &Attribute) -> ConfigIdentity {
    let Ok(metas) = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated) else {
        return ConfigIdentity::Missing;
    };
    for meta in &metas {
        let Meta::NameValue(name_value) = meta else {
            continue;
        };
        if name_value.path.is_ident("name")
            && let Some(value) = literal_string(&name_value.value)
        {
            return ConfigIdentity::Name(value);
        }
        if name_value.path.is_ident("shared")
            && let Expr::Path(path) = &name_value.value
            && let Some(ident) = path.path.get_ident()
        {
            return ConfigIdentity::Shared(ident.to_string());
        }
    }
    ConfigIdentity::Missing
}

fn config_env_disabled(attr: &Attribute) -> bool {
    let Ok(metas) = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated) else {
        return false;
    };
    metas.iter().any(|meta| {
        let Meta::NameValue(name_value) = meta else {
            return false;
        };
        name_value.path.is_ident("env")
            && matches!(&name_value.value, Expr::Lit(lit) if matches!(&lit.lit, Lit::Bool(b) if !b.value()))
    })
}

fn wrapper_class(ty: &Type) -> RowClass {
    let Type::Path(path) = ty else {
        return RowClass::Unconverted;
    };
    let Some(segment) = path.path.segments.last() else {
        return RowClass::Unconverted;
    };
    match segment.ident.to_string().as_str() {
        "Operational" => RowClass::Operational,
        "Secret" => RowClass::Secret,
        "BootstrapControl" => RowClass::Bootstrap,
        "CommandControl" => RowClass::Command,
        _ => RowClass::Unconverted,
    }
}

/// The flag projection, mirroring `CanonicalName::flag_name`.
fn project_flag(canonical: &str, scope: &str, class: RowClass) -> String {
    let local = canonical
        .strip_prefix(scope)
        .and_then(|rest| rest.strip_prefix('.'))
        .unwrap_or(canonical);
    let mut flag = local.replace(['.', '_'], "-");
    if class == RowClass::Secret {
        flag.push_str("-file");
    }
    flag
}

/// The environment projection, mirroring `CanonicalName::env_name`.
fn project_env(canonical: &str) -> String {
    format!("ZEROSHIP_{}", canonical.replace('.', "_").to_ascii_uppercase())
}

fn clap_rows(
    item: &ItemStruct,
    path: &str,
    package: &str,
    consumer: &str,
    overlay: &OverlayLeaves,
    rows: &mut Vec<InventoryRow>,
) {
    let Fields::Named(named) = &item.fields else {
        return;
    };
    let struct_name = item.ident.to_string();
    for field in &named.named {
        let Some(ident) = field.ident.as_ref() else {
            continue;
        };
        // A flattened or subcommand field is a container, not a name. The
        // struct it names is enumerated in its own right, so counting it here
        // would double-count the whole nested set as one row.
        if field.attrs.iter().any(is_container_attr) {
            continue;
        }
        let field_name = ident.to_string();
        let arg = field
            .attrs
            .iter()
            .find(|attr| attr.path().is_ident("arg") || attr.path().is_ident("clap"));
        let (flag, env) = match arg {
            None => ("<positional>".to_owned(), "-".to_owned()),
            Some(attr) => match arg_projections(attr, &field_name) {
                Some(pair) => pair,
                // A shape this scanner does not understand becomes a VISIBLE
                // row, never a dropped one.
                None => ("<unparsed>".to_owned(), "<unparsed>".to_owned()),
            },
        };
        let (toml, evidence) = overlay.join_by_field(&field_name).map_or_else(
            || ("-".to_owned(), TomlEvidence::None),
            |leaf| (leaf.to_owned(), TomlEvidence::LeafNameMatch),
        );
        rows.push(InventoryRow {
            consumer: consumer.to_owned(),
            package: package.to_owned(),
            struct_name: struct_name.clone(),
            field: field_name,
            class: RowClass::Unconverted,
            flag,
            env,
            toml,
            toml_evidence: evidence,
            canonical: "-".to_owned(),
            location: location(path, field.span()),
        });
    }
}

fn is_container_attr(attr: &Attribute) -> bool {
    if !(attr.path().is_ident("command") || attr.path().is_ident("clap")) {
        return false;
    }
    attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        .is_ok_and(|metas| {
            metas.iter().any(|meta| {
                matches!(meta, Meta::Path(path) if path.is_ident("flatten") || path.is_ident("subcommand"))
            })
        })
}

/// Return `(--flag, ENV)` for one `#[arg(...)]`, or `None` when it will not parse.
fn arg_projections(attr: &Attribute, field: &str) -> Option<(String, String)> {
    let metas = attr
        .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        .ok()?;
    let mut long = None;
    let mut env = None;
    let mut has_bare_long = false;
    for meta in &metas {
        match meta {
            Meta::Path(path) if path.is_ident("long") => has_bare_long = true,
            Meta::NameValue(name_value) if name_value.path.is_ident("long") => {
                long = literal_string(&name_value.value);
            }
            Meta::NameValue(name_value) if name_value.path.is_ident("env") => {
                env = literal_string(&name_value.value);
            }
            _ => {}
        }
    }
    let flag = match long {
        Some(explicit) => format!("--{explicit}"),
        None if has_bare_long => format!("--{}", field.replace('_', "-")),
        None => "<positional>".to_owned(),
    };
    Some((flag, env.unwrap_or_else(|| "-".to_owned())))
}

/// Column header, tab separated, matching [`format_tsv`].
pub const TSV_HEADER: &str = "consumer\tpackage\tstruct\tfield\tclass\tflag\tenv\ttoml\t\
toml_evidence\tcanonical\tlocation";

/// Render rows as a header plus one tab-separated line each.
#[must_use]
pub fn format_tsv(rows: &[InventoryRow]) -> String {
    let mut out = String::from(TSV_HEADER);
    out.push('\n');
    for row in rows {
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            row.consumer,
            row.package,
            row.struct_name,
            row.field,
            row.class.as_str(),
            row.flag,
            row.env,
            row.toml,
            row.toml_evidence.as_str(),
            row.canonical,
            row.location
        );
    }
    out
}

/// Collect every `.rs` file under `root` that is not in an ignored directory.
///
/// # Errors
///
/// Returns an I/O diagnostic as a parse-shaped error when a directory cannot be
/// walked, because a silently skipped directory is a silently short inventory.
pub fn collect_rust_sources(root: &Path, subdirs: &[&str]) -> Result<Vec<(String, String)>, Vec<InventoryError>> {
    const IGNORED: &[&str] = &["target", "node_modules", ".git", "third_party", "dist"];
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    let mut stack = subdirs.iter().map(|dir| root.join(dir)).collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                errors.push(InventoryError::Parse {
                    path: dir.display().to_string(),
                    message: error.to_string(),
                });
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if !IGNORED.contains(&name.as_str()) {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                match std::fs::read_to_string(&path) {
                    Ok(source) => {
                        let relative = path
                            .strip_prefix(root)
                            .unwrap_or(&path)
                            .display()
                            .to_string();
                        if seen.insert(relative.clone()) {
                            sources.push((relative, source));
                        }
                    }
                    Err(error) => errors.push(InventoryError::Parse {
                        path: path.display().to_string(),
                        message: error.to_string(),
                    }),
                }
            }
        }
    }
    if errors.is_empty() {
        sources.sort();
        Ok(sources)
    } else {
        Err(errors)
    }
}

/// Every TRACKED first-party Rust file, as `(repository-relative path, source)`.
///
/// `git ls-files` rather than a directory walk, following the repository's
/// existing source-gate pattern in `crates/core/tests/source_is_greppable_test.rs`.
/// The difference matters twice: a build artefact under `target/` is not source
/// and must not be scanned, and a file someone forgot to `git add` is not yet
/// part of the repository, so a gate that walked the filesystem would fail on
/// scratch files while missing nothing real.
///
/// `third_party/` is excluded because it is a vendored submodule with its own
/// workspace and its own rules; it is not ours to rewrite.
///
/// # Errors
///
/// Returns an error when `git ls-files` cannot run, when it lists nothing, or
/// when a listed file cannot be read.
pub fn collect_tracked_rust_sources(root: &Path) -> Result<Vec<(String, String)>, Vec<InventoryError>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--", "*.rs"])
        .output()
        .map_err(|error| {
            vec![InventoryError::Parse {
                path: root.display().to_string(),
                message: format!("git ls-files failed: {error}"),
            }]
        })?;
    if !output.status.success() {
        return Err(vec![InventoryError::Parse {
            path: root.display().to_string(),
            message: format!("git ls-files exited {}", output.status),
        }]);
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    for relative in listing.split('\0').filter(|entry| !entry.is_empty()) {
        if relative.starts_with("third_party/") {
            continue;
        }
        match std::fs::read_to_string(root.join(relative)) {
            Ok(source) => sources.push((relative.to_owned(), source)),
            Err(error) => errors.push(InventoryError::Parse {
                path: relative.to_owned(),
                message: error.to_string(),
            }),
        }
    }
    if sources.is_empty() {
        errors.push(InventoryError::Parse {
            path: root.display().to_string(),
            message: "git ls-files listed no tracked Rust source".to_owned(),
        });
    }
    if errors.is_empty() {
        sources.sort();
        Ok(sources)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        collect_rust_sources, format_tsv, scan_sources, InventoryError, OverlayLeaves, RowClass,
        TomlEvidence,
    };
    use std::path::Path;

    fn no_overlay() -> OverlayLeaves {
        OverlayLeaves::default()
    }

    fn source(name: &str, body: &str) -> Vec<(String, String)> {
        vec![(name.to_owned(), body.to_owned())]
    }

    #[test]
    fn an_empty_file_set_is_an_error_not_a_clean_report() {
        // The failure this exists for: a glob that stops matching produces the
        // same zero rows as a workspace with no configuration at all.
        assert_eq!(
            scan_sources(&[], &no_overlay()).expect_err("empty scan"),
            vec![InventoryError::EmptyScan]
        );
    }

    #[test]
    fn source_with_no_command_struct_is_also_an_error() {
        let sources = source("crates/x/src/lib.rs", "pub struct NotAConfig { pub a: u8 }\n");
        assert_eq!(
            scan_sources(&sources, &no_overlay()).expect_err("no structs"),
            vec![InventoryError::NoCommandStructs(1)]
        );
    }

    #[test]
    fn an_unparsable_file_fails_rather_than_being_skipped() {
        let sources = source("crates/x/src/lib.rs", "fn ( this is not rust");
        let errors = scan_sources(&sources, &no_overlay()).expect_err("parse error");
        assert!(matches!(errors[0], InventoryError::Parse { .. }), "{errors:?}");
    }

    #[test]
    fn a_clap_struct_yields_one_row_per_value_field_with_its_line() {
        let body = r#"
#[derive(Parser)]
#[command(name = "zeroship-demo")]
struct DemoCli {
    #[arg(long, env = "DEMO_PORT", default_value_t = 1)]
    port: u16,

    #[arg(long = "blob-store", env = "BLOB_STORE")]
    blob_store: String,

    #[command(flatten)]
    controls: DemoControlsSources,
}
"#;
        let report =
            scan_sources(&source("crates/demo/src/main.rs", body), &no_overlay()).expect("scan");
        let (rows, summary) = (&report.rows, report.summary);
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert_eq!(summary.rows, 2, "the flattened container must not be a row");
        assert_eq!(summary.unconverted, 2);

        let port = rows.iter().find(|row| row.field == "port").expect("port row");
        assert_eq!(port.consumer, "zeroship-demo");
        assert_eq!(port.package, "demo");
        assert_eq!(port.flag, "--port", "a bare `long` derives the flag");
        assert_eq!(port.env, "DEMO_PORT");
        assert_eq!(port.class, RowClass::Unconverted);
        assert_eq!(port.canonical, "-");
        assert_eq!(port.location, "crates/demo/src/main.rs:5");

        let blob = rows.iter().find(|row| row.field == "blob_store").expect("blob row");
        assert_eq!(blob.flag, "--blob-store");
        assert_eq!(blob.env, "BLOB_STORE");
    }

    #[test]
    fn a_flattened_args_container_is_attributed_to_the_binary_that_flattens_it() {
        // The failure this guards: a `#[derive(Args)]` group carries no
        // `#[command(name)]`, so its rows landed under the "-" consumer and
        // vanished from every per-binary count. An audit whose worklist is
        // "rows for binary X" then reports X complete while X's arguments are
        // still hand-spelled somewhere else in the file.
        let body = r#"
#[derive(Clone, Args)]
struct DemoSecrets {
    #[arg(long, env = "DEMO_DB_URL")]
    db_url: String,
}

#[derive(Parser)]
#[command(name = "zeroship-demo")]
struct DemoCli {
    #[command(flatten)]
    secrets: DemoSecrets,
}
"#;
        let report =
            scan_sources(&source("crates/demo/src/main.rs", body), &no_overlay()).expect("scan");
        let db = report
            .rows
            .iter()
            .find(|row| row.field == "db_url")
            .expect("db_url row");
        assert_eq!(db.consumer, "zeroship-demo");

        // The one-variable control: the SAME Args struct with nothing flattening
        // it stays unattributed. Without this, the assertion above would also
        // pass on an implementation that stamped every Args struct with the last
        // binary name it happened to see.
        let orphan = r#"
#[derive(Clone, Args)]
struct OrphanSecrets {
    #[arg(long, env = "ORPHAN_DB_URL")]
    db_url: String,
}

#[derive(Parser)]
#[command(name = "zeroship-demo")]
struct DemoCli {
    #[arg(long, env = "DEMO_PORT")]
    port: u16,
}
"#;
        let report =
            scan_sources(&source("crates/demo/src/main.rs", orphan), &no_overlay()).expect("scan");
        let db = report
            .rows
            .iter()
            .find(|row| row.field == "db_url")
            .expect("db_url row");
        assert_eq!(db.consumer, "-");

        // Does NOT cover a type flattened by two binaries under the same name in
        // different crates; that case is deliberately left unattributed by
        // `flattened_owners` rather than resolved, and nothing in the tree hits
        // it today.
    }

    #[test]
    fn a_generated_struct_yields_the_canonical_projections() {
        let body = r#"
#[zeroship_config(binary = "zeroship-demo", scope = "demo")]
struct DemoControls {
    #[config(name = "demo.poll_interval", default = 5)]
    pub poll_interval: Operational<u64>,

    #[config(name = "demo.database_url")]
    pub database_url: Secret<String>,

    #[config(shared = CHECK_CONFIG)]
    pub check_config: CommandControl<bool>,

    #[config(name = "demo.allow_unsigned", env = false)]
    pub allow_unsigned: BootstrapControl<bool>,
}
"#;
        let report =
            scan_sources(&source("crates/demo/src/config.rs", body), &no_overlay()).expect("scan");
        let (rows, summary) = (&report.rows, report.summary);
        assert_eq!(summary.converted, 4);
        assert_eq!(summary.unconverted, 0);

        let poll = rows.iter().find(|row| row.field == "poll_interval").expect("row");
        assert_eq!(poll.class, RowClass::Operational);
        assert_eq!(poll.flag, "--poll-interval", "the scope segment is stripped");
        assert_eq!(poll.env, "ZEROSHIP_DEMO_POLL_INTERVAL");
        assert_eq!(poll.toml, "demo.poll_interval");
        assert_eq!(poll.toml_evidence, TomlEvidence::Projection);

        let secret = rows.iter().find(|row| row.field == "database_url").expect("row");
        assert_eq!(secret.class, RowClass::Secret);
        assert_eq!(secret.flag, "--database-url-file");
        assert_eq!(secret.env, "ZEROSHIP_DEMO_DATABASE_URL");

        // The shared symbol resolves through the macro crate's own table, not a
        // copy: a symbol renamed there changes this row without editing here.
        let check = rows.iter().find(|row| row.field == "check_config").expect("row");
        assert_eq!(check.canonical, "check_config");
        assert_eq!(check.flag, "--check-config");
        assert_eq!(check.env, "-", "a command control has no environment tier");
        assert_eq!(check.toml, "-");

        let unsigned = rows.iter().find(|row| row.field == "allow_unsigned").expect("row");
        assert_eq!(unsigned.env, "-", "`env = false` removes the environment tier");
    }

    #[test]
    fn an_unknown_shared_symbol_is_reported_with_its_location() {
        let body = r#"
#[zeroship_config(binary = "zeroship-demo", scope = "demo")]
struct DemoControls {
    #[config(shared = OBSERVABILITY_LOG_FILTR)]
    pub log_filter: Operational<String>,
}
"#;
        let report = scan_sources(&source("crates/demo/src/config.rs", body), &no_overlay())
            .expect("an unknown symbol is a finding, not a fatal error");
        assert_eq!(
            report.findings,
            vec![InventoryError::UnknownShared {
                location: "crates/demo/src/config.rs:4".to_owned(),
                symbol: "OBSERVABILITY_LOG_FILTR".to_owned(),
            }]
        );
        // The row survives: a checklist that drops the line it cannot project
        // is a checklist that under-reports exactly when it matters most.
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].canonical, "<unknown-shared:OBSERVABILITY_LOG_FILTR>");
    }

    #[test]
    fn a_shared_identity_with_one_consumer_is_reported() {
        // `shared = SYMBOL` earns its cost only when SEVERAL binaries declare
        // the identity; with one it just moves the canonical name out of the
        // declaration and into the proc-macro crate. No single declaration can
        // see this - only the whole-tree scan can.
        let one = r"
#[zeroship_config(binary = 'zeroship-alpha', scope = 'alpha')]
struct AlphaSettings {
    #[config(shared = BLOB_STORE, default = String::new())]
    pub blob_store: Operational<String>,
}
"
        .replace('\'', "\"");
        let report = scan_sources(&source("crates/alpha/src/config.rs", &one), &no_overlay())
            .expect("scan");
        assert_eq!(
            report.findings,
            vec![InventoryError::SingleConsumerShared {
                canonical: "blob_store".to_owned(),
                consumer: "zeroship-alpha".to_owned(),
            }]
        );

        // The one-variable control: the SAME declaration plus a second
        // consumer. Only this separates "the check fires on shared symbols"
        // from "the check fires on everything".
        let two = format!(
            "{one}\n{}",
            one.replace("zeroship-alpha", "zeroship-beta")
                .replace("alpha", "beta")
                .replace("AlphaSettings", "BetaSettings")
        );
        let shared = scan_sources(&source("crates/alpha/src/config.rs", &two), &no_overlay())
            .expect("scan");
        assert!(shared.findings.is_empty(), "{:?}", shared.findings);
    }

    #[test]
    fn overlay_leaves_come_from_the_file_config_sections() {
        let body = r#"
pub struct FileConfig {
    pub origin_scheme: Option<OriginScheme>,
    pub control_key: Option<String>,
    pub control: ControlSection,
}
pub struct ControlSection {
    pub port: Option<u16>,
    pub database_url: Option<String>,
}
"#;
        let leaves = OverlayLeaves::from_source("file.rs", body).expect("parse");
        assert_eq!(
            leaves.paths(),
            ["control.database_url", "control.port", "control_key", "origin_scheme"]
        );
        assert_eq!(leaves.join_by_field("database_url"), Some("control.database_url"));
        assert_eq!(leaves.join_by_field("port"), None);
    }

    #[test]
    fn the_overlay_join_is_reported_as_a_heuristic() {
        let leaves = OverlayLeaves::from_source(
            "file.rs",
            "pub struct FileConfig { pub control: ControlSection }\n\
             pub struct ControlSection { pub control_key: Option<String> }\n",
        )
        .expect("parse");
        let body = r#"
#[derive(Parser)]
#[command(name = "zeroship-demo")]
struct DemoCli {
    #[arg(long = "control-key", env = "CONTROL_KEY")]
    control_key: String,
}
"#;
        let report = scan_sources(&source("crates/demo/src/main.rs", body), &leaves).expect("scan");
        let rows = &report.rows;
        assert_eq!(rows[0].toml, "control.control_key");
        assert_eq!(
            rows[0].toml_evidence,
            TomlEvidence::LeafNameMatch,
            "the current overlay tiers are merged by hand, so the join is a name match"
        );
    }

    #[test]
    fn the_tsv_header_has_one_column_per_emitted_field() {
        let body = r#"
#[derive(Parser)]
#[command(name = "zeroship-demo")]
struct DemoCli {
    #[arg(long)]
    port: u16,
}
"#;
        let report = scan_sources(&source("crates/demo/src/main.rs", body), &no_overlay()).expect("scan");
        let rendered = format_tsv(&report.rows);
        let mut lines = rendered.lines();
        let header = lines.next().expect("header");
        let row = lines.next().expect("row");
        assert_eq!(
            header.split('\t').count(),
            row.split('\t').count(),
            "header and row width must agree or every column is off by one"
        );
    }

    #[test]
    fn the_workspace_walk_finds_this_very_file() {
        // The one-variable control for the walk: the same call restricted to a
        // directory that exists but holds no Rust source must come back empty,
        // which separates "the walk works" from "the walk matches everything".
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root");
        let sources = collect_rust_sources(root, &["crates"]).expect("walk");
        assert!(
            sources
                .iter()
                .any(|(path, _)| path.ends_with("config-contract/src/inventory.rs")),
            "the walk must reach this file"
        );
        let empty = collect_rust_sources(root, &["docs"]).expect("walk");
        assert!(empty.is_empty(), "docs holds no Rust source: {empty:?}");
    }
}
