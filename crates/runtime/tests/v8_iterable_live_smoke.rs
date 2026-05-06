//! Smoke tests for `#[v8_iterable(key = K, value = V, mode = live)]`.
//!
//! Background: the default `#[v8_iterable]` derive snapshots
//! `value_pairs()` once at factory-call time and walks the snapshot.
//! WebIDL §3.7.10.2 actually requires LIVE iteration — each `next()`
//! re-reads the parent's state so mutations between yields are
//! observable. `mode = live` opts the derive into spec-compliant
//! semantics.
//!
//! Implementation contract (per the derive's emit):
//!   - The iterator stashes a `Global<Object>` reference to the parent
//!     plus a `usize` cursor + `i32` kind.
//!   - On each `next()`, the iterator re-localises the parent, recovers
//!     the boxed instance, re-calls `value_pairs()`, indexes at the
//!     cursor, and advances.
//!   - If the parent state shrinks below the cursor, `next()` yields
//!     `{ value: undefined, done: true }`. The cursor doesn't reset.
//!   - If the parent state grows, the cursor walks the new entries —
//!     that's the spec behaviour (mutations after the cursor are
//!     visible).
//!
//! Coverage:
//!   - Live observation of inserts during iteration.
//!   - Live observation of deletes during iteration (cursor stays put).
//!   - `forEach` on a live iterable observes mutations made by its own
//!     callback.
//!   - Snapshot mode (default + explicit `mode = snapshot`) keeps
//!     ignoring mutations — back-compat assertion.
#![allow(unsafe_code)]

use std::cell::RefCell;

use zeroship_runtime::byte_string::ByteString;
use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_iterable, v8_method};

// ---------------------------------------------------------------------------
// Test harness — local copy.
// ---------------------------------------------------------------------------

fn run_in_v8<F, R>(
    install: impl FnOnce(&mut v8::PinScope, v8::Local<v8::Object>),
    src: &str,
    f: F,
) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);
    install(scope, global);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

