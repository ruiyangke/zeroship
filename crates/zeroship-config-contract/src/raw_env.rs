//! Cfg-independent source gate for undeclared Rust environment reads, and for
//! every mutation of the process environment.
//!
//! TWO POPULATIONS, ONE WALK. A raw READ is banned so the population of reads
//! stays enumerable, and two roles may perform one. A WRITE - `set_var`,
//! `remove_var` - is banned because the process environment is global and
//! shared, so a value one test sets is read by every other test in the same
//! binary; Rust 2024 made both `unsafe` because they race with concurrent
//! `getenv` in libc. No role is exempt from the write rule. Handing an
//! environment to a CHILD process (`Command::env`) is untouched: it is an
//! explicit argument at the call site, scoped to the child.
//!
//! Two things share this module because they are the same walk over the same
//! syntax tree: the GATE that Section 4.5 of
//! `docs/proposals/2026-08-11-config-name-alignment.md` requires, and the
//! INVENTORY that says how many reads of each class exist right now. Keeping
//! them together means the number in a report and the number the gate enforces
//! cannot drift, and it means the conversion can be measured while it is in
//! progress rather than only after it finishes.
//!
//! Why a source scan at all, when Clippy's `disallowed_methods` already denies
//! these methods: the lint only sees code the current cfg activates. A read
//! behind a disabled feature compiles out and the lint says nothing, while a
//! different feature selection ships it. This parses the file, so a disabled
//! cfg is still source. That is not hypothetical for the WRITE half:
//! `tests/clippy_gate.sh` lints under a fixed `--features` list that does not
//! include `zeroship-plugin-storage/s3`, so the `set_var` calls that used to
//! sit behind `#[cfg(feature = "s3")]` in `crates/plugin-storage/tests/
//! backend_parity.rs` were invisible to the lint and visible only here.
//!
//! What it does NOT do. It cannot see a read inside a dependency, a read behind
//! a macro this crate does not expand, or a name assembled at run time. The
//! first is out of scope by design (`libs/*` and `crates/*` are first-party;
//! vendored code is not ours to rewrite), the second has no instance today, and
//! the third is why the typed accessors take a key rather than a `&str`.

use std::collections::BTreeSet;

use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ExprCall, ExprPath, ItemUse, Lit, Macro, Meta, Token, UseTree};
use thiserror::Error;

const RAW_METHODS: &[&str] = &["var", "var_os", "vars", "vars_os"];

/// The two ways to MUTATE the process environment.
///
/// A separate population from [`RAW_METHODS`] because the reason differs and so
/// does the exemption set. A raw READ is banned so the population of reads
/// stays enumerable, and two roles are permitted to perform one. A WRITE is
/// banned because the process environment is global and shared: a value one
/// test sets is read by every other test in the same binary and by any thread
/// already running, which is why Rust 2024 made both functions `unsafe` - they
/// race with concurrent `getenv` in libc, and the race is undefined behaviour
/// rather than a flaky assertion. Nothing in this workspace needs to write, so
/// NO role is exempt, not even the central accessor.
///
/// What this deliberately does not reach: giving an environment to a CHILD
/// process. `Command::env` and the shell's `VAR=x cmd` are explicit arguments
/// at the call site, scoped to the child, and are the sanctioned way to test
/// that a variable reaches a binary. Neither is a function on `std::env`, so
/// neither is named here or in [`WRITE_FFI_ESCAPES`].
const WRITE_METHODS: &[&str] = &["set_var", "remove_var"];

/// The FFI spellings of the same mutation, for the same reason `libc::getenv`
/// is watched on the read side: a gate that only knows the `std` path is
/// defeated by one `extern` call.
const WRITE_FFI_ESCAPES: &[&str] = &["setenv", "unsetenv", "putenv"];

/// The typed accessors the registering macros wrap.
///
/// Calling one directly reads the environment WITHOUT emitting a read site, so
/// these are the ways to hold a typed key and still stay invisible to the
/// registry. Banning them is what makes "every read is enumerable" true rather
/// than merely conventional. Their own defining module is exempt by path.
const REGISTRATION_BYPASSES: &[&str] = &[
    "read_typed_env",
    "read_declared_env_value",
    "read_declared_env_os_value",
    "read_process_env_snapshot_value",
];

