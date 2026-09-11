use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use syn::{Attribute, Item, Lit, Meta, Token, parse::Parser, punctuated::Punctuated, visit::Visit};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Truth {
    Yes,
    No,
    Maybe,
}

fn cfg(meta: &Meta) -> Truth {
    match meta {
        Meta::Path(path) if path.is_ident("test") => Truth::No,
        Meta::NameValue(value) if value.path.is_ident("feature") => Truth::Maybe,
        Meta::List(list) => {
            let args = Punctuated::<Meta, Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
                .unwrap();
            let values: Vec<_> = args.iter().map(cfg).collect();
            if list.path.is_ident("not") {
                assert_eq!(values.len(), 1);
                match values[0] {
                    Truth::Yes => Truth::No,
                    Truth::No => Truth::Yes,
                    Truth::Maybe => Truth::Maybe,
                }
            } else if list.path.is_ident("all") {
                if values.contains(&Truth::No) {
                    Truth::No
                } else if values.contains(&Truth::Maybe) {
                    Truth::Maybe
                } else {
                    Truth::Yes
                }
            } else if list.path.is_ident("any") {
                if values.contains(&Truth::Yes) {
                    Truth::Yes
                } else if values.contains(&Truth::Maybe) {
                    Truth::Maybe
                } else {
                    Truth::No
                }
            } else {
                Truth::Maybe
            }
        }
        _ => Truth::Maybe,
    }
}

fn production(attrs: &[Attribute]) -> bool {
    attrs.iter().all(|attr| {
        if attr.path().is_ident("cfg") {
            cfg(&attr.parse_args::<Meta>().unwrap()) != Truth::No
        } else {
            true
        }
    })
}

fn attributes(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(v) => &v.attrs,
        Item::Enum(v) => &v.attrs,
        Item::ExternCrate(v) => &v.attrs,
        Item::Fn(v) => &v.attrs,
        Item::ForeignMod(v) => &v.attrs,
        Item::Impl(v) => &v.attrs,
        Item::Macro(v) => &v.attrs,
        Item::Mod(v) => &v.attrs,
        Item::Static(v) => &v.attrs,
        Item::Struct(v) => &v.attrs,
        Item::Trait(v) => &v.attrs,
        Item::TraitAlias(v) => &v.attrs,
        Item::Type(v) => &v.attrs,
        Item::Union(v) => &v.attrs,
        Item::Use(v) => &v.attrs,
        _ => &[],
    }
}

#[derive(Default, Debug)]
pub struct Source {
    pub identifiers: BTreeSet<String>,
    pub literals: Vec<String>,
    pub raw_literals: Vec<String>,
    pub tokens: String,
    pub public_uses: Vec<String>,
    pub public_traits: BTreeSet<String>,
}

impl Source {
    fn tokens(&mut self, tokens: TokenStream) {
        for token in tokens {
            match token {
                TokenTree::Group(group) => self.tokens(group.stream()),
                TokenTree::Ident(ident) => {
                    self.identifiers.insert(ident.to_string());
                }
                TokenTree::Literal(literal) => {
                    if let Ok(Lit::Str(literal)) = syn::parse_str::<Lit>(&literal.to_string()) {
                        self.literals.push(literal.value());
                        self.raw_literals
                            .push(literal.to_token_stream().to_string());
                    }
                }
                _ => {}
            }
        }
    }

    pub fn names_any(&self, names: &[&str]) -> bool {
        names.iter().any(|name| self.identifiers.contains(*name))
    }

    pub fn contains(&self, text: &str) -> bool {
        self.tokens
            .split_whitespace()
            .collect::<String>()
            .contains(&text.split_whitespace().collect::<String>())
    }
}

impl<'ast> Visit<'ast> for Source {
    fn visit_attribute(&mut self, _: &'ast Attribute) {} // Documentation and cfg strings are not code.
    fn visit_item(&mut self, item: &'ast Item) {
        if !production(attributes(item)) {
            return;
        }
        if let Item::Use(item) = item {
            self.tokens.push_str(&item.to_token_stream().to_string());
            if matches!(item.vis, syn::Visibility::Public(_)) {
                self.public_uses
                    .push(item.tree.to_token_stream().to_string());
            }
        }
        if let Item::Trait(item) = item {
            if matches!(item.vis, syn::Visibility::Public(_)) {
                self.public_traits.insert(item.ident.to_string());
            }
        }
        syn::visit::visit_item(self, item);
    }
    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let attrs = match item {
            syn::ImplItem::Const(v) => &v.attrs,
            syn::ImplItem::Fn(v) => &v.attrs,
            syn::ImplItem::Type(v) => &v.attrs,
            syn::ImplItem::Macro(v) => &v.attrs,
            _ => return,
        };
        if production(attrs) {
            syn::visit::visit_impl_item(self, item);
        }
    }
    fn visit_ident(&mut self, ident: &'ast syn::Ident) {
        self.identifiers.insert(ident.to_string());
        self.tokens.push_str(&format!(" {ident} "));
    }
    fn visit_lit_str(&mut self, literal: &'ast syn::LitStr) {
        self.literals.push(literal.value());
        self.raw_literals
            .push(literal.to_token_stream().to_string());
    }
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.tokens(node.tokens.clone());
        self.tokens.push_str(&node.to_token_stream().to_string());
        syn::visit::visit_path(self, &node.path);
    }
    fn visit_expr(&mut self, expr: &'ast syn::Expr) {
        // Keep expression punctuation for structural predicates (method calls,
        // boolean operators and downcasts), while the visitor handles literals.
        if !matches!(expr, syn::Expr::Block(_)) {
            self.tokens.push_str(&expr.to_token_stream().to_string());
        }
        syn::visit::visit_expr(self, expr);
    }
}