fn install_class<'s, T>(
    install_fn: fn(&mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::FunctionTemplate>,
    name: &str,
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let _ = std::marker::PhantomData::<T>;
    let tmpl = install_fn(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, name).unwrap();
    global.set(scope, key.into(), class_fn.into());
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// Live-mode test class — `Bag` holds a RefCell<Vec<(ByteString, ByteString)>>
// with append() and remove() methods exposed to JS.
// ---------------------------------------------------------------------------

mod live {
    use super::*;

    pub struct Bag {
        pub entries: RefCell<Vec<(ByteString, ByteString)>>,
    }

    #[v8_class]
    #[v8_iterable(key = ByteString, value = ByteString, mode = live)]
    impl Bag {
        #[v8_constructor]
        fn new() -> Bag {
            Bag {
                entries: RefCell::new(Vec::new()),
            }
        }

        #[v8_method]
        fn append(&self, k: ByteString, v: ByteString) {
            self.entries.borrow_mut().push((k, v));
        }

        /// Remove the first entry whose key equals `k`. Used to test
        /// the "delete past the cursor" branch.
        #[v8_method]
        fn remove(&self, k: ByteString) {
            let mut e = self.entries.borrow_mut();
            if let Some(idx) = e.iter().position(|(ek, _)| ek.as_slice() == k.as_slice()) {
                e.remove(idx);
            }
        }

        // Required by `#[v8_iterable]` — clones the current state on
        // each call. In live mode, this is invoked per `next()` rather
        // than once at factory-call time.
        fn value_pairs(&self) -> Vec<(ByteString, ByteString)> {
            self.entries.borrow().clone()
        }
    }
}

fn install_live(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    install_class::<live::Bag>(live::Bag::install, "Bag", scope, global);
}

// ---------------------------------------------------------------------------
// Insert during iteration: the new entry is observed.
// ---------------------------------------------------------------------------

#[test]
fn insert_during_iteration_is_observed() {
    let s = run_in_v8(
        install_live,
        r#"
        const b = new Bag();
        b.append("x", "1");
        b.append("y", "2");
        const it = b.entries();
        const r1 = it.next();           // ["x", "1"]
        b.append("z", "3");             // mutate parent — live mode sees it
        const r2 = it.next();           // ["y", "2"]
        const r3 = it.next();           // ["z", "3"] — observed!
        const r4 = it.next();           // done
        JSON.stringify({
            r1: { v: r1.value, d: r1.done },
            r2: { v: r2.value, d: r2.done },
            r3: { v: r3.value, d: r3.done },
            r4: { v: r4.value, d: r4.done },
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"r1":{"v":["x","1"],"d":false},"r2":{"v":["y","2"],"d":false},"r3":{"v":["z","3"],"d":false},"r4":{"d":true}}"#
    );
}

// ---------------------------------------------------------------------------
// Delete past the cursor: cursor stays at its index, yields what's
// now at that slot.
// ---------------------------------------------------------------------------

#[test]
fn delete_past_cursor_yields_current_slot() {
    let s = run_in_v8(
        install_live,
        r#"
        const b = new Bag();
        b.append("a", "1");
        b.append("b", "2");
        b.append("c", "3");
        b.append("d", "4");
        const it = b.entries();
        const r1 = it.next();             // ["a","1"], cursor → 1
        const r2 = it.next();             // ["b","2"], cursor → 2
        b.remove("c");                     // entries now: a, b, d at indices 0, 1, 2
        const r3 = it.next();             // cursor 2 → ["d","4"] (was c, now d slid in)
        const r4 = it.next();             // done
        `${r1.value.join("=")},${r2.value.join("=")},${r3.value.join("=")},${r4.done}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "a=1,b=2,d=4,true");
}

// ---------------------------------------------------------------------------
// Shrink below cursor: returns done.
// ---------------------------------------------------------------------------

#[test]
fn shrink_below_cursor_yields_done() {
    let s = run_in_v8(
        install_live,
        r#"
        const b = new Bag();
        b.append("a", "1");
        b.append("b", "2");
        const it = b.entries();
        const r1 = it.next();             // ["a","1"], cursor → 1
        b.remove("a"); b.remove("b");      // entries empty, length 0 < cursor 1
        const r2 = it.next();             // done
        `${r1.value.join("=")},${r2.done}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "a=1,true");
}

// ---------------------------------------------------------------------------
// forEach in live mode iterates the post-mutation state when the
// callback mutates.
// ---------------------------------------------------------------------------

#[test]
fn for_each_observes_callback_mutations() {
    let s = run_in_v8(
        install_live,
        r#"
        const b = new Bag();
        b.append("a", "1");
        const seen = [];
        b.forEach((v, k) => {
            seen.push(`${k}=${v}`);
            // Append a new entry on the FIRST callback only. Live forEach
            // re-reads value_pairs() between callbacks, so the new entry
            // shows up in the same forEach pass.
            if (seen.length === 1) {
                b.append("b", "2");
            }
        });
        seen.join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "a=1,b=2");
}

// ---------------------------------------------------------------------------
// keys() / values() / entries() all observe the live state.
// ---------------------------------------------------------------------------

#[test]
fn keys_iterator_is_live() {
    let s = run_in_v8(
        install_live,
        r#"
        const b = new Bag();
        b.append("a", "1");
        const it = b.keys();
        const r1 = it.next().value;       // "a"
        b.append("b", "2");
        const r2 = it.next().value;       // "b"
        const r3 = it.next();             // done
        `${r1},${r2},${r3.done}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "a,b,true");
}

#[test]
fn values_iterator_is_live() {
    let s = run_in_v8(
        install_live,
        r#"
        const b = new Bag();
        b.append("a", "1");
        const it = b.values();
        const r1 = it.next().value;       // "1"
        b.append("b", "2");
        const r2 = it.next().value;       // "2"
        `${r1},${r2}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "1,2");
}

// ---------------------------------------------------------------------------
// Brand check on the iterator's next(): calling next on a foreign
// receiver throws (we don't expose the iterator class, but the native
// callback still has to defend).
// ---------------------------------------------------------------------------

#[test]
fn iterator_next_brand_checks() {
    let s = run_in_v8(
        install_live,
        r#"
        const b = new Bag();
        const it = b.entries();
        const Next = Object.getPrototypeOf(it).next;
        let msg = "no-throw";
        try { Next.call({}); }
        catch (e) { msg = "type-error:" + e.name; }
        msg;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "type-error:TypeError");
}

// ---------------------------------------------------------------------------
// Live brand check at factory time (still throws on non-Bag receiver).
// ---------------------------------------------------------------------------

#[test]
fn live_factory_brand_checks() {
    let s = run_in_v8(
        install_live,
        r#"
        let msg = "no-throw";
        try { Bag.prototype.entries.call({}); }
        catch (e) { msg = "type-error:" + e.name; }
        msg;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "type-error:TypeError");
}

// ---------------------------------------------------------------------------
// Explicit `mode = snapshot` works (back-compat / parity with omitted).
// ---------------------------------------------------------------------------

mod explicit_snapshot {
    use super::*;

    pub struct Snap {
        pub entries: RefCell<Vec<(ByteString, ByteString)>>,
    }

    #[v8_class]
    #[v8_iterable(key = ByteString, value = ByteString, mode = snapshot)]
    impl Snap {
        #[v8_constructor]
        fn new() -> Snap {
            Snap {
                entries: RefCell::new(Vec::new()),
            }
        }

        #[v8_method]
        fn append(&self, k: ByteString, v: ByteString) {
            self.entries.borrow_mut().push((k, v));
        }

        fn value_pairs(&self) -> Vec<(ByteString, ByteString)> {
            self.entries.borrow().clone()
        }
    }
}

#[test]
fn explicit_snapshot_mode_ignores_post_factory_mutations() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<explicit_snapshot::Snap>(
                explicit_snapshot::Snap::install,
                "Snap",
                scope,
                global,
            );
        },
        r#"
        const s = new Snap();
        s.append("a", "1");
        const it = s.entries();
        s.append("b", "2");                // post-factory mutation
        const r1 = it.next();              // ["a","1"]
        const r2 = it.next();              // done — snapshot didn't see "b"
        `${r1.value.join("=")},${r2.done}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "a=1,true");
}

// ---------------------------------------------------------------------------
// `&mut self` value_pairs — Headers' lazy sort cache shape.
// Each call to value_pairs may rebuild a cache; live mode means we need
// `&mut self` recovery on each next() and forEach iteration.
// ---------------------------------------------------------------------------

mod mut_live {
    use super::*;

    /// Mimics Headers' shape: a backing list + a lazy "sorted" cache
    /// that's rebuilt the first time after each mutation. The cache is
    /// invalidated by `append` and rebuilt on the first read after.
    pub struct Bag {
        pub entries: Vec<(ByteString, ByteString)>,
        pub cache: Option<Vec<(ByteString, ByteString)>>,
        /// Rebuild counter — exposed via JS so the test can prove the
        /// macro's `&mut self` recovery actually populated the cache.
        pub rebuilds: u32,
    }

    #[v8_class]
    #[v8_iterable(key = ByteString, value = ByteString, mode = live)]
    impl Bag {
        #[v8_constructor]
        fn new() -> Bag {
            Bag {
                entries: Vec::new(),
                cache: None,
                rebuilds: 0,
            }
        }

        #[v8_method]
        fn append(&mut self, k: ByteString, v: ByteString) {
            self.entries.push((k, v));
            // Mutation invalidates the lazy cache — exactly the
            // Headers pattern.
            self.cache = None;
        }

        /// Read-only counter exposing the rebuild hits to the test.
        #[v8_method]
        fn rebuilds(&self) -> u32 {
            self.rebuilds
        }

        /// `&mut self` shape — each call lazily rebuilds the cache and
        /// returns a fresh clone. The macro must recover via `*mut Self`
        /// + `&mut *ptr` so this method is callable.
        fn value_pairs(&mut self) -> Vec<(ByteString, ByteString)> {
            if self.cache.is_none() {
                self.cache = Some(self.entries.clone());
                self.rebuilds += 1;
            }
            self.cache.as_ref().unwrap().clone()
        }
    }
}

#[test]
fn mut_self_value_pairs_lazy_cache_rebuilds_after_mutation() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<mut_live::Bag>(mut_live::Bag::install, "Bag", scope, global);
        },
        r#"
        const b = new Bag();
        b.append("a", "1");
        b.append("b", "2");
        const it = b.entries();
        const r1 = it.next();              // builds cache, rebuilds = 1
        const r2 = it.next();              // hits cache, rebuilds still 1
        b.append("c", "3");                // invalidates cache
        const r3 = it.next();              // rebuilds = 2 (live mode)
        `${r1.value.join("=")},${r2.value.join("=")},${r3.value.join("=")},${b.rebuilds()}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    // Each next() call invokes value_pairs() afresh; the FIRST and
    // THIRD calls hit a cold cache (1 + 1 = 2 rebuilds), the SECOND
    // hits the warm cache. After two appends + two next()s the cache
    // is valid; after the third append the cache is invalidated and
    // the third next() rebuilds it.
    assert_eq!(s, "a=1,b=2,c=3,2");
}

