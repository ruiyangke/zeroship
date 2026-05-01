//! Native `FormData` per WHATWG XHR §5
//! (https://xhr.spec.whatwg.org/#interface-formdata) and HTML §5.10.
//!
//! Replaces the JS polyfill that lived in `embed/formdata.js`. The
//! polyfill stored an internal `_entries` array, never validated the
//! `form` constructor argument (silently accepting anything), and its
//! iterators were not live (snapshot-on-construct, contradicting
//! WebIDL §3.7.10.2).
//!
//! ## v1 simplifications (documented inline)
//!
//! - **No HTMLFormElement / HTMLElement.** Server-side runtime has no
//!   DOM tree, so `new FormData(form)` with a non-undefined argument
//!   throws TypeError. Matches workerd / Cloudflare Workers.
//! - **No Blob / File support.** All values are USVString in v1; the
//!   `(Blob, filename)` overloads of `append` / `set` will surface
//!   when the native Blob class lands. v1 simply runs `ToString` on
//!   whatever is passed — which is harmless for the USVString-only
//!   `Request`/`Response` body integration coming next.
//! - **No multipart serialization.** That's part of fetch's body
//!   extraction (request-init step "byte sequence"), not FormData
//!   itself.
//!
//! ## Storage
//!
//! `Vec<(String, String)>` insertion-ordered (NOT a multimap — the
//! spec calls it "entry list" and order is observable: `forEach` /
//! `for-of` iterate in insertion order, and `set(name)` replaces the
//! first match in-place to preserve position).
//!
//! Compare with `Headers`: those use byte-case-insensitive name
//! lookup. FormData names are case-SENSITIVE per spec — string
//! equality, not byte-case-insensitive.
//!
//! ## Live iteration (WebIDL §3.7.10.2)
//!
//! `FormDataIterator::next()` re-reads the parent's entry list on every
//! call. Mutations between yields ARE observable. The polyfill
//! snapshotted at construction; this fixes that.
//!
//! ## Forward-compat (Blob + Body integration)
//!
//! When Blob lands: extend the storage to
//! `Vec<(String, FormDataEntry)>` where
//!   `enum FormDataEntry { Str(String), Blob(BlobEntry) }`.
//! The IDL methods become overload-dispatched on the second arg
//! (USVString vs Blob). Body integration: `extract_body` for
//! FormData runs the multipart serializer over the entry list.

use crate::state::OpError;

#[allow(unused_imports)]
use zeroship_runtime_macros::{
    v8_class, v8_constructor, v8_inherit_intrinsic, v8_method, v8_name, v8_to_string_tag,
};

// ---------------------------------------------------------------------------
// FormData struct
// ---------------------------------------------------------------------------

/// `FormData` instance state. Stored in the V8 wrapper's internal
/// field 0 as `Box<FormData>`.
///
/// `entries` is the WHATWG "entry list": `[(name, value), ...]` in
/// insertion order. v1 stores values as `String` (USVString); v2 will
/// promote to an enum once Blob exists.
#[derive(Default)]
pub struct FormData {
    pub entries: Vec<(String, String)>,
}

impl FormData {
    // -----------------------------------------------------------------
    // Spec algorithms (private). Names match the spec verbs (D-20).
    // -----------------------------------------------------------------

    /// "append an entry" (XHR §5): create an entry (name, value), add
    /// it to the entry list. v1 has no Blob so no filename / content-
    /// type bookkeeping.
    fn list_append(&mut self, name: String, value: String) {
        self.entries.push((name, value));
    }

    /// "set an entry" (XHR §5): if there are entries with name in the
    /// list, set the first such entry's value to value and remove the
    /// others. Otherwise append (name, value).
    fn list_set(&mut self, name: String, value: String) {
        let mut found = false;
        let mut taken_value = Some(value);
        self.entries.retain_mut(|(n, v)| {
            if *n == name {
                if !found {
                    found = true;
                    if let Some(new_v) = taken_value.take() {
                        *v = new_v;
                    }
                    true
                } else {
                    false
                }
            } else {
                true
            }
        });
        if !found {
            self.entries
                .push((name, taken_value.unwrap_or_default()));
        }
    }

    /// "delete entries" (XHR §5): remove every entry from the list
    /// whose name equals name.
    fn list_delete(&mut self, name: &str) {
        self.entries.retain(|(n, _)| n != name);
    }

