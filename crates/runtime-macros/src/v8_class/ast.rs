//! AST types shared between parse and emit phases of `#[v8_class]`.
//!
//! Extracted from `mod.rs` during the refactor that split the macro
//! into smaller parse and emit units.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MethodKind {
    Method,
    /// An async method — emits a callback that spawns a future via
    /// `state.spawned_ops` and returns a Promise. The user writes
    /// `async fn foo(&self, ...) -> T` (or `Result<T, OpError>`) and
    /// the macro hides the resolver/spawn dance. Rejected at compile
    /// time if the receiver is `&mut self` (borrow across .await is
    /// unsound under V8 re-entry).
    AsyncMethod,
    Getter,
    Setter,
    Constructor,
    /// WebIDL §3.7.4 static operation — `#[v8_static_method]`. No
    /// receiver, no brand check, no internal-field deref. Installed
    /// on the constructor FunctionTemplate, not the prototype.
    StaticMethod,
    /// WebIDL §3.7.4 static attribute (read-only) — `#[v8_static_getter]`.
    /// No receiver. Installed via `set_accessor_property` on the
    /// constructor template.
    StaticGetter,
}

pub(crate) struct ClassMethod<'a> {
    pub(crate) kind: MethodKind,
    pub(crate) func: &'a syn::ImplItemFn,
    /// Whether the receiver is `&mut self` (vs `&self`). Constructors
    /// have no receiver — we set this to false; it's unused for them.
    pub(crate) mut_receiver: bool,
    /// JS-visible name. Defaults to the Rust identifier; overridden by
    /// `#[v8_name = "..."]` on the method. Lets us install
    /// `delete_(&mut self)` under the JS name `delete`, etc.
    pub(crate) js_name: String,
    /// `#[v8_getter(same_object)]` — WebIDL `[SameObject]` semantics:
    /// the getter must return THE SAME JS object across reads on the
    /// same wrapper instance. The macro caches via a V8 private symbol
    /// keyed by `__zs_same_object_<ClassTy>_<getter>`. User method
    /// returns `v8::Global<v8::Object>` (minted on first call); macro
    /// stashes it on the wrapper instance and returns the cached Local
    /// thereafter. Only meaningful for `MethodKind::Getter`.
    pub(crate) same_object: bool,
    /// `#[v8_method(fastcall)]` / `#[v8_getter(fastcall)]` — emit a
    /// CFunction shim alongside the slow-path FunctionCallback so V8
    /// Turbofan can inline the typed-shape call at hot sites. See
    /// `extract_fastcall` for the rationale and `gen_fastcall_*` for
    /// the codegen detail. Mutually compatible with `same_object` only
    /// in the negative — fastcall paths can't allocate and SameObject
    /// returns a Global<Object>, so the two flags are not co-applicable.
    pub(crate) fastcall: bool,
    /// `#[reject_shared(arg1, arg2, …)]` — set of parameter names that
    /// should reject SharedArrayBuffer-backed views with TypeError. Per
    /// WebIDL §3.2.21 (BufferSource without `[AllowShared]`). Earlier
    /// versions walked `func.attrs` afresh per method; now the
    /// analyse phase fills this once.
    pub(crate) reject_shared_names: std::collections::HashSet<String>,
    /// `#[v8_constructor(callable_no_new)]` — opt the constructor out of
    /// the must-new guard. Only meaningful for `MethodKind::Constructor`.
    pub(crate) callable_no_new: bool,
    /// `#[v8_constructor(post_init = "fn_name")]` — post-init hook
    /// ident.
    /// Only meaningful for `MethodKind::Constructor`.
    pub(crate) post_init: Option<syn::Ident>,
}

/// A single `#[v8_const(NAME = LIT)]` declaration.
pub(crate) struct ConstDecl {
    /// The JS-visible property name (Rust ident verbatim).
    pub(crate) name: syn::Ident,
    /// The literal expression — quoted as-is so the literal's type
    /// suffix is preserved through expansion.
    pub(crate) value: syn::ExprLit,
    /// Selected V8-side materialiser, derived from the literal suffix.
    pub(crate) kind: ConstKind,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ConstKind {
    /// Suffix `u16` or `u32` → `v8::Integer::new_from_unsigned`.
    UInt,
    /// Suffix `i32` → `v8::Integer::new`.
    SInt,
}