pub fn parse(text: &str) -> Source {
    let file = syn::parse_file(text).expect("parse Rust source");
    let mut source = Source::default();
    if production(&file.attrs) {
        source.visit_file(&file);
    }
    source
}

pub fn module_tree(entry: &Path) -> BTreeMap<PathBuf, Source> {
    fn walk(path: &Path, child_dir: PathBuf, result: &mut BTreeMap<PathBuf, Source>) {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let file =
            syn::parse_file(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        if !production(&file.attrs) {
            return;
        }
        let mut source = Source::default();
        source.visit_file(&file);
        assert!(
            result.insert(path.to_owned(), source).is_none(),
            "duplicate module: {}",
            path.display()
        );
        modules(&file.items, path.parent().unwrap(), &child_dir, result);
    }
    fn modules(
        items: &[Item],
        parent: &Path,
        child_dir: &Path,
        result: &mut BTreeMap<PathBuf, Source>,
    ) {
        for item in items {
            let Item::Mod(module) = item else { continue };
            if !production(&module.attrs) {
                continue;
            }
            let name = module.ident.to_string();
            if let Some((_, items)) = &module.content {
                modules(
                    items,
                    &child_dir.join(&name),
                    &child_dir.join(&name),
                    result,
                );
                continue;
            }
            let explicit = module.attrs.iter().find_map(|attr| {
                if !attr.path().is_ident("path") {
                    return None;
                }
                let Meta::NameValue(value) = &attr.meta else {
                    panic!("path attribute must name a file")
                };
                let syn::Expr::Lit(value) = &value.value else {
                    panic!("literal module path required")
                };
                let Lit::Str(value) = &value.lit else {
                    panic!("string module path required")
                };
                Some(parent.join(value.value()))
            });
            let path = explicit.unwrap_or_else(|| {
                let flat = child_dir.join(format!("{name}.rs"));
                let nested = child_dir.join(&name).join("mod.rs");
                assert_ne!(
                    flat.exists(),
                    nested.exists(),
                    "ambiguous or missing module {}",
                    flat.display()
                );
                if flat.exists() { flat } else { nested }
            });
            let children = if path.file_name().unwrap() == "mod.rs" {
                path.parent().unwrap().to_owned()
            } else {
                path.with_extension("")
            };
            walk(&path, children, result);
        }
    }
    let mut result = BTreeMap::new();
    walk(entry, entry.parent().unwrap().to_owned(), &mut result);
    assert!(
        !result.is_empty(),
        "source tree must contain its production root"
    );
    result
}

#[test]
fn production_analysis_distinguishes_cfg_code_comments_and_literals() {
    let source = parse(
        r##"
        // compio_postgres::Comment
        #[cfg(test)] mod tests { use compio_postgres::TestOnly; }
        #[cfg(test)] fn helper() { let _: rusqlite::Connection; }
        #[cfg(not(test))] fn shipped() { let _: RealDriver; }
        #[cfg(any(test, feature = "production"))] fn possible() { let _: PossibleDriver; }
        #[cfg(all(test, feature = "production"))] fn impossible() { let _: ImpossibleDriver; }
        fn q() { format!(r#"SELECT id FROM users"#); panic!("rusqlite in an error"); }
    "##,
    );
    assert!(!source.names_any(&["compio_postgres", "rusqlite", "ImpossibleDriver"]));
    assert!(source.names_any(&["RealDriver"]));
    assert!(source.names_any(&["PossibleDriver"]));
    assert!(source.literals.contains(&"SELECT id FROM users".into()));
    assert!(parse("fn q(c: compio_postgres::Client) {}").names_any(&["compio_postgres"]));
}

#[test]
fn module_declarations_bound_production_visibility() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join("lib.rs"), "#[cfg(test)] mod tests; mod real;").unwrap();
    std::fs::write(root.join("tests.rs"), "use compio_postgres::Client;").unwrap();
    std::fs::write(root.join("real.rs"), "pub fn real() {}").unwrap();
    let tree = module_tree(&root.join("lib.rs"));
    assert!(tree.contains_key(&root.join("real.rs")));
    assert!(!tree.contains_key(&root.join("tests.rs")));
    std::fs::write(root.join("lib.rs"), "mod tests; mod real;").unwrap();
    assert!(
        module_tree(&root.join("lib.rs"))[&root.join("tests.rs")].names_any(&["compio_postgres"])
    );
}
