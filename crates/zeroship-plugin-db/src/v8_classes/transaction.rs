//! `mint_tx_view` — the collections-only object handed to a
//! `Db.transaction(fn)` callback.
//!
//! ## Design
//!
//! The `Transaction` `#[v8_class]` (with its `commit`/`rollback`/
//! `collection` methods and GC-driven auto-rollback finalizer) is
//! **gone**. Transaction orchestration moved entirely into Rust
//! ([`crate::transaction`]): begin/commit/rollback/
//! savepoint are driven by the native `Db.transaction(fn)` orchestrator,
//! never by JS-reachable methods. There is no `env.db.beginTransaction`
//! and no `tx.commit()` / `tx.rollback()` anywhere in the object graph —
//! creators abort by throwing inside the callback and commit by resolving.
//!
//! What remains here is [`mint_tx_view`]: the object the orchestrator
//! passes to the creator callback. It is a plain `v8::Object` whose
//! properties are one [`Collection`](super::collection::Collection) per
//! cached-schema collection — **collections as props, no methods**
//! (Q-P9-C). `tx.posts.find(...)` works because each property is a real
//! `Collection`; every CRUD method on it routes through the open
//! transaction connection automatically, since
//! [`crate::exec::run_sql`] consults
//! `ThreadDbContext::tx_conns` whenever it is set (the
//! orchestrator sets it for the duration of the transaction).
//!
//! ## Why collections-as-props (not a `Transaction` instance)
//!
//! A `tx` object that exposed `commit` / `rollback` / `collection` would
//! re-introduce a JS-reachable transaction primitive — exactly the
//! capability surface this redesign set out to remove. By minting a bare object with
//! only collection properties, there is no method for creator (or
//! escaped) code to call: the transaction lifecycle is owned by Rust end
//! to end. The bootstrap layer wraps each native collection prop in a
//! `Result`→throw shim before the creator sees it (so the callback sees
//! the throwing contract), but the *shape* — collections only — is fixed
//! here.

#![allow(unsafe_code)]

use zeroship_runtime::state::OpError;

use zeroship_data_core::binding::DbBinding;
use crate::v8_classes::collection::mint_collection;

/// Mint the collections-only `tx` view for a `Db.transaction(fn)`
/// callback.
///
/// Builds a fresh `v8::Object` and sets one
/// [`Collection`](super::collection::Collection) property per collection
/// the per-thread schema cache knows about for this binding (the same set
/// native runtime boot publishes). Each minted `Collection` is an
/// ordinary v8_class instance — identical to what `db.collection(name)`
/// returns — so its CRUD methods route through the active transaction
/// connection via the `tx_conn` slot the orchestrator set before calling
/// the creator callback.
///
/// No `commit` / `rollback` / `collection` / `transaction` / `live`
/// method is set on the view: the only members are collections. Manual
/// abort = throw inside the callback; commit is implicit on resolve.
///
/// When the descriptor store holds no entry for this binding, as with a raw-JS
/// schema-less deploy, the view is an empty object. A transaction with no
/// declared collections has nothing to address through `tx.<name>`; the
/// commit/rollback envelope still applies.
pub(crate) fn mint_tx_view<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: &DbBinding,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let view = v8::Object::new(scope);

    // One tx-bound Collection per cached-schema collection. The list
    // mirrors `Db::collection(name)`'s minting; the binding to the open
    // tx is implicit (the `tx_conn` slot is set), so no per-object tx id
    // is threaded.
    let collections: Vec<String> = crate::descriptor::declared_collections(binding)
        .into_iter()
        .map(|(name, _schema)| name)
        .collect();

    for name in collections {
        let col = mint_collection(scope, name.clone(), binding.clone())?;
        let key = v8::String::new(scope, &name)
            .ok_or_else(|| OpError::type_error("tx-view: collection name allocation failed"))?;
        view.set(scope, key.into(), col.into());
    }

    Ok(view)
}

#[cfg(test)]
mod tests {
    //! Shape guards for the `Db.transaction(fn)` callback argument.
    //!
    //! The proposal (Q-P9-C, §4.4) fixes the tx-view as **collections
    //! only** — no `commit` / `rollback` / `collection` / `transaction` /
    //! `live` method. These tests mint a view directly (no DB needed —
    //! the per-thread schema cache is empty in this test context, so the
    //! view is a bare object) and assert no tx-lifecycle method leaked
    //! onto it. If a future change re-introduces a `commit`/`rollback`
    //! method on the view, these fail.
    #![allow(unsafe_code)]

    use zeroship_runtime::init_v8;

    use zeroship_data_core::binding::DbBinding;

    fn assert_absent(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>, name: &str) {
        let key = v8::String::new(scope, name).unwrap();
        let v = obj.get(scope, key.into()).unwrap();
        assert!(
            v.is_undefined(),
            "tx-view must NOT expose `{name}` — it is collections-only \
             (no JS-reachable transaction primitive); got a defined value"
        );
    }

    #[test]
    fn tx_view_has_no_commit_or_rollback_methods() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let binding = DbBinding::cold_start("test_app");
        let view = super::mint_tx_view(scope, &binding).expect("mint_tx_view");

        // None of the legacy `Transaction` methods, nor `transaction` /
        // `live`, may appear on the view.
        for forbidden in [
            "commit",
            "rollback",
            "collection",
            "transaction",
            "live",
            "beginTransaction",
        ] {
            assert_absent(scope, view, forbidden);
        }

        // It is a plain object (its [[Prototype]] is Object.prototype,
        // not some Transaction.prototype carrying methods). Confirm the
        // prototype chain has no `commit`.
        let key = v8::String::new(scope, "commit").unwrap();
        // `get` walks the prototype chain; a plain object's chain ends at
        // Object.prototype which has no `commit`.
        let v = view.get(scope, key.into()).unwrap();
        assert!(
            v.is_undefined(),
            "commit must be absent up the whole prototype chain"
        );
    }

    #[test]
    fn tx_view_is_empty_without_runtime_descriptor() {
        // A schema-less app has no collections in the thread context, so the view has
        // no own enumerable properties. This is the raw-JS-deploy path.
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let binding = DbBinding::cold_start("test_app");
        let view = super::mint_tx_view(scope, &binding).expect("mint_tx_view");
        let names = view
            .get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
            .unwrap();
        assert_eq!(
            names.length(),
            0,
            "tx-view for a schema-less app must be empty"
        );
    }
}
