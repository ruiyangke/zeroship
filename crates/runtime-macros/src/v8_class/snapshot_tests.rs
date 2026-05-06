//! Codegen snapshot tests for `#[v8_state_marker]`.
//!
//! Lock the macro's emission against unintended drift. Per design §5.1
//! / §6.4: the no-attribute path is required to be byte-identical to
//! the old emission shape (modulo the qualified Private-symbol name in
//! row 16). The new `#[v8_state_marker]` path is also snapshotted so a
//! future change can detect drift in either direction.
//!
//! We snapshot the prettyplease-formatted output of `expand_tokens` so
//! the snapshot stays human-readable across rustc / quote tweaks. Bumps
//! require `cargo insta accept` with reviewer audit (design §8 settled-
//! question 9).
//!
//! Extracted out of `mod.rs` so the module head stays a thin
//! orchestrator. The snapshot files themselves live in
//! `crates/runtime-macros/src/v8_class/snapshots/` (paths unchanged
//! across the move).

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

/// Insta snapshot for the variadic-param shape. A
/// `Vec<v8::Local<v8::Value>>` trailing parameter captures all JS
/// args from its position onward — used by spec-shaped methods like
/// `AsyncLocalStorage.run(store, fn, ...args)` whose JS surface
/// takes an arbitrary trailing arg list. The codegen must:
///   - count preceding positional args (here: 1) for the start index;
///   - emit a single `(start..args.length()).map(args.get).collect()`
///     instead of one `args.get(idx)` per positional arg;
///   - bind the result as the user's parameter ident so the call site
///     dispatches naturally as `<State>::method(&self, target, rest)`.
///
/// Locks the variadic codegen against future drift — any change to
/// the slice shape, the index computation, or the bind site shows
/// up as a snapshot delta.
#[test]
fn snapshot_class_with_variadic_method() {
    let item = quote! {
        impl Spreader {
            #[v8_constructor]
            fn new() -> Spreader {
                Spreader
            }

            #[v8_method]
            fn invoke<'s>(
                &self,
                scope: &mut v8::PinScope<'s, '_>,
                target: v8::Local<v8::Value>,
                rest: Vec<v8::Local<v8::Value>>,
            ) -> v8::Local<'s, v8::Value> {
                let func: v8::Local<v8::Function> = target.try_into().unwrap();
                let undef = v8::undefined(scope).into();
                func.call(scope, undef, &rest).unwrap_or_else(|| v8::undefined(scope).into())
            }
        }
    };
    let out = expand_tokens(quote! {}, item);
    insta::assert_snapshot!("class_with_variadic_method", format_expansion(out));
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
