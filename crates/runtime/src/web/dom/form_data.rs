//! Native `FormData` per WHATWG XHR §5
//! (https://xhr.spec.whatwg.org/#interface-formdata) and HTML §5.10.
//!
//! Replaces the JS polyfill that lived in `embed/formdata.js`. The
//! polyfill stored an internal `_entries` array, never validated the
//! `form` constructor argument (silently accepting anything), and its
//! iterators were not live (snapshot-on-construct, contradicting
//! WebIDL §3.7.10.2).
//!
//! ## Storage
//!
//! `Vec<(String, FormDataValue)>` insertion-ordered (NOT a multimap —
//! the spec calls it "entry list" and order is observable: `forEach` /
//! `for-of` iterate in insertion order, and `set(name)` replaces the
//! first match in-place to preserve position).
//!
//! `FormDataValue` is one of:
//!   - `String(String)` — USVString entry,
//!   - `Blob(v8::Global<v8::Object>)` — a File (post-spec wrapping per
//!     XHR §5: a Blob value is wrapped as a File at append time with
//!     name="blob" or the supplied filename, lastModified=current
//!     time. We always store a File-shaped object so `get` can return
//!     `instanceof File === true` directly.).
//!
//! Compare with `Headers`: those use byte-case-insensitive name
//! lookup. FormData names are case-SENSITIVE per spec — string
//! equality, not byte-case-insensitive.
//!
//! ## Live iteration (WebIDL §3.7.10.2)
//!
//! `FormDataIterator::next()` re-reads the parent's entry list on every
//! call. Mutations between yields ARE observable.

use crate::state::OpError;
use crate::webidl::usv_string::USVString;

#[allow(unused_imports)]
use zeroship_runtime_macros::{
    v8_class, v8_constructor, v8_inherit_intrinsic, v8_iterable, v8_method, v8_name,
    v8_to_string_tag,
};

// ---------------------------------------------------------------------------
// FormData struct
// ---------------------------------------------------------------------------

/// One entry value: either a USVString or a File-wrapped Blob.
///
/// Per WHATWG XHR §5 the spec calls this `FormDataEntryValue` and
/// defines it as `(File | USVString)`. We store the File-side as a
/// V8 Global so the JS object identity is preserved across get()
/// calls (mandated by `setEntries(name, value)` step "set its
/// value to entry's value", which the WPT iterates).
///
/// `Clone` is required by `#[v8_iterable(value_marshal = ...)]`: the
/// macro clones each pair before yielding so it can advance the
/// cursor without holding a borrow into `__pairs`. `v8::Global<T>` is
/// itself Clone (Rc-shaped), so this is structurally cheap.
#[derive(Clone)]
pub enum FormDataValue {
    /// USVString entry.
    String(String),
    /// File entry (always a File, not a Blob — per the spec's
    /// "blob → File" wrapping at append time).
    File(v8::Global<v8::Object>),
}

/// `FormData` instance state. Stored in the V8 wrapper's internal
/// field 0 as `Box<FormData>`.
///
/// `entries` is the WHATWG "entry list": `[(name, value), ...]` in
/// insertion order.
#[derive(Default)]
pub struct FormData {
    pub entries: Vec<(String, FormDataValue)>,
}

impl FormData {
    // -----------------------------------------------------------------
    // Spec algorithms (private). Names match the spec verbs.
    // -----------------------------------------------------------------

    fn list_append(&mut self, name: String, value: FormDataValue) {
        self.entries.push((name, value));
    }

    fn list_set(&mut self, name: String, value: FormDataValue) {
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
                .push((name, taken_value.unwrap_or(FormDataValue::String(String::new()))));
        }
    }

    fn list_delete(&mut self, name: &str) {
        self.entries.retain(|(n, _)| n != name);
    }

    fn list_has(&self, name: &str) -> bool {
        self.entries.iter().any(|(n, _)| n == name)
    }
}

// ---------------------------------------------------------------------------
// FormData IDL surface (constructor + has + delete via macro;
// append/set/get/getAll hand-rolled below — they need scope-aware
// argument processing the macro doesn't yet model. keys/values/
// entries/forEach/[Symbol.iterator] come from `#[v8_iterable]`).
// ---------------------------------------------------------------------------

#[v8_class]
#[v8_iterable(
    key = USVString,
    value = FormDataValue,
    mode = live,
    value_marshal = entry_value_to_v8
)]
impl FormData {
    /// `new FormData(form?: HTMLFormElement, submitter?: HTMLElement)`
    ///
    /// Server-side we have no HTMLFormElement, so any non-undefined
    /// `form` argument throws TypeError.
    #[v8_constructor]
    fn new(form: v8::Local<v8::Value>) -> Result<Self, OpError> {
        if !form.is_undefined() {
            return Err(OpError::type_error(
                "FormData constructor with HTMLFormElement is not supported",
            ));
        }
        Ok(FormData::default())
    }