/// Build inputs a build script may read raw.
///
/// These are compiler and Cargo inputs, not process startup configuration:
/// they exist only while the crate is being compiled, no operator sets them,
/// and there is no process for a typed consumer marker to name. Section 4.5
/// exempts them explicitly. The list is exact rather than a `CARGO_` prefix
/// test plus a wildcard, so adding one is a visible edit.
const BUILD_INPUTS: &[&str] = &[
    "OUT_DIR",
    "TARGET",
    "HOST",
    "PROFILE",
    "NUM_JOBS",
    "OPT_LEVEL",
    "DEBUG",
    "RUSTC",
    "RUSTDOC",
    // Cargo sets this for every build script and it cannot be absent when one runs, so
    // it meets this list's own test exactly. It is already the ordinary spelling
    // elsewhere in the tree as the compile-time `env!("CARGO_MANIFEST_DIR")`, which this
    // scanner reads on a different path; `crates/zeroship-migrate-node/build.rs` needs
    // the run-time form because it walks the workspace from it.
    "CARGO_MANIFEST_DIR",
];

/// The exact path of the one module allowed to touch `std::env`.
///
/// This said `crates/core/src/config/env.rs` until 2026-08-28. A path constant that no
/// longer names a file does not fail loudly: `FileRole::for_path` simply stopped
/// matching, the central accessor was reclassified `Ordinary`, and the gate began
/// reporting the ONE file whose whole purpose is raw access as a violation - along with
/// its sanctioned `#[allow]`.
///
/// The rename landed in `105a75131` (2026-08-26, "every crate directory is named for the
/// package it holds"), which moved the directory and left this constant behind. That
/// commit is still the last one to touch this file, the accessor it names, and
/// `crates/zeroship-core/tests/config_env_access_gate.rs` - so the source half of the
/// raw-environment rule failed from there to here. Keep this in step with the directory.
pub const CENTRAL_ACCESSOR: &str = "crates/zeroship-core/src/config/env.rs";

/// The exact tail every sealed library test-key module must have.
///
/// Section 4.5 permits `libs/<crate>/tests/common/env.rs` and nothing else.
/// `libs/*` are publishable and zeroship-independent, so they cannot depend on
/// the core config types; a sealed local enum is the substitute, and pinning it
/// to one path is what stops "sealed accessor" becoming a phrase any file can
/// claim by writing a comment.
pub const SEALED_LIB_TEST_MODULE: &str = "tests/common/env.rs";

/// What the gate permits in one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRole {
    /// Ordinary first-party source: no raw access of any kind.
    Ordinary,
    /// [`CENTRAL_ACCESSOR`]: raw access is the point of the file.
    CentralAccessor,
    /// `libs/<crate>/tests/common/env.rs`: raw access inside its accessor only.
    SealedLibraryTest,
    /// A build script: the exact [`BUILD_INPUTS`] names, read raw.
    BuildScript,
}

impl FileRole {
    /// Classify a repository-relative path.
    ///
    /// Path-driven on purpose. A role that a file could assert about itself -
    /// an attribute, a marker comment - would be a role any file could claim.
    #[must_use]
    pub fn for_path(path: &str) -> Self {
        if path == CENTRAL_ACCESSOR {
            return Self::CentralAccessor;
        }
        if path.starts_with("libs/") && path.ends_with(SEALED_LIB_TEST_MODULE) {
            return Self::SealedLibraryTest;
        }
        if path == "build.rs" || path.ends_with("/build.rs") {
            return Self::BuildScript;
        }
        Self::Ordinary
    }
}

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
    /// The process environment was MUTATED. Permitted in no role.
    #[error("process-global environment mutation: {0}")]
    Write(String),
    /// A zeroship-owned name was captured at compile time.
    #[error("undeclared compile-time environment read: {0}")]
    CompileTime(String),
    /// The typed accessor was called without the registering macro.
    #[error("typed environment read bypasses ReadSite registration: {0}")]
    UnregisteredRead(String),
    /// The disallowed-method lint was locally silenced outside the exempt paths.
    #[error("illicit allow of the raw-environment lint: {0}")]
    IllicitAllow(String),
    /// A sealed library test module did not have the shape that seals it.
    #[error("sealed library test accessor is not sealed: {0}")]
    SealedShape(String),
    /// A declared key named something that is not an environment spelling.
    #[error("declared environment key has an invalid name: {0}")]
    InvalidKeyName(String),
    /// An `external`-class key claimed a zeroship-owned name.
    #[error("external-class key claims a zeroship-owned name: {0}")]
    MisclassifiedKey(String),
    /// The scan covered no source at all, so a clean result proves nothing.
    #[error("raw environment scan examined zero source files")]
    EmptyScan,
}