#[test]
fn mut_self_value_pairs_for_each_observes_mutations() {
    // Reuses the same `Bag` from `mut_live`. forEach must re-call
    // value_pairs(&mut self) per iteration.
    let s = run_in_v8(
        |scope, global| {
            install_class::<mut_live::Bag>(mut_live::Bag::install, "Bag", scope, global);
        },
        r#"
        const b = new Bag();
        b.append("a", "1");
        const seen = [];
        b.forEach((v, k) => {
            seen.push(`${k}=${v}`);
            if (seen.length === 1) b.append("b", "2"); // live mode picks up
        });
        seen.join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "a=1,b=2");
}

// ---------------------------------------------------------------------------
// `&self, scope` value_pairs (snapshot mode) — value_pairs reads the
// scope to fetch state from a slot. Mirrors a class that needs the
// scope to materialise its pair list (e.g. for slot-based fixtures).
// ---------------------------------------------------------------------------

mod scope_snapshot {
    use super::*;

    /// Marker stored on the scope slot — supplies the pairs.
    pub struct Source {
        pub pairs: Vec<(ByteString, ByteString)>,
    }

    pub struct Bag;

    #[v8_class]
    #[v8_iterable(key = ByteString, value = ByteString, mode = snapshot)]
    impl Bag {
        #[v8_constructor]
        fn new() -> Bag {
            Bag
        }