    /// `delete(name: USVString)` — XHR §5 `dom-formdata-delete`.
    #[v8_method]
    #[v8_name = "delete"]
    fn delete_(&mut self, name: String) {
        self.list_delete(&name);
    }

    /// `has(name: USVString) -> boolean` — XHR §5 `dom-formdata-has`.
    #[v8_method]
    fn has(&self, name: String) -> bool {
        self.list_has(&name)
    }

    /// `value_pairs(&self)` — the WebIDL §3.7.10.2 "value pairs to
    /// iterate over" hook for `#[v8_iterable(mode = live)]`. Clones
    /// the entries into `(USVString, FormDataValue)` (the `Global`
    /// inside `FormDataValue::File` is Rc-shaped — clone is cheap).
    /// The macro yields each pair via `entry_value_to_v8` (specified
    /// by `value_marshal`) so File entries surface as their own
    /// V8 object identity rather than a string.
    fn value_pairs(&self) -> Vec<(USVString, FormDataValue)> {
        self.entries
            .iter()
            .map(|(n, v)| (USVString::from(n.clone()), v.clone()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Value-to-V8 marshal — used by `#[v8_iterable(value_marshal = …)]`
// ---------------------------------------------------------------------------

/// Convert a stored `FormDataValue` to a JS value in the given scope.
/// Strings become V8 strings; File entries become Local handles
/// (preserving JS object identity across `get` calls per WHATWG XHR
/// §5).
///
/// Wired into the macro via `value_marshal = entry_value_to_v8` on the
/// `#[v8_iterable]` attribute. Per yield (live mode), the macro
/// emits `let __v_v: Local<Value> = entry_value_to_v8(scope, &__v);`,
/// so the union shape `(USVString or File)` doesn't need to live in
/// the macro's built-in classifier.
fn entry_value_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    v: &FormDataValue,
) -> v8::Local<'s, v8::Value> {
    match v {
        FormDataValue::String(s) => v8::String::new(scope, s).unwrap().into(),
        FormDataValue::File(g) => {
            let local = v8::Local::new(scope, g);
            local.into()
        }
    }
}

// ---------------------------------------------------------------------------
// install_global — wire append/set/get/getAll. The iterable surface
// (keys/values/entries/forEach/[@@iterator]) comes from the macro.
// ---------------------------------------------------------------------------

pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let tmpl = FormData::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();

    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    let proto: v8::Local<v8::Object> = proto_v.try_into().unwrap();

    // append/set/get/getAll stay hand-rolled — they need scope-aware
    // argument processing for the (USVString, value, optional
    // filename) overloads that the macro doesn't yet model. The
    // iterator surface is now macro-generated above.
    install_method(scope, proto, "append", append_callback);
    install_method(scope, proto, "set", set_callback);
    install_method(scope, proto, "get", get_callback);
    install_method(scope, proto, "getAll", get_all_callback);

    let key = v8::String::new(scope, "FormData").unwrap();
    global.set(scope, key.into(), class_fn.into());
}

fn install_method<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    proto: v8::Local<v8::Object>,
    name: &str,
    cb: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let tmpl = v8::FunctionTemplate::new(scope, cb);
    let func = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, name).unwrap();
    proto.set(scope, key.into(), func.into());
}

// ---------------------------------------------------------------------------
// append / set — handle both USVString and Blob/File overloads
// ---------------------------------------------------------------------------

fn append_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this_obj = args.this();
    let fd = match fd_from_this(scope, this_obj) {
        Some(p) => p,
        None => {
            throw_illegal_invocation(scope);
            return;
        }
    };
    let name = args.get(0).to_rust_string_lossy(scope);
    let value_v = args.get(1);
    let filename_v = args.get(2);
    let entry = match build_entry(scope, value_v, filename_v) {
        Ok(e) => e,
        Err(msg) => {
            let m = v8::String::new(scope, &msg).unwrap();
            let exc = v8::Exception::type_error(scope, m);
            scope.throw_exception(exc);
            return;
        }
    };
    fd.list_append(name, entry);
}

fn set_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this_obj = args.this();
    let fd = match fd_from_this(scope, this_obj) {
        Some(p) => p,
        None => {
            throw_illegal_invocation(scope);
            return;
        }
    };
    let name = args.get(0).to_rust_string_lossy(scope);
    let value_v = args.get(1);
    let filename_v = args.get(2);
    let entry = match build_entry(scope, value_v, filename_v) {
        Ok(e) => e,
        Err(msg) => {
            let m = v8::String::new(scope, &msg).unwrap();
            let exc = v8::Exception::type_error(scope, m);
            scope.throw_exception(exc);
            return;
        }
    };
    fd.list_set(name, entry);
}