/// One declared non-config key found in source, for the classification report.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeclaredKeySite {
    /// Repository-relative file the key literal appears in.
    pub file: String,
    /// Constructor used, and therefore the stated class.
    pub class: String,
    /// The literal environment name.
    pub name: String,
}

/// What one scan found, beyond pass or fail.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RawEnvReport {
    /// Files examined.
    pub files: usize,
    /// Every declared non-config key literal, in file order.
    pub declared_keys: Vec<DeclaredKeySite>,
    /// Raw accesses permitted by the file's role, for the exemption ledger.
    pub permitted_raw: Vec<String>,
}

#[derive(Default)]
struct Imports {
    function_aliases: BTreeSet<String>,
    /// Local names bound to `std::env::set_var` / `remove_var`. Kept apart
    /// from `function_aliases` so a write is never reported as a read: the two
    /// carry different messages and different exemption rules.
    write_aliases: BTreeSet<String>,
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
    if prefix == ["std", "env"] && WRITE_METHODS.contains(&original) {
        imports.write_aliases.insert(local.to_owned());
        imports.violations.push(RawEnvViolation::Write(format!(
            "imported as std::env::{original} as {local}"
        )));
    }
}

struct Reads<'a> {
    imports: &'a Imports,
    role: FileRole,
    violations: Vec<RawEnvViolation>,
    declared_keys: Vec<(String, String)>,
    permitted_raw: Vec<String>,
    /// Raw accesses seen, counted whatever the role. The sealed-library shape
    /// check needs the count even when the role suppresses the violation.
    raw_reads: usize,
    /// Environment names passed to a raw access as a string literal.
    raw_literal_arguments: Vec<String>,
}

impl Reads<'_> {
    /// Record a key declared inline by one of the reading macros.
    ///
    /// `declared_env!(class, "NAME", Consumer)` and its `_os` twin carry the
    /// class as the first argument; `test_env!("NAME")` and its `_os` twin are
    /// `test` by definition. Anything else is left alone.
    fn collect_macro_declared_key(
        &mut self,
        macro_name: Option<&str>,
        arguments: &Punctuated<Expr, Token![,]>,
    ) {
        let (class, name_index) = match macro_name {
            Some("declared_env" | "declared_env_os") => {
                let Some(Expr::Path(path)) = arguments.first() else {
                    return;
                };
                let Some(class) = path.path.get_ident().map(ToString::to_string) else {
                    return;
                };
                (class, 1)
            }
            Some("test_env" | "test_env_os") => ("test".to_owned(), 0),
            _ => return,
        };
        if !matches!(
            class.as_str(),
            "external" | "test" | "dev" | "cli" | "build" | "creator" | "platform"
        ) {
            return;
        }
        if let Some(Expr::Lit(literal)) = arguments.get(name_index)
            && let Lit::Str(value) = &literal.lit
        {
            self.declared_keys.push((class, value.value()));
        }
    }
}