    /// "first entry value" (XHR §5 dom-formdata-get): return the value
    /// of the first entry whose name matches, or null.
    fn list_get(&self, name: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    }

    /// "all entry values" (XHR §5 dom-formdata-getall): return values
    /// of all entries whose name matches, in list order.
    fn list_get_all(&self, name: &str) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// "contains an entry" (XHR §5 dom-formdata-has): true iff any
    /// entry has name as its name.
    fn list_has(&self, name: &str) -> bool {
        self.entries.iter().any(|(n, _)| n == name)
    }
}

// ---------------------------------------------------------------------------
// FormData IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
impl FormData {
    /// `new FormData(form?: HTMLFormElement, submitter?: HTMLElement)`
    ///
    /// XHR §5: "If form is given, then ... construct the entry list."
    /// Server-side we have no HTMLFormElement, so any non-undefined
    /// `form` argument throws TypeError. Matches workerd's behaviour.
    /// `submitter` is ignored entirely (only meaningful when form is
    /// present).
    ///
    /// `new FormData()` and `new FormData(undefined)` succeed with an
    /// empty entry list.
    #[v8_constructor]
    fn new(form: v8::Local<v8::Value>) -> Result<Self, OpError> {
        if !form.is_undefined() {
            return Err(OpError::type_error(
                "FormData constructor with HTMLFormElement is not supported",
            ));
        }
        Ok(FormData::default())
    }

    /// `append(name: USVString, value: USVString)` — XHR §5
    /// `dom-formdata-append`.
    ///
    /// v1 IDL is USVString-only (D-1 for v1; Blob overloads come with
    /// the native Blob class). Both arguments are USVString-coerced
    /// via the macro's `String` extraction (which uses
    /// `to_rust_string_lossy` — same effect as the USVString algorithm
    /// because lone surrogates are replaced with U+FFFD).
    #[v8_method]
    fn append(&mut self, name: String, value: String) {
        self.list_append(name, value);
    }

    /// `set(name: USVString, value: USVString)` — XHR §5
    /// `dom-formdata-set`. Same shape as append, but list-set.
    #[v8_method]
    fn set(&mut self, name: String, value: String) {
        self.list_set(name, value);
    }

    /// `delete(name: USVString)` — XHR §5 `dom-formdata-delete`.
    /// Renamed at JS surface because `delete` is a Rust keyword.
    #[v8_method]
    #[v8_name = "delete"]
    fn delete_(&mut self, name: String) {
        self.list_delete(&name);
    }

    /// `get(name: USVString) -> FormDataEntryValue?` — XHR §5
    /// `dom-formdata-get`. Returns the first matching entry's value,
    /// or null if no match.
    #[v8_method]
    fn get(&self, name: String) -> Option<String> {
        self.list_get(&name)
    }

    /// `getAll(name: USVString) -> sequence<FormDataEntryValue>` —
    /// XHR §5 `dom-formdata-getall`.
    #[v8_method]
    #[v8_name = "getAll"]
    fn get_all(&self, name: String) -> Vec<String> {
        self.list_get_all(&name)
    }

    /// `has(name: USVString) -> boolean` — XHR §5 `dom-formdata-has`.
    #[v8_method]
    fn has(&self, name: String) -> bool {
        self.list_has(&name)
    }

    // forEach / keys() / values() / entries() / [@@iterator] aren't
    // emitted by the macro. Same reason as `Headers`: they need direct
    // access to `args.this()` to wire `parent: Global<Object>` into the
    // FormDataIterator. They're installed in `install_global` below.
}

// ---------------------------------------------------------------------------
// FormDataIterator (live, default iterator object per WebIDL §3.7.10)
// ---------------------------------------------------------------------------

/// WebIDL §3.7.10 iteration kinds for the default iterator object.
#[derive(Debug, Clone, Copy)]
pub enum IterKind {
    /// "key" — `FormData.keys()` and the default iterator's key arg.
    Key,
    /// "value" — `FormData.values()`.
    Value,
    /// "key+value" — `FormData.entries()` and `[Symbol.iterator]`.
    KeyAndValue,
}

/// Iterator state. The `parent` Global keeps the source FormData
/// wrapper alive (and thus its boxed `FormData` valid); `index`
/// advances per `next()`; `kind` controls each yielded value's shape.
pub struct FormDataIterator {
    parent: Option<v8::Global<v8::Object>>,
    index: usize,
    kind: IterKind,
}

