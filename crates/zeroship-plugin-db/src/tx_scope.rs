//! Async-scoped "am I inside a transaction callback" marker.
//!
//! ## Why this exists
//!
//! `env.db.transaction(fn)` has to decide, at call time, whether it is
//! opening a **new** transaction (`BEGIN`) or **nesting** inside one that
//! is already open (`SAVEPOINT`). Until 2026-08-10 that decision read
//! [`crate::context::ThreadDbContext::has_tx_for`] — "does this app
//! currently have a transaction open on this isolate?".
//!
//! That is a *temporal* test standing in for a *structural* one, and the
//! two come apart the moment two transactions for one app overlap in
//! time. They do overlap: a worker OS thread multiplexes many requests
//! over one isolate and hands control to another dispatch at every
//! `.await`, and `pnpm dev` is a single isolate by construction
//! (`zeroship serve --workers=1`). Measured on both tiers by
//! `tests/e2e_dev_vs_deployed_db.sh`:
//!
//! ```text
//! request A   db.transaction(async tx => { insert; await …; throw })
//! request B                     db.transaction(async tx => { insert })  // resolves "committed"
//! ```
//!
//! B read `has_tx_for == true`, opened a SAVEPOINT on **A's** connection,
//! reported success — and A's `ROLLBACK` then destroyed B's row. Two
//! unrelated end users' work, entangled, with the loser told it had
//! committed.
//!
//! ## The discriminator
//!
//! Nesting is a property of the **call's async context**, not of the
//! app's wall-clock state: a `transaction()` call nests exactly when it
//! runs inside the enclosing callback's continuation chain. V8 v147 has
//! the primitive for that — `Isolate::SetContinuationPreservedEmbedderData`,
//! the same slot `node:async_hooks`' `AsyncLocalStorage` uses (see
//! `crates/runtime/src/node/async_hooks/als.rs`). The slot holds a JS
//! `Map`, and V8 carries it across every async hop, restoring it when a
//! promise reaction runs.
//!
//! So: [`enter`] plants `app_id` under our own registry Symbol for the
//! duration of the synchronous `user_fn.call(...)` frame, every
//! continuation that branches off inside the callback inherits it, and
//! [`current_tx_app`] reads it back. A concurrent dispatch's continuations
//! branched off *before* that frame and therefore see nothing — which is
//! the whole point.
//!
//! ## Interop with `AsyncLocalStorage`
//!
//! The slot is shared with ALS, so this module obeys the same convention:
//! the value is a `v8::Map`, entries are keyed by Symbol, and [`enter`]
//! CLONES the map before adding its entry (mutating in place would leak
//! the entry into sibling async branches that captured the map by
//! reference). Our key comes from the global symbol registry rather than
//! a per-instance Symbol, because there is exactly one transaction scope
//! per isolate and it must be readable from a different call site than
//! the one that wrote it.

/// Global-registry key for the transaction-scope entry in the shared
/// continuation-preserved `Map`. Namespaced so it cannot collide with a
/// creator's own `Symbol.for(...)` key.
const SCOPE_SYMBOL_KEY: &str = "zeroship.plugin-db.txScope";

fn scope_symbol<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Symbol>> {
    let key = v8::String::new(scope, SCOPE_SYMBOL_KEY)?;
    Some(v8::Symbol::for_key(scope, key))
}

/// Read the continuation-preserved context `Map`, if one is installed.
fn read_context_map<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Map>> {
    let val = scope.get_continuation_preserved_embedder_data();
    if val.is_undefined() || val.is_null() {
        return None;
    }
    v8::Local::<v8::Map>::try_from(val).ok()
}

/// Clone a JS `Map` by walking its flattened `[k0, v0, k1, v1, …]` array.
/// Mirrors `als::clone_map`; duplicated rather than shared because that
/// helper is `pub(crate)` inside the runtime crate.
fn clone_context_map<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    src: v8::Local<v8::Map>,
) -> v8::Local<'s, v8::Map> {
    let dst = v8::Map::new(scope);
    let arr = src.as_array(scope);
    let len = arr.length();
    let mut i: u32 = 0;
    while i + 1 < len {
        let (Some(k), Some(v)) = (arr.get_index(scope, i), arr.get_index(scope, i + 1)) else {
            return dst;
        };
        dst.set(scope, k, v);
        i += 2;
    }
    dst
}

/// The `app_id` whose `transaction()` callback the CURRENT async context
/// is executing inside, or `None` at top level.
///
/// `None` is the answer for a dispatch that merely *overlaps* another
/// app-level transaction in time, which is exactly the case the old
/// `has_tx_for` test got wrong.
pub(crate) fn current_tx_app(scope: &mut v8::PinScope<'_, '_>) -> Option<String> {
    let map = read_context_map(scope)?;
    let sym = scope_symbol(scope)?;
    let value = map.get(scope, sym.into())?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    Some(value.to_rust_string_lossy(scope))
}

/// Plant `app_id` as the current transaction scope. Returns the previous
/// slot value, which the caller MUST hand to [`leave`] on every exit path
/// so a sibling branch is not left inside a scope it never entered.
pub(crate) fn enter(
    scope: &mut v8::PinScope<'_, '_>,
    app_id: &str,
) -> Option<v8::Global<v8::Value>> {
    let sym = scope_symbol(scope)?;
    let value = v8::String::new(scope, app_id)?;
    let prev = scope.get_continuation_preserved_embedder_data();
    let prev_global = v8::Global::new(scope, prev);

    // Clone-then-extend: see the module docs. A sibling async branch that
    // captured the current map keeps seeing the map it captured.
    let next = match read_context_map(scope) {
        Some(m) => clone_context_map(scope, m),
        None => v8::Map::new(scope),
    };
    next.set(scope, sym.into(), value.into());
    scope.set_continuation_preserved_embedder_data(next.into());
    Some(prev_global)
}

/// Restore the slot value [`enter`] displaced.
pub(crate) fn leave(scope: &mut v8::PinScope<'_, '_>, prev: Option<v8::Global<v8::Value>>) {
    let Some(prev) = prev else { return };
    let local = v8::Local::new(scope, prev);
    scope.set_continuation_preserved_embedder_data(local);
}

/// Read the transaction frame out of V8 and freeze a [`TxRoute`] from it.
///
/// This is the whole of the V8 half of routing, and it lives here because this
/// module is where the context map is. [`TxRoute::capture`] still owns the
/// comparison that decides the route - it takes the observation, not the scope -
/// so the SEC-1 property is stated once, in the type that carries it, and
/// `tx_route.rs` names no `v8::` type.
///
/// Call this from a dispatch prologue while `scope` is live. The answer is only
/// correct at the dispatch boundary: the runtime's continuation slot rotates on
/// the next pump turn.
pub(crate) fn capture_route(scope: &mut v8::PinScope<'_, '_>, app_id: &str) -> crate::tx_route::TxRoute {
    crate::tx_route::TxRoute::capture(current_tx_app(scope).as_deref(), app_id)
}