        /// `&self, scope` — proves the macro passes the outer scope
        /// through. Without scope we couldn't reach `Source`.
        fn value_pairs(
            &self,
            scope: &mut v8::PinScope,
        ) -> Vec<(ByteString, ByteString)> {
            scope
                .get_slot::<Source>()
                .map(|s| s.pairs.clone())
                .unwrap_or_default()
        }
    }
}

#[test]
fn ref_self_value_pairs_with_scope_is_passed_through() {
    let s = run_in_v8(
        |scope, global| {
            scope.set_slot(scope_snapshot::Source {
                pairs: vec![
                    (
                        ByteString::from_bytes(b"x".to_vec()),
                        ByteString::from_bytes(b"1".to_vec()),
                    ),
                    (
                        ByteString::from_bytes(b"y".to_vec()),
                        ByteString::from_bytes(b"2".to_vec()),
                    ),
                ],
            });
            install_class::<scope_snapshot::Bag>(
                scope_snapshot::Bag::install,
                "Bag",
                scope,
                global,
            );
        },
        r#"
        const b = new Bag();
        const out = [];
        for (const [k, v] of b) out.push(`${k}=${v}`);
        out.join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "x=1,y=2");
}

// ---------------------------------------------------------------------------
// `&mut self, scope` value_pairs (live mode) — URLSearchParams shape:
// `value_pairs` calls a scope-taking helper (sync_from_parent) and then
// returns the pair list. Both `&mut self` AND `&mut PinScope` must be
// recognized by the macro.
// ---------------------------------------------------------------------------