impl<'ast> Visit<'ast> for Reads<'_> {
    /// Handle a raw read at the CALL, not at the callee path.
    ///
    /// The build-script exemption depends on the literal argument, which only
    /// exists here. Descending into the callee afterwards would report the same
    /// read a second time from `visit_expr_path`, so this arm walks the
    /// arguments itself and deliberately does not visit `call.func`.
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        // The write arm, handled here and not at the callee path for the same
        // reason the read arm is: descending into `call.func` afterwards would
        // report the same mutation a second time from `visit_expr_path`. No
        // role permits a write, so unlike the read arm there is no literal
        // argument to inspect and no exemption to consult.
        if let Expr::Path(path) = call.func.as_ref() {
            let segments = path_segments(path);
            if is_write_path(&segments, self.imports) {
                let line = call.func.span().start().line;
                self.violations.push(RawEnvViolation::Write(format!(
                    "{line}: {}",
                    segments.join("::")
                )));
                for argument in &call.args {
                    self.visit_expr(argument);
                }
                return;
            }
        }
        let raw_callee = match call.func.as_ref() {
            Expr::Path(path) => {
                let segments = path_segments(path);
                if let Some(class) = declared_key_class(&segments)
                    && let Some(name) = first_string_literal(call)
                {
                    self.declared_keys.push((class, name));
                }
                is_raw_path(&segments, self.imports).then(|| segments.join("::"))
            }
            _ => None,
        };
        if let Some(joined) = raw_callee {
            let literal = first_string_literal(call);
            self.raw_reads += 1;
            if let Some(name) = literal.clone() {
                self.raw_literal_arguments.push(name);
            }
            if self.role == FileRole::BuildScript
                && literal
                    .as_deref()
                    .is_some_and(|name| BUILD_INPUTS.contains(&name))
            {
                self.permitted_raw
                    .push(format!("{joined}({})", literal.unwrap_or_default()));
            } else {
                let line = call.func.span().start().line;
                self.violations
                    .push(RawEnvViolation::Read(format!("{line}: {joined}")));
            }
            for argument in &call.args {
                self.visit_expr(argument);
            }
            return;
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_path(&mut self, path: &'ast ExprPath) {
        let segments = path_segments(path);
        let line = path.span().start().line;
        // A write reached WITHOUT calling it - `let f = std::env::set_var;`,
        // or handing it to a `map` - is the same mutation one indirection
        // away. `visit_expr_call` returns early for a direct call, so this arm
        // only ever sees the indirect form.
        if is_write_path(&segments, self.imports) {
            self.violations.push(RawEnvViolation::Write(format!(
                "{line}: {}",
                segments.join("::")
            )));
        }
        if is_raw_path(&segments, self.imports) {
            self.violations.push(RawEnvViolation::Read(format!(
                "{line}: {}",
                segments.join("::")
            )));
        }
        if segments
            .last()
            .is_some_and(|last| REGISTRATION_BYPASSES.contains(&last.as_str()))
        {
            self.violations.push(RawEnvViolation::UnregisteredRead(format!(
                "{line}: {}",
                segments.join("::")
            )));
        }
        visit::visit_expr_path(self, path);
    }

    /// Look INSIDE a macro invocation, not just at its name.
    ///
    /// `syn`'s default walk stops at the token stream, so
    /// `assert!(std::env::var(NAME).is_err())` was invisible to this scanner
    /// until 2026-08-12: `crates/zeroship-plugin-storage/src/limits.rs:198` was a real
    /// raw read that the file's other read shadowed in every count. A gate
    /// whose blind spot is "wrap it in `assert!`" is not a gate.
    ///
    /// Most macro bodies parse as a comma-separated expression list, so the
    /// first arm re-uses the ordinary expression walk and keeps exact spans.
    /// When they do not (a macro taking a type, a match arm, arbitrary
    /// tokens), the fallback is a TOKEN TEXT test, which cannot resolve
    /// aliases and has no line number, but fails loudly rather than passing
    /// silently.
    fn visit_macro(&mut self, mac: &'ast Macro) {
        let name = mac
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string());
        if matches!(name.as_deref(), Some("env" | "option_env"))
            && mac.tokens.to_string().contains("ZEROSHIP_")
        {
            self.violations.push(RawEnvViolation::CompileTime(
                name.clone().unwrap_or_else(|| "environment macro".to_owned()),
            ));
        }
        if let Ok(arguments) =
            mac.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
        {
            // The ONE-LINE reading macros declare their key inline, so the
            // `DeclaredEnvKey::<class>("NAME")` call the census looks for does
            // not exist until macro expansion - which a source scan never sees.
            // Missing this made the census report 5 platform-class names when
            // the tree had many more, all of them in the ergonomic spelling
            // every conversion was told to prefer.
            self.collect_macro_declared_key(name.as_deref(), &arguments);
            for argument in &arguments {
                self.visit_expr(argument);
            }
        } else if let Some(spelling) = raw_spelling_in_tokens(&mac.tokens) {
            self.violations.push(RawEnvViolation::Read(format!(
                "in {} macro body: {spelling}",
                name.clone().unwrap_or_else(|| "unnamed".to_owned())
            )));
        } else if let Some(spelling) = write_spelling_in_tokens(&mac.tokens) {
            self.violations.push(RawEnvViolation::Write(format!(
                "in {} macro body: {spelling}",
                name.unwrap_or_else(|| "unnamed".to_owned())
            )));
        }
        visit::visit_macro(self, mac);
    }

    fn visit_attribute(&mut self, attribute: &'ast Attribute) {
        if self.role == FileRole::Ordinary && silences_raw_env_lint(attribute) {
            self.violations.push(RawEnvViolation::IllicitAllow(
                "clippy::disallowed_methods".to_owned(),
            ));
        }
        visit::visit_attribute(self, attribute);
    }
}