impl Default for FormDataIterator {
    fn default() -> Self {
        FormDataIterator {
            parent: None,
            index: 0,
            kind: IterKind::KeyAndValue,
        }
    }
}

#[v8_class]
#[v8_to_string_tag = "FormData Iterator"]
#[v8_inherit_intrinsic = "IteratorPrototype"]
impl FormDataIterator {
    /// `next() -> { value, done }` per ECMA-262 25.1.1 (Iterator
    /// Protocol) and WebIDL §3.7.10.2 (default iterator object).
    ///
    /// Re-reads the parent's `entries` on every call so mutations
    /// between yields ARE observable per spec.
    #[v8_method]
    fn next<'s>(&mut self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let parent_global = match &self.parent {
            Some(g) => g,
            None => return iter_result_done(scope),
        };
        let parent = v8::Local::new(scope, parent_global);

        // Reach into the parent's internal field 0 and reborrow its
        // boxed FormData. Safe because:
        //   - V8 isolates are per-thread (AGENTS.md key invariant).
        //   - The Global keeps the parent alive, which keeps
        //     Box<FormData> alive (its finalizer runs only after GC).
        //   - The borrow scope is the entirety of next() — we don't
        //     yield across it.
        let ext = match parent
            .get_internal_field(scope, 0)
            .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
        {
            Some(e) => e,
            None => return iter_result_done(scope),
        };
        let fd: &FormData = unsafe { &*(ext.value() as *const FormData) };

        if self.index >= fd.entries.len() {
            return iter_result_done(scope);
        }
        let (n, v) = fd.entries[self.index].clone();
        self.index += 1;

        let value: v8::Local<v8::Value> = match self.kind {
            IterKind::KeyAndValue => {
                let arr = v8::Array::new(scope, 2);
                let n_str = v8::String::new(scope, &n).unwrap();
                let v_str = v8::String::new(scope, &v).unwrap();
                arr.set_index(scope, 0, n_str.into());
                arr.set_index(scope, 1, v_str.into());
                arr.into()
            }
            IterKind::Key => v8::String::new(scope, &n).unwrap().into(),
            IterKind::Value => v8::String::new(scope, &v).unwrap().into(),
        };
        iter_result(scope, value, false)
    }
}

/// Build `{ value, done }` per ECMA-262 7.4.7 "CreateIterResultObject".
fn iter_result<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<v8::Value>,
    done: bool,
) -> v8::Local<'s, v8::Value> {
    let result = v8::Object::new(scope);
    let value_key = v8::String::new(scope, "value").unwrap();
    let done_key = v8::String::new(scope, "done").unwrap();
    let done_v = v8::Boolean::new(scope, done);
    result.set(scope, value_key.into(), value);
    result.set(scope, done_key.into(), done_v.into());
    result.into()
}

fn iter_result_done<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
    let undef = v8::undefined(scope);
    iter_result(scope, undef.into(), true)
}

// ---------------------------------------------------------------------------
// Iterator factory: keys / values / entries / [Symbol.iterator] / forEach
// ---------------------------------------------------------------------------

fn iter_template<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::FunctionTemplate> {
    FormDataIterator::install(scope)
}

/// Install `FormData` on `globalThis`, wiring the macro-emitted
/// constructor + IDL methods AND the parent-aware
/// keys/values/entries/forEach/[Symbol.iterator] surface that the
/// macro can't emit (they need access to `args.this()`).
pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let tmpl = FormData::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();

    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    let proto: v8::Local<v8::Object> = proto_v.try_into().unwrap();

    install_iter_factory(scope, proto, "keys", IterKind::Key);
    install_iter_factory(scope, proto, "values", IterKind::Value);
    install_iter_factory(scope, proto, "entries", IterKind::KeyAndValue);

    {
        let tmpl = v8::FunctionTemplate::new(scope, for_each_callback);
        let func = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "forEach").unwrap();
        proto.set(scope, key.into(), func.into());
    }

    // [Symbol.iterator] aliases entries per WebIDL §3.7.10.
    let sym_iter = v8::Symbol::get_iterator(scope);
    let entries_key = v8::String::new(scope, "entries").unwrap();
    let entries_v = proto.get(scope, entries_key.into()).unwrap();
    proto.set(scope, sym_iter.into(), entries_v);

    let key = v8::String::new(scope, "FormData").unwrap();
    global.set(scope, key.into(), class_fn.into());
}