mod mut_scope_live {
    use super::*;

    /// Marker stashed on the scope slot — value_pairs pulls a fresh
    /// "remote view" from here on each call to mimic
    /// `sync_from_parent(scope)`.
    pub struct Remote {
        pub pairs: RefCell<Vec<(ByteString, ByteString)>>,
    }

    pub struct Bag {
        /// Local mirror — `value_pairs` syncs into this from the
        /// scope-stored remote, then returns it. `&mut self` is
        /// required because `entries` is mutated during sync.
        pub entries: Vec<(ByteString, ByteString)>,
        /// Sync counter — exposed for verification.
        pub syncs: u32,
    }

    #[v8_class]
    #[v8_iterable(key = ByteString, value = ByteString, mode = live)]
    impl Bag {
        #[v8_constructor]
        fn new() -> Bag {
            Bag {
                entries: Vec::new(),
                syncs: 0,
            }
        }

        #[v8_method]
        fn syncs(&self) -> u32 {
            self.syncs
        }

        /// `&mut self, scope` — full URLSearchParams shape.
        fn value_pairs(
            &mut self,
            scope: &mut v8::PinScope,
        ) -> Vec<(ByteString, ByteString)> {
            // sync_from_parent: pull the remote pairs into the local
            // mirror via the scope slot. This is the line that
            // requires both `&mut self` AND `&mut PinScope`.
            if let Some(remote) = scope.get_slot::<Remote>() {
                self.entries = remote.pairs.borrow().clone();
            }
            self.syncs += 1;
            self.entries.clone()
        }
    }
}

#[test]
fn mut_self_with_scope_value_pairs_syncs_per_next() {
    let s = run_in_v8(
        |scope, global| {
            scope.set_slot(mut_scope_live::Remote {
                pairs: RefCell::new(vec![(
                    ByteString::from_bytes(b"k".to_vec()),
                    ByteString::from_bytes(b"v".to_vec()),
                )]),
            });
            install_class::<mut_scope_live::Bag>(
                mut_scope_live::Bag::install,
                "Bag",
                scope,
                global,
            );
        },
        r#"
        const b = new Bag();
        const it = b.entries();
        const r1 = it.next();              // syncs the single pair
        const r2 = it.next();              // done
        // syncs() returns 2: one per next() call, both observed.
        `${r1.value.join("=")},${r2.done},${b.syncs()}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "k=v,true,2");
}

// ---------------------------------------------------------------------------
// `value_marshal = some_fn` — arbitrary V types via user-supplied
// marshal function. Mirrors FormData's `(USVString or File)` union.
// ---------------------------------------------------------------------------

mod custom_marshal {
    use super::*;

    /// A union-shaped value: a string OR a number, surfaced as either
    /// a v8 string or v8 number on yield.
    #[derive(Clone)]
    pub enum Either {
        Str(String),
        Num(f64),
    }

    /// Marshal hook — `value_marshal = either_to_v8` on the iterable
    /// attribute. Macro emits `let __v_v = either_to_v8(scope, &__v);`
    /// per yield. Signature is fixed: `fn(&mut PinScope, &V) ->
    /// Local<Value>`.
    pub fn either_to_v8<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        v: &Either,
    ) -> v8::Local<'s, v8::Value> {
        match v {
            Either::Str(s) => v8::String::new(scope, s).unwrap().into(),
            Either::Num(n) => v8::Number::new(scope, *n).into(),
        }
    }

    pub struct Bag {
        pub entries: RefCell<Vec<(ByteString, Either)>>,
    }

    #[v8_class]
    #[v8_iterable(
        key = ByteString,
        value = Either,
        mode = live,
        value_marshal = either_to_v8
    )]
    impl Bag {
        #[v8_constructor]
        fn new() -> Bag {
            Bag {
                entries: RefCell::new(Vec::new()),
            }
        }

        #[v8_method]
        fn append_str(&self, k: ByteString, v: ByteString) {
            self.entries.borrow_mut().push((
                k,
                Either::Str(String::from_utf8_lossy(v.as_slice()).into_owned()),
            ));
        }

        #[v8_method]
        fn append_num(&self, k: ByteString, v: f64) {
            self.entries.borrow_mut().push((k, Either::Num(v)));
        }

        fn value_pairs(&self) -> Vec<(ByteString, Either)> {
            self.entries.borrow().clone()
        }
    }
}

