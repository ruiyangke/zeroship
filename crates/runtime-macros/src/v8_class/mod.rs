//! `#[v8_class]` proc macro — wraps a Rust `impl` block as a V8
//! ObjectTemplate-backed class.
//!
//! Walks the impl block, collects methods marked with `#[v8_method]`,
//! `#[v8_async_method]`, `#[v8_getter]`, `#[v8_setter]`,
//! `#[v8_constructor]`, and emits a `Self::install(scope) ->
//! v8::Local<v8::FunctionTemplate>` function plus per-method
//! callbacks.
//!
//! Instance state lives in the V8 object's internal field (slot 0): a
//! `Box<Self>` is stored as `External` and reclaimed via a guaranteed
//! V8 weak-finalizer when the wrapper is GC'd.
//!
//! Argument and return marshaling lives in the parent crate's
//! `gen_extract` + `gen_call_return` helpers. Supported types: String,
//! bool, u32, i32, f64, Vec<u8>, Option<T>, Result<T, OpError>, plus
//! `v8::Local<v8::Value>` passthrough for union-typed args.
//!
//! ### Async methods (`#[v8_async_method]`)
//!
//! Async-marked methods compile to a sync V8 callback that allocates a
//! `v8::PromiseResolver`, spawns the user's `async fn` body via
//! `state.spawned_ops`, and returns the Promise immediately. The pump
//! resolves (or rejects) the bound resolver from `OpResult::JsValue`
//! when the future settles. `&mut self` async methods are rejected at
//! compile time — borrow across `.await` is unsound under V8 re-entry.
//! Use `&self` with `Cell` / `RefCell` for state that needs to mutate
//! inside the body. See `gen_async_method_callback`'s doc comment for
//! the borrow-safety contract.
//!
//! ## Same-name getter+setter pairing
//!
//! Defining `#[v8_getter] value(&self)` AND `#[v8_setter] value(&mut
//! self, v)` in the same impl block is illegal Rust (duplicate method
//! names). The supported pattern: rename the Rust fns and apply
//! `#[v8_name = "value"]` to both halves. The install codegen pairs
//! by JS-visible name into a single `set_accessor_property("value",
//! getter, setter, attrs)` call rather than two installs that would
//! each overwrite the previous. See `tests/v8_paired_accessor_smoke.rs`
//! for the supported shapes.
//!
//! ## Submodule layout
//!
//! - `parse` — attribute parsing (`extract_*` helpers), `MethodKind`
//!   classifier, receiver-shape predicates.
//! - `method` — slow-path FunctionCallback codegen for methods, getters,
//!   setters, async methods, static methods/getters, and constructors.
//!   Hosts `gen_box_and_install_finalizer` and the re-entrancy guard.
//! - `fastcall` — `#[v8_method(fastcall)]` / `#[v8_getter(fastcall)]`
//!   shim emission and signature validation.
//! - `helpers` — cross-submodule utilities: `method_callback_ident`,
//!   `gen_param_extractions`, `parse_params_skipping_self`, type
//!   classification predicates.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use syn::{ItemImpl, Type};

mod analyze;
pub(crate) mod ast;
mod emit;
mod fastcall;
mod helpers;
mod parse;
pub(crate) mod shared;

pub(crate) use ast::{ClassMethod, ConstDecl, ConstKind, MethodKind};

use parse::{extract_state_marker, resolve_state_and_marker};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand_tokens(attr.into(), item.into()).into()
}

/// proc-macro2 entry — same logic as [`expand`] but operates on
/// `TokenStream2` so unit tests in this crate can call it without going
/// through the proc-macro driver. Insta snapshots in
/// `tests/v8_class_codegen_snapshot.rs` consume this entry.
pub fn expand_tokens(_attr: TokenStream2, item: TokenStream2) -> TokenStream2 {
    let input: ItemImpl = match syn::parse2(item) {
        Ok(parsed) => parsed,
        Err(e) => return e.to_compile_error(),
    };

    let receiver_ty = match extract_class_ident(&input.self_ty) {
        Some(t) => t,
        None => {
            return syn::Error::new_spanned(
                &input.self_ty,
                "#[v8_class] requires a plain type, e.g. `impl Headers`",
            )
            .to_compile_error();
        }
    };

    // MAC-01 Phase 1 (design `docs/proposals/macro-v8-state.md` §4.2):
    // resolve `(state_ty, marker_ty)`.
    //  - `state_ty` is what the box stored in V8 internal field 0
    //    contains (`Box<StateTy>`). The macro emits casts as
    //    `*mut StateTy` / `*const StateTy`, the constructor returns
    //    `StateTy`, and per-method `&self` desugars against the impl
    //    receiver — which IS `StateTy` under Option B.
    //  - `marker_ty` drives JS-class identity: install/brand slots,
    //    callback names, the install fn's enclosing impl, the
    //    `set_class_name` literal, must-new/Symbol.toStringTag, and
    //    iterable companion install.
    //
    // Without `#[v8_state_marker]`: state == marker == receiver
    // (byte-identical to today's emission, locked by insta snapshots).
    // With `#[v8_state_marker(M)] impl S`: state = S, marker = M.
    let state_marker_path = extract_state_marker(&input.attrs);
    let (state_ty, marker_ty) =
        match resolve_state_and_marker(receiver_ty, state_marker_path.as_ref()) {
            Ok(pair) => pair,
            Err(ts) => return ts,
        };
    // Most existing call sites read `class_ty` as the JS-identity ident
    // (install slot / brand check / callback names) — that's now the
    // marker. The seam from parse → analyse is `analyze::analyze`,
    // which folds method classification + per-attribute extraction +
    // iterable codegen + all compile-time guards into the
    // `ClassConfig` parameter object that the emit phase consumes.
    match analyze::analyze(&input, state_ty, &marker_ty) {
        Ok((cfg, stripped_impl)) => emit::assemble_tokens(&cfg, &stripped_impl),
        Err(ts) => ts,
    }
}