/// Hand-rolled `FormData.prototype.forEach(callback, thisArg?)` per
/// WebIDL §3.7.10.3. Re-reads the live entry list between callback
/// invocations (mutation during forEach IS observable per spec).
///
/// Why hand-rolled (not macro): the callback's third arg is the
/// FormData object itself — `args.this()` — and the macro doesn't
/// thread that through user method bodies.
fn for_each_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this_obj = args.this();
    let fd: &mut FormData = match this_obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
    {
        Some(e) => unsafe { &mut *(e.value() as *mut FormData) },
        None => {
            let msg = v8::String::new(scope, "Illegal invocation").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let cb_arg = args.get(0);
    let cb_fn: v8::Local<v8::Function> = match cb_arg.try_into() {
        Ok(f) => f,
        Err(_) => {
            let msg = v8::String::new(scope, "forEach callback is not callable").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };
    let this_arg = args.get(1);

    let mut idx = 0usize;
    loop {
        if idx >= fd.entries.len() {
            return;
        }
        let (n, v) = fd.entries[idx].clone();
        idx += 1;

        let value_v = v8::String::new(scope, &v).unwrap();
        let key_v = v8::String::new(scope, &n).unwrap();
        let cb_args = [value_v.into(), key_v.into(), this_obj.into()];

        // call() returns None on throw — V8 has the exception pending.
        // Stop iteration; the throw propagates to the JS caller.
        if cb_fn.call(scope, this_arg, &cb_args).is_none() {
            return;
        }
    }
}

fn install_iter_factory<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    proto: v8::Local<v8::Object>,
    name: &str,
    kind: IterKind,
) {
    // Encode the kind as an integer in the External so the same
    // raw callback dispatches all three factory methods.
    let kind_marker: i64 = match kind {
        IterKind::Key => 0,
        IterKind::Value => 1,
        IterKind::KeyAndValue => 2,
    };
    let data = v8::Integer::new(scope, kind_marker as i32);
    let tmpl = v8::FunctionTemplate::builder(iter_factory_callback)
        .data(data.into())
        .build(scope);
    let func = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, name).unwrap();
    proto.set(scope, key.into(), func.into());
}

fn iter_factory_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // `this` must be a FormData instance — its internal field 0 holds
    // Box<FormData>. Verify by ext extraction; throw on mismatch.
    let this_obj = args.this();
    let _fd_ptr = match this_obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
    {
        Some(e) => e.value() as *mut FormData,
        None => {
            let msg = v8::String::new(scope, "Illegal invocation").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    // Decode kind from the FunctionTemplate data slot.
    let kind = {
        let raw = args.data();
        let n = if let Ok(int) = v8::Local::<v8::Integer>::try_from(raw) {
            int.value()
        } else {
            2
        };
        match n {
            0 => IterKind::Key,
            1 => IterKind::Value,
            _ => IterKind::KeyAndValue,
        }
    };

    // Build a FormDataIterator object via the cached template.
    let it_tmpl = iter_template(scope);
    let inst_tmpl = it_tmpl.instance_template(scope);
    let it_obj = match inst_tmpl.new_instance(scope) {
        Some(o) => o,
        None => {
            let msg = v8::String::new(scope, "Failed to allocate iterator").unwrap();
            let exc = v8::Exception::error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };
    // Wire prototype to the macro-emitted prototype (which carries
    // `next` and is chained to %IteratorPrototype% via
    // #[v8_inherit_intrinsic]).
    let it_class_fn = it_tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let it_proto_v = it_class_fn.get(scope, proto_key.into()).unwrap();
    it_obj.set_prototype(scope, it_proto_v);

    let parent_global = v8::Global::new(scope, this_obj);
    let boxed = Box::new(FormDataIterator {
        parent: Some(parent_global),
        index: 0,
        kind,
    });
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    it_obj.set_internal_field(0, ext.into());

    // Finalizer: same shape as the macro's gen_box_and_install_finalizer.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        it_obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut FormDataIterator));
        }),
    );
    std::mem::forget(weak);

    rv.set(it_obj.into());
}
