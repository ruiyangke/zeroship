//! Cfg-independent source fallback for undeclared Rust environment reads.
//!
//! Step 1 exercises this scanner on fixtures only. The workspace-wide compiler
//! lint and tracked-file gate remain later migration work because current live
//! readers have not been converted yet.

use std::collections::BTreeSet;

use syn::visit::{self, Visit};
use syn::{ExprPath, ItemUse, Macro, UseTree};
use thiserror::Error;

const RAW_METHODS: &[&str] = &["var", "var_os", "vars", "vars_os"];

/// One cfg-independent raw environment access found in Rust source.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RawEnvViolation {
    /// The fixture was not valid Rust, so a clean scan would be meaningless.
    #[error("could not parse Rust source: {0}")]
    Parse(String),
    /// A forbidden method was imported or re-exported, possibly under an alias.
    #[error("undeclared raw environment import: {0}")]
    Import(String),
    /// A forbidden method or known FFI escape was referenced.
    #[error("undeclared raw environment read: {0}")]
    Read(String),
    /// A zeroship-owned name was captured at compile time.
    #[error("undeclared compile-time environment read: {0}")]
    CompileTime(String),
}

#[derive(Default)]
struct Imports {
    function_aliases: BTreeSet<String>,
    module_aliases: BTreeSet<String>,
    violations: Vec<RawEnvViolation>,
}

impl<'ast> Visit<'ast> for Imports {
    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        collect_use_tree(&item.tree, &mut Vec::new(), self);
        visit::visit_item_use(self, item);
    }
}

fn collect_use_tree(tree: &UseTree, prefix: &mut Vec<String>, imports: &mut Imports) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_use_tree(&path.tree, prefix, imports);
            prefix.pop();
        }
        UseTree::Name(name) => {
            let original = name.ident.to_string();
            inspect_import(prefix, &original, &original, imports);
        }
        UseTree::Rename(rename) => inspect_import(
            prefix,
            &rename.ident.to_string(),
            &rename.rename.to_string(),
            imports,
        ),
        UseTree::Glob(_) => {
            if prefix.as_slice() == ["std", "env"] {
                imports
                    .violations
                    .push(RawEnvViolation::Import("std::env::*".to_owned()));
            }
        }
        UseTree::Group(group) => {
            for item in &group.items {
                collect_use_tree(item, prefix, imports);
            }
        }
    }
}

fn inspect_import(prefix: &[String], original: &str, local: &str, imports: &mut Imports) {
    if prefix == ["std"] && original == "env" {
        imports.module_aliases.insert(local.to_owned());
        return;
    }
    if prefix == ["std", "env"] && original == "self" {
        imports.module_aliases.insert(local.to_owned());
        return;
    }
    if prefix == ["std", "env"] && RAW_METHODS.contains(&original) {
        imports.function_aliases.insert(local.to_owned());
        imports.violations.push(RawEnvViolation::Import(format!(
            "std::env::{original} as {local}"
        )));
    }
}

struct Reads<'a> {
    imports: &'a Imports,
    violations: Vec<RawEnvViolation>,
}

impl<'ast> Visit<'ast> for Reads<'_> {
    fn visit_expr_path(&mut self, path: &'ast ExprPath) {
        let segments = path
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        if is_raw_path(&segments, self.imports) {
            self.violations
                .push(RawEnvViolation::Read(segments.join("::")));
        }
        visit::visit_expr_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast Macro) {
        let name = mac.path.segments.last().map(|segment| segment.ident.to_string());
        if matches!(name.as_deref(), Some("env" | "option_env"))
            && mac.tokens.to_string().contains("ZEROSHIP_")
        {
            self.violations.push(RawEnvViolation::CompileTime(
                name.unwrap_or_else(|| "environment macro".to_owned()),
            ));
        }
        visit::visit_macro(self, mac);
    }
}

fn is_raw_path(segments: &[String], imports: &Imports) -> bool {
    if let [std, env, method] = segments {
        if std == "std" && env == "env" && RAW_METHODS.contains(&method.as_str()) {
            return true;
        }
    }
    if let [module, method] = segments {
        if imports.module_aliases.contains(module) && RAW_METHODS.contains(&method.as_str()) {
            return true;
        }
        if module == "libc" && method == "getenv" {
            return true;
        }
    }
    matches!(segments, [function] if imports.function_aliases.contains(function))
}

/// Reject direct, imported, aliased, cfg-disabled, or helper-hidden raw reads.
///
/// The argument to `std::env::var` is deliberately irrelevant: a helper that
/// accepts `&str` is still a read and is caught even when no environment-name
/// literal exists in this source.
///
/// # Errors
///
/// Returns every violation found, or a parse error if the input is not Rust.
pub fn check_rust_source(source: &str) -> Result<(), Vec<RawEnvViolation>> {
    let file = syn::parse_file(source)
        .map_err(|error| vec![RawEnvViolation::Parse(error.to_string())])?;
    let mut imports = Imports::default();
    imports.visit_file(&file);

    let read_violations = {
        let mut reads = Reads {
            imports: &imports,
            violations: Vec::new(),
        };
        reads.visit_file(&file);
        reads.violations
    };

    let mut violations = imports.violations;
    violations.extend(read_violations);
    violations.sort_by_key(ToString::to_string);
    violations.dedup();
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}