/// Find a raw-read spelling in a token stream that would not parse as Rust.
///
/// STRING LITERALS ARE EXCLUDED, and that is not a nicety: this scanner's own
/// test asserts on `path.ends_with("std::env::var")` inside a `matches!` guard,
/// which does not parse as an expression list. A text test over the printed
/// tokens flagged that literal and made the gate fail on the file that proves
/// the gate works. Dropping `Literal` tokens is the difference between "the
/// code performs a read" and "the code mentions one".
///
/// This still sees only the literal `std::env::<method>` and `libc::getenv`
/// spellings: an alias imported elsewhere in the file is beyond a text test,
/// which is why it is the FALLBACK and not the mechanism.
fn raw_spelling_in_tokens(tokens: &proc_macro2::TokenStream) -> Option<String> {
    let mut flat = String::new();
    flatten_non_literal_tokens(tokens, &mut flat);
    for method in RAW_METHODS {
        let spelling = format!("std::env::{method}");
        if flat.contains(&spelling) {
            return Some(spelling);
        }
    }
    flat.contains("libc::getenv")
        .then(|| "libc::getenv".to_owned())
}

/// The write-side twin of [`raw_spelling_in_tokens`], with the same blind spot
/// and the same reason for excluding string literals: this file's own tests
/// name `std::env::set_var` inside `matches!` guards and assertion messages,
/// and a text test that counted those would fail on the code that proves the
/// gate works.
fn write_spelling_in_tokens(tokens: &proc_macro2::TokenStream) -> Option<String> {
    let mut flat = String::new();
    flatten_non_literal_tokens(tokens, &mut flat);
    for method in WRITE_METHODS {
        let spelling = format!("std::env::{method}");
        if flat.contains(&spelling) {
            return Some(spelling);
        }
    }
    WRITE_FFI_ESCAPES
        .iter()
        .map(|escape| format!("libc::{escape}"))
        .find(|spelling| flat.contains(spelling))
}

fn flatten_non_literal_tokens(tokens: &proc_macro2::TokenStream, out: &mut String) {
    for tree in tokens.clone() {
        match tree {
            proc_macro2::TokenTree::Group(group) => {
                flatten_non_literal_tokens(&group.stream(), out);
            }
            proc_macro2::TokenTree::Ident(ident) => out.push_str(&ident.to_string()),
            proc_macro2::TokenTree::Punct(punct) => out.push(punct.as_char()),
            proc_macro2::TokenTree::Literal(_) => out.push(' '),
        }
    }
}

fn path_segments(path: &ExprPath) -> Vec<String> {
    path.path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect()
}

fn first_string_literal(call: &ExprCall) -> Option<String> {
    match call.args.first()? {
        Expr::Lit(literal) => match &literal.lit {
            Lit::Str(value) => Some(value.value()),
            _ => None,
        },
        _ => None,
    }
}

/// Recognize `DeclaredEnvKey::<class>("NAME")` however it is spelled.
fn declared_key_class(segments: &[String]) -> Option<String> {
    let [.., type_name, method] = segments else {
        return None;
    };
    // A FAMILY prefix counts too: it is a declared, classified name shape, and
    // leaving it out would make the census under-report exactly the open-ended
    // case that is hardest to see.
    if type_name != "DeclaredEnvKey" && type_name != "DeclaredEnvFamily" {
        return None;
    }
    matches!(
        method.as_str(),
        "external" | "test" | "dev" | "cli" | "build" | "creator" | "platform"
    )
    .then(|| method.clone())
}