/// Build a `FormDataValue` from the IDL argument pair `(value,
/// filenameOrUndefined)`. Per WHATWG XHR §5 "create an entry" /
/// HTML "constructing the form data set":
///
///   - If `value` is a File and filename is missing → entry value
///     is the File itself (preserve identity).
///   - If `value` is a File and filename is given → entry value is a
///     new File with content+type+lastModified copied from the
///     source but name set to filename.
///   - If `value` is a Blob (not a File) → wrap as a new File with
///     name=filename or "blob", lastModified=now.
///   - Otherwise → USVString-coerce.
fn build_entry(
    scope: &mut v8::PinScope,
    value_v: v8::Local<v8::Value>,
    filename_v: v8::Local<v8::Value>,
) -> Result<FormDataValue, String> {
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(value_v) {
        if crate::blob_native::blob::is_blob_instance_public(scope, obj) {
            // Determine the filename arg (USVString-coerced).
            let filename: Option<String> = if filename_v.is_undefined() {
                None
            } else {
                Some(filename_v.to_rust_string_lossy(scope))
            };
            let is_file = crate::blob_native::blob::is_file_instance_public(scope, obj);
            // Identity preservation: File without filename override.
            if is_file && filename.is_none() {
                return Ok(FormDataValue::File(v8::Global::new(scope, obj)));
            }
            // Mint a new File from the Blob/File's bytes + type.
            let (bytes, blob_type) =
                match crate::blob_native::blob::read_blob_bytes_and_type(scope, obj) {
                    Some(p) => p,
                    None => {
                        return Err(
                            "FormData append/set: Blob value has no readable bytes".into()
                        )
                    }
                };
            let final_filename = filename.unwrap_or_else(|| "blob".to_string());
            // Preserve lastModified for File→File rewraps; default to
            // current time for Blob→File.
            let last_modified: Option<i64> = if is_file {
                let lm_key = v8::String::new(scope, "lastModified").unwrap();
                obj.get(scope, lm_key.into())
                    .and_then(|v| v.number_value(scope))
                    .map(|n| n as i64)
            } else {
                None
            };
            let file_v = crate::blob_native::file::create_file_with_last_modified(
                scope,
                bytes,
                final_filename,
                &blob_type,
                last_modified,
            );
            let file_obj: v8::Local<v8::Object> = match file_v.try_into() {
                Ok(o) => o,
                Err(_) => {
                    return Err(
                        "FormData append/set: minted File is not an object".into()
                    )
                }
            };
            return Ok(FormDataValue::File(v8::Global::new(scope, file_obj)));
        }
    }
    // USVString fallback. Spec requires USVString conversion (lone
    // surrogates → U+FFFD); to_rust_string_lossy does this.
    Ok(FormDataValue::String(value_v.to_rust_string_lossy(scope)))
}

// ---------------------------------------------------------------------------
// get / getAll
// ---------------------------------------------------------------------------

fn get_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this_obj = args.this();
    let fd = match fd_from_this(scope, this_obj) {
        Some(p) => p,
        None => {
            throw_illegal_invocation(scope);
            return;
        }
    };
    let name = args.get(0).to_rust_string_lossy(scope);
    for (n, v) in &fd.entries {
        if *n == name {
            let local = entry_value_to_v8(scope, v);
            rv.set(local);
            return;
        }
    }
    rv.set(v8::null(scope).into());
}

fn get_all_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this_obj = args.this();
    let fd = match fd_from_this(scope, this_obj) {
        Some(p) => p,
        None => {
            throw_illegal_invocation(scope);
            return;
        }
    };
    let name = args.get(0).to_rust_string_lossy(scope);
    let arr = v8::Array::new(scope, 0);
    let mut i: u32 = 0;
    for (n, v) in &fd.entries {
        if *n == name {
            let local = entry_value_to_v8(scope, v);
            arr.set_index(scope, i, local);
            i += 1;
        }
    }
    rv.set(arr.into());
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fd_from_this<'a>(
    scope: &mut v8::PinScope,
    this_obj: v8::Local<v8::Object>,
) -> Option<&'a mut FormData> {
    let ext = this_obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())?;
    let ptr = ext.value() as *mut FormData;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: Each FormData wrapper carries a unique boxed FormData
    // (the macro's gen_box_and_install_finalizer ensures finalizer
    // ownership). V8 isolates are single-threaded per AGENTS.md
    // invariant, and we don't yield across the borrow.
    Some(unsafe { &mut *ptr })
}

fn throw_illegal_invocation(scope: &mut v8::PinScope) {
    let msg = v8::String::new(scope, "Illegal invocation").unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    scope.throw_exception(exc);
}
