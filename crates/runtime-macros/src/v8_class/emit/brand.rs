//! Brand-check helper codegen.
//!
//! Wave 3 commit 3 — extracted from `mod.rs`'s 195-line megaquote
//! (design `docs/proposals/runtime-macros-refactor.md` §4.1, F3).
//! Emits the per-class `__brand_check_<Class>` fn used by every
//! method/getter/setter callback prologue (via
//! `shared::recover_box`) before the unsafe internal-field deref.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};

use super::super::shared::class_config::ClassConfig;

/// Emit the `__brand_check_<Class>` helper fn — the WebIDL §3.7
/// brand-identity check that walks the receiver's prototype chain
/// looking for the cached `Foo.prototype`. Returns `true` if the
/// receiver IS a Foo (or a subclass via `#[v8_inherit]`); `false`
/// otherwise.
///
/// Walks at most 1024 prototype links — matches V8's internal
/// `Object::PrototypeChainLength` sanity bound. The cap is NOT a
/// cycle defence (ECMAScript §10.4.7.2 step 8 already rejects cycle
/// creation in `Object.setPrototypeOf`); it exists as belt-and-braces
/// against future proxy-driven prototype chains that might fake
/// infinite linear depth.
///
/// The cached prototype is populated lazily on first call, NOT in
/// `install` — eager `get_function(scope)` at install time would
/// freeze the FunctionTemplate's instance shape and silently no-op
/// any subsequent `prototype_template().set_accessor_property(...)`.
/// URL hand-installs `searchParams` on the prototype_template after
/// `URL::install` returns; we must not break that.
pub(super) fn gen_brand_check_helpers(cfg: &ClassConfig) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let install_slot_ty = format_ident!("__InstallSlot_{}", class_ty);
    let brand_slot_ty = format_ident!("__BrandSlot_{}", class_ty);
    // Wave 9 N1: brand-check ident from ClassConfig (computed once at
    // ClassConfig::new). This emit site DEFINES the fn — the cached
    // ident is what every consumer reads.
    let brand_check_fn = &cfg.brand_check_ident;

    quote! {
        /// Brand-check helper: walks the prototype chain of `this`
        /// looking for the cached `Foo.prototype`. Returns true on
        /// match (the receiver IS a Foo, or a subclass via
        /// `#[v8_inherit]`), false otherwise.
        ///
        /// Walks at most 1024 prototype links — matching V8's own
        /// internal `Object::PrototypeChainLength` sanity bound. The
        /// cap is NOT a cycle defence: ECMAScript §10.4.7.2 step 8
        /// already requires `Object.setPrototypeOf` to reject any
        /// assignment that would create a cycle, so user JS cannot
        /// construct one. The cap exists purely as a defence-in-depth
        /// belt-and-braces against an underlying V8 bug or future
        /// proxy-driven prototype chain that fakes infinite linear
        /// depth. Real WebIDL inheritance chains are 1-3 hops; pure
        /// prototypal chains rarely exceed 5; reaching the cap is a
        /// pathological case for which "false" is the conservative
        /// answer.
        ///
        /// The cost is dwarfed by the ~100ns V8 callback overhead —
        /// the brand check itself is O(depth) Local pointer
        /// comparisons.
        ///
        /// The cached prototype is populated lazily on first call —
        /// NOT in `install` — because eager `get_function(scope)` at
        /// install time would freeze the FunctionTemplate's instance
        /// shape and silently no-op any subsequent
        /// `prototype_template().set_accessor_property(...)` calls.
        /// Several classes (URL.searchParams, etc.) install accessors
        /// on the prototype_template AFTER `Self::install` returns; we
        /// must not break those.
        ///
        /// First-call cost (one-time per isolate): one `get_function`
        /// + one `.get(prototype)`. Steady state: an isolate-slot read
        /// (Rc-clone-shaped) plus the chain walk.
        ///
        /// Lifetimes are elided here on purpose. An explicit `<'s>`
        /// would tie the `Local<Object>` argument's lifetime to the
        /// `&mut PinScope` lifetime in an invariant way (mutable
        /// references are invariant over their type param), which
        /// then conflicts with `args.this()`'s callsite-derived
        /// lifetime. Elision lets each Local pick its own appropriate
        /// (and shorter) lifetime — the helper body never returns a
        /// `Local` so there's no need to relate them outside the
        /// function.
        #[doc(hidden)]
        #[allow(non_snake_case, dead_code)]
        fn #brand_check_fn(
            scope: &mut v8::PinScope,
            obj: v8::Local<v8::Object>,
        ) -> bool {
            // Resolve the cached prototype, lazily populating the
            // brand slot on first call. We can't hold the slot's
            // borrow across `set_slot` (mutable borrow) so we drop
            // it (via `.cloned()` of the Global) before any `set_slot`
            // call.
            let cached_global: v8::Global<v8::Object> =
                if let Some(slot) = scope.get_slot::<#brand_slot_ty>() {
                    slot.0.clone()
                } else {
                    // Lazy fetch from the install slot. If that slot
                    // is missing too, the class wasn't installed in
                    // this isolate — fall through to false.
                    let tmpl_global = match scope.get_slot::<#install_slot_ty>() {
                        Some(s) => s.0.clone(),
                        None => return false,
                    };
                    let tmpl_local = v8::Local::new(scope, &tmpl_global);
                    let func = match tmpl_local.get_function(scope) {
                        Some(f) => f,
                        None => return false,
                    };
                    let proto_key = match v8::String::new(scope, "prototype") {
                        Some(s) => s,
                        None => return false,
                    };
                    let proto_v = match func.get(scope, proto_key.into()) {
                        Some(v) => v,
                        None => return false,
                    };
                    let proto: v8::Local<v8::Object> = match proto_v.try_into() {
                        Ok(o) => o,
                        Err(_) => return false,
                    };
                    let g = v8::Global::new(scope, proto);
                    let g_clone = g.clone();
                    scope.set_slot(#brand_slot_ty(g));
                    g_clone
                };
            let expected_proto: v8::Local<v8::Object> = v8::Local::new(scope, &cached_global);
            // Walk the [[Prototype]] chain. Each `get_prototype` call
            // can return null (chain root) or a Value (potentially an
            // Object). The 1024 cap matches V8's internal sanity
            // bound; cycle creation is already blocked by V8 (see
            // doc-comment).
            let mut current: v8::Local<v8::Value> = match obj.get_prototype(scope) {
                Some(v) => v,
                None => return false,
            };
            for _ in 0..1024 {
                if current.is_null_or_undefined() {
                    return false;
                }
                let cur_obj: v8::Local<v8::Object> = match current.try_into() {
                    Ok(o) => o,
                    Err(_) => return false,
                };
                // V8 Locals compare by handle equality, which matches
                // pointer identity for Persistent-derived Locals. The
                // cached prototype is the exact Object the install
                // captured at first-install time; any genuine `new
                // Foo()` (or instance of a class inheriting Foo) has
                // that Object on its chain.
                if cur_obj == expected_proto {
                    return true;
                }
                current = match cur_obj.get_prototype(scope) {
                    Some(v) => v,
                    None => return false,
                };
            }
            false
        }
    }
}