/// Whether an attribute silences the raw-environment lint.
///
/// Matches `allow`, `expect` and the tool-namespaced spellings, because all
/// three suppress the diagnostic and only one of them is the obvious one.
fn silences_raw_env_lint(attribute: &Attribute) -> bool {
    let path = attribute.path();
    let is_suppression = path.is_ident("allow")
        || path.is_ident("expect")
        || path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "allow" || segment.ident == "expect");
    if !is_suppression {
        return false;
    }
    let Meta::List(list) = &attribute.meta else {
        return false;
    };
    let tokens = list.tokens.to_string().replace(' ', "");
    tokens.contains("clippy::disallowed_methods")
}

/// Whether a call path MUTATES the process environment.
///
/// The mirror of [`is_raw_path`] over [`WRITE_METHODS`]. A METHOD call is not
/// reachable here by construction, which is what keeps this from flagging the
/// unrelated `set_var` methods the workspace legitimately owns -
/// `EnvStore::set_var` in the control plane is `store.set_var(..)`, a method
/// receiver and not a path, and `syn` routes it to `visit_expr_method_call`.
/// Only a free function reached through `std::env`, an alias of that module,
/// or an alias of the function itself matches.
fn is_write_path(segments: &[String], imports: &Imports) -> bool {
    if let [std, env, method] = segments
        && std == "std"
        && env == "env"
        && WRITE_METHODS.contains(&method.as_str())
    {
        return true;
    }
    if let [module, method] = segments {
        if imports.module_aliases.contains(module) && WRITE_METHODS.contains(&method.as_str()) {
            return true;
        }
        if module == "libc" && WRITE_FFI_ESCAPES.contains(&method.as_str()) {
            return true;
        }
    }
    matches!(segments, [function] if imports.write_aliases.contains(function))
}

