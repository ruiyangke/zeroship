use super::{repo, source};
use syn::{
    parse::Parser, punctuated::Punctuated, visit::Visit, Attribute, ItemMod, ItemTrait, Meta, Token,
};

fn gates_item(meta: &Meta) -> bool {
    if meta.path().is_ident("cfg") {
        return true;
    }
    let Meta::List(list) = meta else { return false };
    if !list.path.is_ident("cfg_attr") {
        return false;
    }
    Punctuated::<Meta, Token![,]>::parse_terminated
        .parse2(list.tokens.clone())
        .expect("cfg_attr arguments")
        .iter()
        .skip(1)
        .any(gates_item)
}

fn gated(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| gates_item(&attr.meta))
}

#[derive(Default)]
struct Contracts {
    examined: usize,
    parent_gated: bool,
    violations: Vec<String>,
}

impl<'ast> Visit<'ast> for Contracts {
    fn visit_item_mod(&mut self, module: &'ast ItemMod) {
        if !source::production(&module.attrs) {
            return;
        }
        let previous = self.parent_gated;
        self.parent_gated |= gated(&module.attrs);
        syn::visit::visit_item_mod(self, module);
        self.parent_gated = previous;
    }

    fn visit_item_trait(&mut self, item: &'ast ItemTrait) {
        if !matches!(item.vis, syn::Visibility::Public(_)) || !source::production(&item.attrs) {
            return;
        }
        self.examined += 1;
        if self.parent_gated || gated(&item.attrs) {
            self.violations.push(item.ident.to_string());
        }
        for member in &item.items {
            self.examined += 1;
            let (name, attrs) = match member {
                syn::TraitItem::Fn(item) => (item.sig.ident.to_string(), &item.attrs),
                syn::TraitItem::Type(item) => (item.ident.to_string(), &item.attrs),
                syn::TraitItem::Const(item) => (item.ident.to_string(), &item.attrs),
                syn::TraitItem::Macro(_) => {
                    self.violations
                        .push(format!("{}: macro-generated contract", item.ident));
                    continue;
                }
                _ => panic!("unsupported trait member in {}", item.ident),
            };
            if gated(attrs) {
                self.violations.push(format!("{}::{name}", item.ident));
            }
        }
    }
}

fn inspect(input: &str) -> Contracts {
    let file = syn::parse_file(input).expect("parse contract source");
    let mut result = Contracts {
        parent_gated: gated(&file.attrs),
        ..Contracts::default()
    };
    result.visit_file(&file);
    result
}

#[test]
fn driver_and_storage_contracts_are_unconditional() {
    let mut examined = 0;
    for name in ["driver", "storage", "lock_policy"] {
        let path = format!("crates/zeroship-data-orm/src/{name}.rs");
        let result = inspect(&repo::read(&path));
        assert!(result.examined > 0, "{path}: no contract items examined");
        examined += result.examined;
        assert!(
            result.violations.is_empty(),
            "{path}: gated contracts: {:?}",
            result.violations
        );
    }
    assert!(examined >= 20, "contract scan lost its trait members");
}

#[test]
fn contract_check_rejects_conditional_traits_and_members_regardless_of_formatting() {
    for input in [
        "#[cfg(feature = \"driver\")] pub trait Driver { fn run(&self); }",
        "pub trait Driver { #[cfg(feature = \"driver\")] fn run(&self); }",
        "pub unsafe trait Driver { #[cfg(\n feature = \"driver\"\n)] unsafe fn run(&self); }",
        "pub trait Driver { #[cfg(any(feature = \"a\", feature = \"b\"))] type Row; }",
        "pub trait Driver { #[cfg_attr(feature = \"a\", cfg(unix))] const MODE: bool; }",
        "#[cfg(feature = \"driver\")] mod backend { pub trait Driver {} }",
        "#![cfg(feature = \"driver\")] pub trait Driver {}",
        "pub trait Driver { define_methods!(); }",
    ] {
        assert!(
            !inspect(input).violations.is_empty(),
            "accepted conditional contract: {input}"
        );
    }
}

#[test]
fn contract_check_ignores_documentation_test_modules_and_non_gating_attributes() {
    let result = inspect(
        r##"
        // #[cfg(feature = "driver")] pub trait Imaginary {}
        #[cfg_attr(feature = "driver", allow(dead_code))]
        pub trait Driver {
            #[allow(async_fn_in_trait)]
            async fn run(&self) { let _ = r#"#[cfg(feature = "driver")]"#; }
            type Row;
        }
        #[cfg(test)] mod tests {
            #[cfg(feature = "driver")] pub trait TestOnly {}
        }
    "##,
    );
    assert_eq!(result.examined, 3);
    assert!(result.violations.is_empty());
}