fn extract_class_ident(ty: &Type) -> Option<&syn::Ident> {
    match ty {
        Type::Path(p) => p.path.get_ident(),
        _ => None,
    }
}


// ---------------------------------------------------------------------------
// Codegen snapshot tests (MAC-01 Phase 1)
// ---------------------------------------------------------------------------
//
// Lock the macro's emission against unintended drift. Per design §5.1
// / §6.4: the no-attribute path is required to be byte-identical to
// pre-Phase-1 emission (modulo the qualified Private-symbol name in row
// 16). The new `#[v8_state_marker]` path is also snapshotted so a
// future change can detect drift in either direction.
//
// We snapshot the prettyplease-formatted output of `expand_tokens` so
// the snapshot stays human-readable across rustc / quote tweaks. Bumps
// require `cargo insta accept` with reviewer audit (design §8 settled-
// question 9).
#[cfg(test)]
mod snapshots {
    use super::expand_tokens;
    use proc_macro2::TokenStream as TokenStream2;
    use quote::quote;

    /// Format the macro output through prettyplease so the snapshot
    /// stays diff-friendly across whitespace tweaks in `quote!`.
    fn format_expansion(out: TokenStream2) -> String {
        // Parse the emitted tokens back as a `syn::File` so prettyplease
        // can format them. The macro emits items at module scope.
        let parsed: syn::File = syn::parse2(out).expect("macro output parses as items");
        prettyplease::unparse(&parsed)
    }

    /// Insta inline snapshot for the no-attribute (control) shape — a
    /// `#[v8_class] impl Foo { ... }` with one constructor + one method
    /// + one getter + one setter. Locks the byte-identical-emission
    /// invariant that the no-attribute path must satisfy
    /// (design §5.1 over CloseEventState / AbortSignal / Blob).
    #[test]
    fn snapshot_class_basic() {
        let item = quote! {
            impl Foo {
                #[v8_constructor]
                fn new(start: u32) -> Foo {
                    Foo { value: start }
                }

                #[v8_method]
                fn touch(&mut self) -> u32 {
                    self.value += 1;
                    self.value
                }

                #[v8_getter]
                fn value(&self) -> u32 {
                    self.value
                }

                #[v8_setter]
                #[v8_name = "value"]
                fn set_value(&mut self, n: u32) {
                    self.value = n;
                }
            }
        };
        let out = expand_tokens(quote! {}, item);
        insta::assert_snapshot!("class_basic", format_expansion(out));
    }

    /// Insta inline snapshot for the new `#[v8_state_marker(Marker)]
    /// impl State` shape. The marker (`Marker`) drives JS-class
    /// identity; the receiver (`State`) drives the `Box<State>` payload
    /// and per-method receiver type.
    #[test]
    fn snapshot_class_with_state_marker() {
        let item = quote! {
            #[v8_state_marker(Marker)]
            impl State {
                #[v8_constructor]
                fn new(start: u32) -> Result<State, OpError> {
                    Ok(State { value: start })
                }

                #[v8_method]
                fn touch(&mut self) -> u32 {
                    self.value += 1;
                    self.value
                }

                #[v8_getter]
                fn value(&self) -> u32 {
                    self.value
                }
            }
        };
        let out = expand_tokens(quote! {}, item);
        insta::assert_snapshot!("class_with_state_marker", format_expansion(out));
    }

    /// Hard-error snapshot: marker == receiver. Per design §4.7 the
    /// macro emits a clear compile_error rather than silently treating
    /// it as a no-op (which would mask a typo'd marker name).
    #[test]
    fn snapshot_class_marker_equals_receiver_errors() {
        let item = quote! {
            #[v8_state_marker(Foo)]
            impl Foo {
                #[v8_constructor]
                fn new() -> Foo { Foo }
            }
        };
        let out = expand_tokens(quote! {}, item);
        // Compile-error tokens still parse as a valid syn::File (each
        // `compile_error!(...)` is an item-level macro invocation), so
        // prettyplease can format them.
        insta::assert_snapshot!(
            "class_marker_equals_receiver_errors",
            format_expansion(out)
        );
    }
}