fn is_raw_path(segments: &[String], imports: &Imports) -> bool {
    if let [std, env, method] = segments
        && std == "std"
        && env == "env"
        && RAW_METHODS.contains(&method.as_str())
    {
        return true;
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

/// Require a sealed library test module to actually be sealed.
///
/// The path exemption alone would let any file under
/// `libs/<crate>/tests/common/env.rs` do anything at all, which turns Section
/// 4.5's carve-out into "raw reads are fine if you put them in a file with this
/// name". Three properties are what make the carve-out equivalent in kind to
/// the typed keys the rest of the workspace uses:
///
///   * ONE raw access. Two means the accessor is not the only door.
///   * A key ENUM with unit variants, so the permitted names are a closed,
///     scannable set rather than whatever a caller passes.
///   * No string literal reaching the raw call. A literal there means a caller
///     could name a variable the enum does not, which is the closed set gone.
///
/// What this does NOT check: that the enum's `name` arms are literals rather
/// than computed. A computed arm would still have to be a `const fn` returning
/// `&'static str`, so the set stays finite, but its contents would no longer be
/// greppable. That is a gap, and it is smaller than the one being closed.
fn check_sealed_shape(
    file: &syn::File,
    raw_reads: usize,
    raw_literal_arguments: &[String],
) -> Vec<RawEnvViolation> {
    let mut violations = Vec::new();
    if raw_reads > 1 {
        violations.push(RawEnvViolation::SealedShape(format!(
            "{raw_reads} raw accesses; a sealed accessor has exactly one"
        )));
    }
    if !raw_literal_arguments.is_empty() {
        violations.push(RawEnvViolation::SealedShape(format!(
            "raw access takes a string literal ({}); it must take the sealed key",
            raw_literal_arguments.join(", ")
        )));
    }
    let has_unit_variant_enum = file.items.iter().any(|item| match item {
        syn::Item::Enum(declaration) => {
            !declaration.variants.is_empty()
                && declaration
                    .variants
                    .iter()
                    .all(|variant| matches!(variant.fields, syn::Fields::Unit))
        }
        _ => false,
    });
    if !has_unit_variant_enum {
        violations.push(RawEnvViolation::SealedShape(
            "no unit-variant key enum; the permitted names are not a closed set".to_owned(),
        ));
    }
    violations
}

/// Reject direct, imported, aliased, cfg-disabled, or helper-hidden raw reads.
///
/// The argument to `std::env::var` is deliberately irrelevant outside a build
/// script: a helper that accepts `&str` is still a read and is caught even when
/// no environment-name literal exists in this source.
///
/// # Errors
///
/// Returns every violation found, or a parse error if the input is not Rust.
pub fn check_rust_source(source: &str) -> Result<(), Vec<RawEnvViolation>> {
    check_rust_source_with_role(source, FileRole::Ordinary).map(|_| ())
}

/// Check one source under an explicit role, returning what it declared.
///
/// # Errors
///
/// Returns every violation the role does not permit.
pub fn check_rust_source_with_role(
    source: &str,
    role: FileRole,
) -> Result<RawEnvReport, Vec<RawEnvViolation>> {
    let file =
        syn::parse_file(source).map_err(|error| vec![RawEnvViolation::Parse(error.to_string())])?;
    let mut imports = Imports::default();
    imports.visit_file(&file);

    let reads = {
        let mut reads = Reads {
            imports: &imports,
            role,
            violations: Vec::new(),
            declared_keys: Vec::new(),
            permitted_raw: Vec::new(),
            raw_reads: 0,
            raw_literal_arguments: Vec::new(),
        };
        reads.visit_file(&file);
        (
            reads.violations,
            reads.declared_keys,
            reads.permitted_raw,
            reads.raw_reads,
            reads.raw_literal_arguments,
        )
    };
    let (read_violations, declared_keys, permitted_raw, raw_reads, raw_literal_arguments) = reads;

    let mut violations = imports.violations;
    violations.extend(read_violations);

    // The two exempt roles are exempt from RAW ACCESS, not from everything: a
    // registration bypass or a compile-time zeroship read is still a violation
    // there, because neither has anything to do with why the exemption exists.
    if matches!(
        role,
        FileRole::CentralAccessor | FileRole::SealedLibraryTest
    ) {
        violations.retain(|violation| {
            !matches!(
                violation,
                RawEnvViolation::Read(_)
                    | RawEnvViolation::Import(_)
                    | RawEnvViolation::IllicitAllow(_)
            )
        });
    }
    if role == FileRole::SealedLibraryTest {
        violations.extend(check_sealed_shape(&file, raw_reads, &raw_literal_arguments));
    }

    let mut report = RawEnvReport {
        files: 1,
        declared_keys: declared_keys
            .into_iter()
            .map(|(class, name)| DeclaredKeySite {
                file: String::new(),
                class,
                name,
            })
            .collect(),
        permitted_raw,
    };

    for key in &report.declared_keys {
        if !is_env_spelling(&key.name) {
            violations.push(RawEnvViolation::InvalidKeyName(format!(
                "{}::{}",
                key.class, key.name
            )));
        }
        if key.class == "external" && key.name.starts_with("ZEROSHIP_") {
            violations.push(RawEnvViolation::MisclassifiedKey(key.name.clone()));
        }
    }

    violations.sort_by_key(ToString::to_string);
    violations.dedup();
    if violations.is_empty() {
        report.declared_keys.sort();
        Ok(report)
    } else {
        Err(violations)
    }
}

/// Whether a literal is a plausible environment-variable spelling.
///
/// Mirrors `zeroship_core::config::is_valid_env_name`. It is restated rather
/// than imported because this crate scans SOURCE: the value being checked is a
/// string literal lifted out of a syntax tree, not a value the core crate ever
/// sees, and adding the dependency would not make the two agree by force.
#[must_use]
pub fn is_env_spelling(name: &str) -> bool {
    !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Scan a whole set of named sources, refusing to report a clean empty run.
///
/// [`check_rust_source`] answers "is THIS file clean?", which is silently true
/// for a file set that a broken enumeration left empty. The gate that walks
/// `git ls-files` must call this instead, so a glob that stops matching fails
/// loudly rather than reporting zero violations.
///
/// # Errors
///
/// Returns [`RawEnvViolation::EmptyScan`] for an empty set, otherwise every
/// violation found, each prefixed with its source name.
pub fn check_sources(sources: &[(String, String)]) -> Result<usize, Vec<RawEnvViolation>> {
    scan_sources_by_role(sources).map(|report| report.files)
}

/// Scan a named source set, applying each file's path-derived role.
///
/// # Errors
///
/// Returns [`RawEnvViolation::EmptyScan`] for an empty set, otherwise every
/// violation found, each prefixed with its source name.
pub fn scan_sources_by_role(
    sources: &[(String, String)],
) -> Result<RawEnvReport, Vec<RawEnvViolation>> {
    if sources.is_empty() {
        return Err(vec![RawEnvViolation::EmptyScan]);
    }
    let mut violations = Vec::new();
    let mut report = RawEnvReport {
        files: sources.len(),
        ..RawEnvReport::default()
    };
    for (name, source) in sources {
        match check_rust_source_with_role(source, FileRole::for_path(name)) {
            Ok(found) => {
                report
                    .declared_keys
                    .extend(found.declared_keys.into_iter().map(|mut key| {
                        key.file.clone_from(name);
                        key
                    }));
                report
                    .permitted_raw
                    .extend(found.permitted_raw.into_iter().map(|raw| format!("{name}: {raw}")));
            }
            Err(found) => violations.extend(found.into_iter().map(|violation| prefix(name, violation))),
        }
    }
    if violations.is_empty() {
        Ok(report)
    } else {
        Err(violations)
    }
}

/// Collect every declared key literal, whatever else the sources violate.
///
/// [`scan_sources_by_role`] drops its report when it finds a violation, which
/// is right for a gate and wrong for a census: while tracked blockers remain,
/// the class counts would be unavailable exactly when someone might add to
/// them. This walks the same syntax trees and answers only "which keys are
/// declared, in which class".
///
/// # Errors
///
/// Returns a parse error for any source that is not Rust, and refuses an empty
/// set for the same reason [`scan_sources_by_role`] does.
pub fn collect_declared_keys(
    sources: &[(String, String)],
) -> Result<Vec<DeclaredKeySite>, Vec<RawEnvViolation>> {
    if sources.is_empty() {
        return Err(vec![RawEnvViolation::EmptyScan]);
    }
    let mut keys = Vec::new();
    let mut errors = Vec::new();
    for (name, source) in sources {
        let file = match syn::parse_file(source) {
            Ok(file) => file,
            Err(error) => {
                errors.push(RawEnvViolation::Parse(format!("{name}: {error}")));
                continue;
            }
        };
        let imports = Imports::default();
        let mut reads = Reads {
            imports: &imports,
            role: FileRole::Ordinary,
            violations: Vec::new(),
            declared_keys: Vec::new(),
            permitted_raw: Vec::new(),
            raw_reads: 0,
            raw_literal_arguments: Vec::new(),
        };
        reads.visit_file(&file);
        keys.extend(
            reads
                .declared_keys
                .into_iter()
                .map(|(class, key)| DeclaredKeySite {
                    file: name.clone(),
                    class,
                    name: key,
                }),
        );
    }
    if errors.is_empty() {
        keys.sort();
        Ok(keys)
    } else {
        Err(errors)
    }
}

fn prefix(name: &str, violation: RawEnvViolation) -> RawEnvViolation {
    match violation {
        RawEnvViolation::Parse(detail) => RawEnvViolation::Parse(format!("{name}: {detail}")),
        RawEnvViolation::Import(detail) => RawEnvViolation::Import(format!("{name}: {detail}")),
        RawEnvViolation::Read(detail) => RawEnvViolation::Read(format!("{name}: {detail}")),
        RawEnvViolation::Write(detail) => RawEnvViolation::Write(format!("{name}: {detail}")),
        RawEnvViolation::CompileTime(detail) => {
            RawEnvViolation::CompileTime(format!("{name}: {detail}"))
        }
        RawEnvViolation::UnregisteredRead(detail) => {
            RawEnvViolation::UnregisteredRead(format!("{name}: {detail}"))
        }
        RawEnvViolation::IllicitAllow(detail) => {
            RawEnvViolation::IllicitAllow(format!("{name}: {detail}"))
        }
        RawEnvViolation::SealedShape(detail) => {
            RawEnvViolation::SealedShape(format!("{name}: {detail}"))
        }
        RawEnvViolation::InvalidKeyName(detail) => {
            RawEnvViolation::InvalidKeyName(format!("{name}: {detail}"))
        }
        RawEnvViolation::MisclassifiedKey(detail) => {
            RawEnvViolation::MisclassifiedKey(format!("{name}: {detail}"))
        }
        RawEnvViolation::EmptyScan => RawEnvViolation::EmptyScan,
    }
}
