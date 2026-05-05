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

#[cfg(test)]
mod snapshot_tests;

pub(crate) use ast::{ClassMethod, ConstDecl, ConstKind, MethodKind};

use parse::resolve_state_and_marker;

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
    //
    // Wave 4: the impl-block-level extract is folded into a single-scan
    // walk (`parse_attrs`) inside `analyze::analyze`. Read it through
    // the new entry point so we get strict-error behaviour on malformed
    // shapes (closes F5 + H5).
    let parsed_class_attrs = match parse::parse_attrs(&input.attrs) {
        Ok(p) => p,
        Err(e) => return e.to_compile_error(),
    };
    let (state_ty, marker_ty) =
        match resolve_state_and_marker(receiver_ty, parsed_class_attrs.state_marker.as_ref()) {
            Ok(pair) => pair,
            Err(ts) => return ts,
        };
    // Most existing call sites read `class_ty` as the JS-identity ident
    // (install slot / brand check / callback names) — that's now the
    // marker. The seam from parse → analyse is `analyze::analyze`,
    // which folds method classification + per-attribute extraction +
    // iterable codegen + all compile-time guards into the
    // `ClassConfig` parameter object that the emit phase consumes.
    match analyze::analyze(&input, state_ty, &marker_ty, parsed_class_attrs) {
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