#[test]
fn value_marshal_fn_emits_arbitrary_v8_values() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<custom_marshal::Bag>(
                custom_marshal::Bag::install,
                "Bag",
                scope,
                global,
            );
        },
        r#"
        const b = new Bag();
        b.append_str("k1", "hello");
        b.append_num("k2", 42);
        const out = [];
        for (const [k, v] of b) {
            out.push(`${k}:${typeof v}:${v}`);
        }
        out.join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    // The marshal hook produces a V8 string for the first entry and
    // a V8 number for the second — proving `value_marshal` ran on
    // each yield.
    assert_eq!(s, "k1:string:hello,k2:number:42");
}

#[test]
fn mut_self_with_scope_value_pairs_observes_remote_growth() {
    // Run the test in two phases inside a single isolate via a
    // bespoke harness — the standard `run_in_v8` only takes one
    // script. We need to mutate the Remote between two next() calls.
    use zeroship_runtime::init_v8;
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);

    scope.set_slot(mut_scope_live::Remote {
        pairs: RefCell::new(vec![(
            ByteString::from_bytes(b"a".to_vec()),
            ByteString::from_bytes(b"1".to_vec()),
        )]),
    });
    install_class::<mut_scope_live::Bag>(
        mut_scope_live::Bag::install,
        "Bag",
        scope,
        global,
    );

    // Step 1: ask the iterator to yield one entry (live, but
    // remote only has 1 pair).
    let src1 = "globalThis.__b = new Bag(); globalThis.__it = __b.entries(); JSON.stringify(__it.next().value);";
    let src1_v8 = v8::String::new(scope, src1).unwrap();
    let r1 = v8::Script::compile(scope, src1_v8, None).unwrap().run(scope).unwrap();
    let r1_s = r1.to_rust_string_lossy(scope);
    assert_eq!(r1_s, r#"["a","1"]"#);

    // Step 2: grow the remote, then ask the iterator for another
    // entry. Live mode means the cursor walks into the new entry.
    let remote = scope.get_slot::<mut_scope_live::Remote>().unwrap();
    remote.pairs.borrow_mut().push((
        ByteString::from_bytes(b"b".to_vec()),
        ByteString::from_bytes(b"2".to_vec()),
    ));
    let src2 = "JSON.stringify(__it.next().value);";
    let src2_v8 = v8::String::new(scope, src2).unwrap();
    let r2 = v8::Script::compile(scope, src2_v8, None).unwrap().run(scope).unwrap();
    let r2_s = r2.to_rust_string_lossy(scope);
    assert_eq!(r2_s, r#"["b","2"]"#);
}
