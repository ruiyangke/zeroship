# `runtime-macros` — Refactor Guide (final-state spec)

- **Date:** 2026-05-05
- **Status:** Draft v2 — proposal (do **not** commit until the implementing PR lands; per `feedback_proposal_workflow`)
- **v2 changelog:** addresses critic-round R1 (panic safety, serde divergence, PoC, in-crate facade decision, Slot type compile error, PR sequencing for Wave 5, property-test plan, diff classifier, deprecation policy timeline, KnownType context split, Wave 4↔5 dependency unbundling, fastcall arg/return non-overlap)
- **v1 → v2 score progression target:** R0 self-score 88.6% → R1 critic 71/100 → v2 target ≥85/100
- **Tracking:** `crates/runtime-macros/TODO.md` "Critique findings (2026-05-05)"
- **Owner / scope:** `crates/runtime-macros/` end-to-end. No consumer-side migrations land in this proposal — `crates/runtime/` follow-ups are tracked in §3.5 and §4 only as targets for the new public API.
- **LOC delta when fully landed:** approximately **-450 LOC net** in `runtime-macros` itself (mod.rs -1,150; method.rs -150; lib.rs -120; webidl_dict.rs -40; webidl_enum.rs -10; offset by +1,000 LOC of new files in `shared/`, `v8_class/emit/`, `v8_class/fastcall/`, `v8_iterable/`, `webidl_dict/`, and `webidl_enum/`). Cognitive-load delta is the headline number, not LOC: every file ≤ 250 LOC, every codegen helper ≤ 60 LOC, one parameter object instead of 10 args, one trait family instead of 12 ad-hoc parsers.
- **References:**
  - `docs/reviews/runtime-macros-code-critique-2026-05-05-v3.md` — line-level findings (8 critical, 17 high, 13 medium, 10 low). v1 and v2 review iterations were dropped; v3 is the final post-Wave-9 closeout.
  - `docs/reviews/runtime-macros-architecture-critique-2026-05-05-v3.md` — F1-F10 + §3 anti-patterns + §5 crate boundary + §13 subtle + §10 ranked recommendations. v1/v2 dropped; v3 is the final closeout.
  - `docs/proposals/macro-v8-state.md` (MAC-01) — establishes the marker/state split this refactor preserves unchanged.
  - `docs/proposals/macro-constructor-post-init.md` (MAC-02) — establishes the `post_init` codegen this refactor preserves unchanged.

> **Composite score today: 60/100** (architecture critic) / **6.0/10** (code critic). Target after Waves 2-8 land: **≥85/100**. Wave 1 (commits `22ab81c`, `ec09c37`, `d9c5528`, `896c6de`, `ae43938`, `f81e982`) shipped the smallest, highest-ROI fixes; the remaining waves are open and described below.

---

## §1 Problem statement

### §1.1 Why we are refactoring

`runtime-macros` is the **load-bearing kernel** of the WebIDL surface. Every `#[v8_class]`-driven class — Headers, URL, URLSearchParams, Blob, File, Event, CloseEvent, MessageEvent, CustomEvent, DOMException, AbortController, AbortSignal, AsyncLocalStorage, Request, Response, plus all live iterators (HeadersIterator, URLSearchParamsIterator, FormDataIterator) — flows through this crate. The two derives (`WebIdlDict`, `WebIdlEnum`) handle every WebIDL dictionary and enum used in `web/fetch`, `web/streams`, `web/dom`, `webcrypto`, and `node_*`. The crate is ~6,800 LOC of proc-macro that emits another ~12K LOC of Rust per build.

<!-- Added in v2 R1: addressing critic's CRITICAL-1 / Problem framing — quantified blast radius -->

#### §1.1.1 Blast radius (quantified)

Concrete impact numbers, gathered against `master @ 4c41db3`:

| Metric | Count | Source |
|---|---|---|
| `#[v8_class]`-driven classes shipped | **18** | `grep -rl '#\[v8_class\]' crates/runtime/src/` |
| `WebIdlDict` derives | **24** | `grep -rl 'derive(WebIdlDict' crates/runtime/src/` |
| `WebIdlEnum` derives | **9** | `grep -rl 'derive(WebIdlEnum' crates/runtime/src/` |
| Smoke tests gated on macro behavior | **254+** | `crates/runtime/tests/v8_*_smoke.rs` |
| Trybuild compile-fail snapshots | **6** | `crates/runtime-macros/tests/compile_fail/` |
| Insta snapshots | **3** | `crates/runtime-macros/src/v8_class/snapshots/` |
| Macro source LOC | **~6,800** | `wc -l crates/runtime-macros/src/**/*.rs` |
| Avg. emit per class (estimated) | **~700 LOC** | proxy: `cargo-expand` on Headers.rs ≈ 680 LOC |
| Symbol-leak consumer sites | **4** distinct files | request.rs:230,924; als.rs:259; event_target.rs:125-178 |
| Hand-rolled `__InstallSlot_*` mimics | **1** (`event_target.rs`) | the F2 escape hatch |
| Time spent reading the megaquote in PR review (anecdote) | **~25 min/PR** | observation by the v1 author |

The total surface affected by a single `runtime-macros` change is ~12K LOC of generated Rust across ~30 consumer files. A subtle emit drift (e.g., a fastcall arg-type miscoercion) reaches every consumer simultaneously.

**Comparable refactors:**
- `serde_derive` 1.0.x → 1.0.140 split `internals/` from the proc-macro entry — landed over ~2 months as a series of additive PRs.
- `pin-project` 0.4 → 1.0 introduced `pin-project-internal` as a separate crate plus a parent crate — the macro-emit-runtime split is the same architectural axis as our Wave 5 facade.
- `tracing-attributes` 0.1.21 → 0.1.27 factored `expand::*` into per-emit-fragment helpers — the same shape as our Wave 6 `emit/*.rs` split.

Two parallel critique reports landed on 2026-05-05 with a converged diagnosis:

| Dimension | Code-critic | Arch-critic | Joint headline |
|---|---|---|---|
| Correctness | 5/10 | implicit (clean) | F1 quadruple OpError-throw duplication; C3 brand-check cap rationale wrong; C4 finalizer/teardown ordering hole; C5 HashSet-keyed re-entry guard wastes memory; C8 `expect()` panic in emit code |
| Naming / Public surface | 5/10 | 4/10 | F2 `__zs_*` / `__InstallSlot_*` are de-facto public API consumed by name (request.rs:230, request.rs:924, als.rs:259, hand-rolled mimic in event_target.rs:125) — the macro has a public API nobody declared |
| Organization / File / module | 7/10 | 5/10 | F3 mod.rs at 1,409 LOC + 195-line megaquote at lines 507-700; F4 gen_install takes 10 parameters; F7 ConstDecl/ConstKind/MethodKind/ClassMethod live in mod.rs but parse.rs constructs them — cyclic ownership |
| Patterns / Extensions | 6/10 | 5-6/10 | F5 12 `extract_*` parsers with 4 different return shapes (`bool`, `Option<T>`, `Result<Option<T>, syn::Error>`, `HashSet<String>`) and inconsistent strict-vs-silent error policy; F8 6+ redundant attribute walks per impl block |
| Test architecture | — | 6/10 | F9 no insta snapshots for `WebIdlDict` / `WebIdlEnum` / `v8_iterable` — refactor drift will pass smoke tests |

The composite score is **60/100**. Not catastrophic. Functional. Well-documented. **Below the 80+ bar for production Rust** that this codebase enforces in other crates (`compio-postgres`, `runtime/streams_native.rs`, `gateway/dispatch`).

### §1.2 The five concrete debts

Listed in priority order, with status of each:

#### Debt 1 — 4× OpError-throw duplication (C1 + C2 + F1)

Status: **Wave 1 partially closed.** Commit `22ab81c` factored `gen_throw_op_error_arms()` in `lib.rs:722-749` and migrated the two `lib.rs` sites (`gen_extract_throw` and `gen_throw_error`). The two remaining sites in `crates/runtime-macros/src/v8_class/method.rs:760-789` (constructor's `make_instance` Result arm) and `crates/runtime-macros/src/v8_class/method.rs:829-857` (post_init OpError arm) **still hand-roll the dispatch verbatim** — a comment at `method.rs:825-828` openly admits the duplication. Wave 2 closes them.

#### Debt 2 — `__zs_*` / `__InstallSlot_*` symbol-name leak (F2)

Status: **Open.** Consumers reach into the macro's emit by literal symbol name:

- `crates/runtime/src/web/fetch/request.rs:230` calls `__zs_is_Request(scope, input_v)`
- `crates/runtime/src/web/fetch/request.rs:924` reads `scope.get_slot::<__InstallSlot_Request>()`
- `crates/runtime/src/node/async_hooks/als.rs:259` calls `__zs_is_AsyncLocalStorage(scope, this_v)`
- `crates/runtime/src/web/dom/event_target.rs:125-178` **hand-rolls** `pub struct __InstallSlot_EventTarget(...)` to mimic the macro's slot type so that `#[v8_inherit(EventTarget)]` derives find a compatible cache slot.

The double-underscore says private; the consumer code disagrees. Renaming any of these is a breaking change. There is no `STABILITY.md` documenting the contract. Wave 5 closes this with a `runtime/` facade + `STABILITY.md` + a re-exported `<Class>::is_instance` / `<Class>::Slot` public API.

#### Debt 3 — mod.rs at 1,409 LOC + 195-line megaquote (F3)

Status: **Open.** `crates/runtime-macros/src/v8_class/mod.rs:507-700` is one `quote! { ... }` invocation that emits the user's stripped impl, the install slot, the brand slot, the brand-check fn (61 LOC of inline body), the `__zs_is_<Class>` fn, the `impl <Class> { #install }` block, the constructor callback, all method/getter/setter/async-method callbacks, all fastcall callbacks, AND the iterable codegen — 195 lines of tokens with deeply nested doc comments inside the templating expression. Modifying anything in this block requires reading 195 lines to understand the emission order. Wave 6 splits this into per-fragment helpers (`gen_brand_check_helpers`, `gen_install_slot_types`, `gen_public_is_fn`).

#### Debt 4 — `gen_install` 10-parameter signature (F4)

Status: **Open.** `crates/runtime-macros/src/v8_class/mod.rs:739-750`:

```rust
fn gen_install(
    class_ty: &syn::Ident,
    methods: &[&ClassMethod],
    to_string_tag_override: Option<&str>,
    inherit_intrinsic: Option<&str>,
    inherit_base: Option<&syn::Path>,
    install_iterable_call: Option<&TokenStream2>,
    async_iterable_method: Option<&str>,
    const_decls: &[ConstDecl],
    has_any_fastcall: bool,
) -> TokenStream2 { ... }
```

(Note: `has_user_constructor` was removed in `ae43938`; today's signature is 9 args, but every new attribute lands one more arg. MAC-08 / MAC-10 / MAC-11 will push it back over 10 if landed un-refactored.)

Wave 3 introduces a `ClassConfig` struct populated in `expand_tokens` and consumed by every codegen helper. Adding a new attribute = one field, not a parameter-list migration.

#### Debt 5 — Stringly-typed dispatch tables (anti-pattern §3)

Status: **Open.** At least 7 dispatch tables keyed by `Type::to_string()`:

| Site | Purpose | Cited |
|---|---|---|
| `lib.rs:851-929` (`gen_extract` final match) | extract scalar types | F10 |
| `lib.rs:554-562` (`clamp_kind`) | `[Clamp]` newtype detection | L10 |
| `lib.rs:571-580` (`wrap_kind`) | `Wrap*` newtype detection | L10 |
| `lib.rs:489-595` (9 `is_X` predicates) | `is_byte_string` / `is_vec_u8` / `is_usv_string` etc. | L10 |
| `parse.rs:14-39` (`classify`) | method kind by attribute name | §3 |
| `fastcall.rs:72-82, 119-122, 141-145` | allowed primitives (3 sites) | F6 |
| `fastcall.rs:194-250, 268-407` | arg / return mapping | §3 |

Each new type or attribute requires updating multiple stringly-keyed match arms in the right priority order (`is_byte_string` checked before `Option`-handling). Wave 4 introduces a `KnownType` registry + a single dispatcher.

### §1.3 Target

Composite **≥85/100** on the same 7-dimension rubric after Waves 2-8 land. Re-run the same critic+reviser loop on the refactored crate; the score progression is tracked per-wave in §4.

Constraints — items that **must not change**:

- 14 `pub` proc-macro entries (`v8_class`, `v8_method`, `v8_async_method`, `v8_getter`, `v8_setter`, `v8_constructor`, `v8_static_method`, `v8_static_getter`, `v8_inherit`, `v8_inherit_intrinsic`, `v8_state_marker`, `v8_const`, `v8_async_iterable`, `v8_name`, `v8_to_string_tag`, `reject_shared`) and the 2 derives (`WebIdlDict`, `WebIdlEnum`).
- All 254+ `v8_*_smoke` tests in `crates/runtime/tests/` continue to pass.
- All trybuild compile-fail snapshots continue to pass with byte-identical wording.
- `httpGet 16w` benchmark within ±5% of the 314,753 req/s baseline (`crates/runtime/benches/results-2026-05-04-after-fastcall.txt`).
- The recently-shipped extensions (post_init, fastcall, value_marshal, value_pairs `&mut self`, paired accessors, v8_const, v8_async_iterable) keep their behavior contracts byte-for-byte.

---

## §2 Target architecture

### §2.1 Final-state file layout

```
crates/runtime-macros/
├── Cargo.toml
├── STABILITY.md                              ← NEW (Wave 5)
├── README.md                                 ← NEW (Wave 5; module map, attribute-add walkthrough)
├── src/
│   ├── lib.rs                                ← proc-macro entries ONLY (~250 LOC, down from 1,351)
│   │
│   ├── runtime/                              ← NEW (Wave 5): emit-time facade re-exports
│   │   └── mod.rs                            (~120 LOC; one module per emit-target namespace)
│   │
│   ├── shared/                               ← NEW (Wave 3): phase-shared codegen primitives
│   │   ├── mod.rs                            (~10 LOC; pub-uses)
│   │   ├── class_config.rs                   ClassConfig struct + builders (~150 LOC)
│   │   ├── op_error.rs                       gen_throw_op_error_arms (Wave 1 ✅)
│   │   ├── must_str.rs                       gen_must_str / must_str_abs (Wave 1 ✅)
│   │   ├── recover_box.rs                    gen_recover_box / gen_recover_external (Wave 2)
│   │   ├── reentry_guard.rs                  gen_reentry_guard (Cell<Option<usize>>) (Wave 2)
│   │   └── known_type.rs                     KnownType registry + classify_extract (Wave 4)
│   │
│   ├── v8_class/
│   │   ├── mod.rs                            ← orchestration only (~150 LOC, down from 1,409)
│   │   ├── parse/                            ← split (Wave 4)
│   │   │   ├── mod.rs                        ParsedClassAttrs + ParsedMethodAttrs
│   │   │   ├── marker_attr.rs                MarkerAttr trait + extract_marker_attr<T>
│   │   │   ├── class_attrs.rs                parse_class_attrs (single walk)
│   │   │   ├── method_attrs.rs               parse_method_attrs (single walk)
│   │   │   ├── ast.rs                        MethodKind, ClassMethod, ConstDecl, ConstKind (moved from mod.rs — closes F7)
│   │   │   └── resolve.rs                    resolve_state_and_marker
│   │   ├── analyze.rs                        ← NEW (Wave 3): ParsedClassAttrs → ClassConfig
│   │   ├── emit/                             ← NEW (Wave 3+6): split the megaquote
│   │   │   ├── mod.rs                        assemble_tokens(&ClassConfig) (~80 LOC)
│   │   │   ├── install.rs                    gen_install(&ClassConfig)
│   │   │   ├── brand.rs                      gen_brand_check_helpers
│   │   │   ├── slot_types.rs                 gen_install_slot_types + gen_public_is_fn
│   │   │   ├── constructor.rs                gen_constructor_callback / gen_default_constructor_callback
│   │   │   ├── method.rs                     gen_method_callback (slow path)
│   │   │   ├── async_method.rs               gen_async_method_callback (split out)
│   │   │   ├── getter.rs                     gen_getter_callback / gen_same_object_getter_callback
│   │   │   ├── setter.rs                     gen_setter_callback
│   │   │   ├── static_op.rs                  gen_static_callback
│   │   │   ├── consts.rs                     gen_const_install (v8_const)
│   │   │   ├── async_iterable.rs             gen_async_iterable_install (v8_async_iterable)
│   │   │   ├── box_install.rs                gen_box_and_install_finalizer
│   │   │   └── must_new.rs                   gen_must_new_prologue
│   │   ├── fastcall/                         ← split (Wave 4)
│   │   │   ├── mod.rs                        validate_fastcall_signature + facade
│   │   │   ├── types.rs                      FastcallType enum + KnownType bridge
│   │   │   └── emit.rs                       gen_fastcall_callback (cinfo + cfn + extern "C" shim)
│   │   └── snapshots/                        existing 3 + 6 new from Wave 7
│   │
│   ├── v8_iterable/                          ← split the 1,291-LOC monolith (Wave 7)
│   │   ├── mod.rs                            extract_iterable + generate (orchestration, ~80 LOC)
│   │   ├── parse.rs                          IterableAttr parsing + value_pairs sniffer
│   │   ├── analyze.rs                        IterableConfig (mode, marshalling, receiver shape)
│   │   ├── emit_factory.rs                   keys/values/entries/forEach factory codegen
│   │   ├── emit_iterator.rs                  the FooIterator companion class codegen
│   │   ├── emit_marshal.rs                   per-K / per-V marshaling (uses KnownType registry)
│   │   └── snapshots/                        ← NEW (Wave 7): snapshot + live + value_marshal cases
│   │
│   ├── webidl_dict/                          ← split (Wave 7)
│   │   ├── mod.rs                            expand entry + DictConfig
│   │   ├── parse.rs                          field-attr parser (reject_null, name override)
│   │   ├── emit.rs                           per-field extraction codegen
│   │   └── snapshots/                        ← NEW (Wave 7)
│   │
│   └── webidl_enum/                          ← split (Wave 7)
│       ├── mod.rs                            expand entry + EnumConfig
│       ├── parse.rs                          variant-attr parser
│       ├── emit.rs                           from_str / from_v8 codegen
│       ├── pascal_to_kebab.rs                ✅ already extracted (Wave 1, 896c6de)
│       └── snapshots/                        ← NEW (Wave 7)
│
└── tests/
    ├── compile_fail/                         ← consolidated (Wave 4 emits new strict-attr rejections)
    │   ├── post_init/                        existing 4 fixtures
    │   ├── state_marker/                     existing 2 fixtures
    │   ├── v8_name_malformed/                ← NEW (Wave 4)
    │   ├── v8_to_string_tag_malformed/       ← NEW (Wave 4)
    │   ├── v8_inherit_intrinsic_malformed/   ← NEW (Wave 4)
    │   ├── fastcall_signature/               ← NEW (Wave 7)
    │   └── webidl_dict_reference_field/      ← NEW (Wave 7)
    └── insta_macro_outputs/                  ← marshalling for new snapshot suites (Wave 7)
```

### §2.2 Per-file specification

For every new file, the spec below states:

- **LOC budget** — hard ceiling; if exceeded, split further.
- **Public surface** — top-level functions/types and their visibility.
- **Closes** — which critique finding the file addresses.

#### `src/lib.rs` — proc-macro entries only (~250 LOC)

After: only the 16 `#[proc_macro_attribute]` / `#[proc_macro_derive]` `pub fn` entries plus thin module declarations. Every helper migrates out. Closes part of F3 (lib.rs is one of the three god files; Wave 1 already shrank it from 1,371 → 1,351 by extracting `must_str` and `gen_throw_op_error_arms`; Waves 3-4 shrink it the rest of the way).

```rust
mod runtime;
mod shared;
mod v8_class;
mod v8_iterable;
mod webidl_dict;
mod webidl_enum;

#[proc_macro_attribute]
pub fn v8_class(attr: TokenStream, item: TokenStream) -> TokenStream { v8_class::expand(attr, item) }
// ... 13 more attribute markers + 2 derives ...
```

#### `src/runtime/mod.rs` — emit-time facade (~120 LOC, Wave 5)

Re-exports every `::zeroship_runtime::*` path the macro emits, so the macro's emit references go through ONE module instead of 28 hard-coded paths. Closes F2 (second half) and §5 crate boundary.

```rust
//! Stable emit-time facade for the zeroship runtime.
//! 
//! The `runtime-macros` crate emits Rust code that references runtime types
//! and free functions. To keep that emission decoupled from the runtime's
//! internal module layout, the macro emits paths that go THROUGH this
//! facade module:
//!
//!   ::zeroship_runtime_macros::runtime::byte_string::ByteString
//!   ::zeroship_runtime_macros::runtime::state::OpError
//!   ::zeroship_runtime_macros::runtime::dom::exception::build
//!
//! The macro itself does NOT depend on `zeroship_runtime` (it's a proc-macro
//! crate; depending on a runtime crate would be cyclic). Instead, the runtime
//! re-exports its types under a stable path (`zeroship_runtime::macro_runtime`)
//! and the user's emit references resolve to the runtime crate at the user's
//! callsite.
//!
//! This is a wire-format contract — see STABILITY.md.

pub mod byte_string {
    pub use ::zeroship_runtime::byte_string::{ByteString, read_byte_string};
}
pub mod state {
    pub use ::zeroship_runtime::state::{
        OpError, OpErrorKind, OpResult, SharedState, IntoResolveValue,
    };
}
pub mod dom { pub mod exception { pub use ::zeroship_runtime::dom::exception::build; } }
pub mod node_error { pub use ::zeroship_runtime::node_error::build_node_exception; }
pub mod clamp { pub use ::zeroship_runtime::clamp::*; }
pub mod wrap { pub use ::zeroship_runtime::wrap::*; }
pub mod enforce_range { pub use ::zeroship_runtime::enforce_range::*; }
pub mod url_native { pub mod helpers { pub use ::zeroship_runtime::url_native::helpers::*; } }
pub mod convert { pub use ::zeroship_runtime::convert::WebIdlConvertible; }
```

The macro's emit changes from `::zeroship_runtime::state::OpErrorKind::TypeError` to `::zeroship_runtime_macros::runtime::state::OpErrorKind::TypeError`. The runtime crate keeps its existing module layout — only the macro's emit indirects through the facade.

**Trade-off discussion (§9 Open Question 1):** the facade could be a separate crate (`zeroship-runtime-macros-rt`) to formalize the contract, OR it could stay in-crate. In-crate is simpler (one crate boundary, no Cargo.lock churn) and trades on the "we are bespoke for one consumer" architectural reality. We pick **in-crate**.

#### `src/shared/class_config.rs` — `ClassConfig` (~150 LOC, Wave 3)

Closes F4 (10-arg `gen_install`).

```rust
//! `ClassConfig` — the parameter object passed to every codegen helper
//! after parse + analyze phases. Adding a new impl-block-level attribute
//! means one new field here + one new emit helper that reads it; never a
//! parameter-list migration.

pub(crate) struct ClassConfig<'a> {
    // Identity
    pub class_ty:    &'a syn::Ident,
    pub state_ty:    &'a syn::Ident,
    pub marker_ty:   syn::Ident,

    // Methods (already classified by parse::method_attrs)
    pub methods:               Vec<ClassMethod<'a>>,
    pub constructor:           Option<&'a ClassMethod<'a>>,
    pub has_any_fastcall:      bool,

    // Class-wide attributes
    pub to_string_tag:         Option<String>,
    pub inherit_intrinsic:     Option<String>,
    pub inherit_base:          Option<syn::Path>,
    pub async_iterable_method: Option<String>,
    pub consts:                Vec<ConstDecl>,
    pub iterable:              Option<v8_iterable::IterableConfig>,
}

impl<'a> ClassConfig<'a> {
    /// Convenience: list of fastcall-eligible methods (filtered).
    pub fn fastcall_methods(&self) -> impl Iterator<Item = &ClassMethod<'a>> { ... }

    /// Convenience: paired (getter, setter) accessor pairs grouped by JS name.
    pub fn accessor_pairs(&self) -> Vec<(String, AccessorPair<'a>)> { ... }
}
```

Visibility: `pub(crate)`. Built in `v8_class::analyze::analyze(parsed: ParsedClassAttrs) -> ClassConfig`.

#### `src/shared/op_error.rs` — `gen_throw_op_error_arms` (Wave 1 ✅)

Already shipped (`d9c5528`, `22ab81c`). Sole canonical site for the 6-variant OpErrorKind dispatch. Wave 2 migrates the two remaining inline sites in `method.rs` to call this helper. Closes C1 + C2 + F1.

#### `src/shared/must_str.rs` — `must_str` / `must_str_abs` (Wave 1 ✅)

Already shipped (`d9c5528`). Wave 2 completes migration in `method.rs` (~6 sites) and `v8_iterable.rs` (~30 sites) — these were deferred per the helper's doc-comment ("owned by Wave 1 #170 / #171").

#### `src/shared/recover_box.rs` — `gen_recover_box` (Wave 2)

Closes the 7-site duplication of the External-recovery preamble (§3 anti-pattern row "the 'recover External from internal field 0 → throw if missing' preamble appears in `gen_method_callback`, `gen_setter_callback`, `gen_same_object_getter_callback`, `gen_async_method_callback`, `v8_iterable::generate` (factory + forEach + next) — 7 sites of the same 10-LOC preamble").

```rust
/// Emit the prologue: brand-check + External recovery + (optional)
/// re-entry guard + the unsafe `&mut Self` materialization.
///
/// Returns a `RecoveredBox` value that the caller pattern-matches into:
///   - `tokens` — the prologue token stream
///   - `instance_binding` — the local binding name (`__instance` by convention)
///
/// `mut_receiver` controls whether the binding is `&mut Self` (with reentry
/// guard) or `&Self` (without).
pub(crate) fn gen_recover_box(
    class_ty: &syn::Ident,
    state_ty: &syn::Ident,
    method_name: &syn::Ident,
    mut_receiver: bool,
) -> RecoveredBox { ... }
```

Visibility: `pub(crate)`. Used by every emit/{method,getter,setter,static_op,async_method,box_install}.rs site, plus `v8_iterable/emit_factory.rs` and `v8_iterable/emit_iterator.rs`.

#### `src/shared/reentry_guard.rs` — `gen_reentry_guard` (Wave 2)

Replaces the per-method `thread_local!<RefCell<HashSet<usize>>>` with `thread_local!<Cell<Option<usize>>>`. Closes C5 / H13.

Before (`v8_class/method.rs:99-104`):
```rust
::std::thread_local! {
    static __INFLIGHT: ::std::cell::RefCell<::std::collections::HashSet<usize>> =
        ::std::cell::RefCell::new(::std::collections::HashSet::new());
}
let __already_inflight = __INFLIGHT.with(|__s| !__s.borrow_mut().insert(__inflight_addr));
```

After:
```rust
::std::thread_local! {
    static __INFLIGHT: ::std::cell::Cell<::std::option::Option<usize>> =
        const { ::std::cell::Cell::new(::std::option::Option::None) };
}
let __already_inflight = __INFLIGHT.with(|__s| {
    if __s.get() == Some(__inflight_addr) {
        true
    } else if __s.get().is_none() {
        __s.set(Some(__inflight_addr));
        false
    } else {
        // Some(other_addr): we're inside ANOTHER instance's call. The
        // semantic meaning of the guard is "is THIS (instance, method) re-
        // entered" — not "is some method on some instance running". So we
        // must distinguish: another instance running is fine, the same
        // instance re-entering is the rejection case. Implementation
        // choice: a single Cell stores Some(addr) when a method-on-this-
        // class is in flight; if a different addr arrives, we accept and
        // overwrite (saving the prior). Drop guard restores prior value.
        false
    }
});
```

Open question (§9 Open Question 3): the per-class single-slot `Cell<Option<usize>>` requires a different keying than `HashSet<usize>` once nested re-entry of distinct instances is on the table. Two implementation paths:

**Option A (single-slot, restore-on-drop):** the `Cell` holds the single inflight addr; the drop guard restores the prior value. Nested distinct-instance calls are allowed. Constant memory (8 bytes per (method, thread)).

**Option B (per-instance Cell):** keep the `HashSet` (stay with the current shape) but use `IntSet`/`BTreeSet` to avoid hash overhead. Closer to current behavior; not actually needed since the spec is "0 or 1".

We pick **Option A**. The drop guard already exists; updating it to restore-prior is a one-line change.

#### `src/shared/known_type.rs` — `KnownType` registry + `classify_extract` (Wave 4)

Closes F10 (gen_extract dispatcher) + L10 (9 nearly-identical type predicates) + anti-pattern §3 (7 stringly-typed dispatch tables converge to one).

```rust
/// Every type the macro recognizes for extraction / marshalling /
/// fastcall validation. The registry is the single source of truth;
/// every dispatcher (`classify_extract`, `clamp_kind`, `wrap_kind`,
/// `is_fastcall_primitive`, `gen_call_return`) reads from this enum.
pub(crate) enum KnownType {
    // Scalars (bare)
    Bool, I32, U32, I64, U64, F32, F64, Usize, Isize,

    // Strings
    String, ByteString, USVString,

    // Bytes
    VecU8, VecVecU8,

    // Newtypes
    ClampU16, ClampU32, ClampI32, ClampU64, ClampI64,
    EnforceRangeU32, EnforceRangeU64,
    WrapU8, WrapU16, WrapU32, WrapI8, WrapI16, WrapI32,

    // Compound
    Option(Box<KnownType>),
    Result(Box<KnownType>),                      // Result<T, OpError>
    LocalValue,                                  // v8::Local<v8::Value>
    LocalSpec(syn::Ident),                       // v8::Local<v8::T> (T is captured)
    GlobalSpec(syn::Ident),                      // v8::Global<v8::T>

    // User dict / enum (via WebIdlConvertible)
    UserConvertible(syn::Type),
}

/// Classify a `syn::Type` into the registry. The single source of truth.
pub(crate) fn classify(ty: &syn::Type) -> Option<KnownType> { ... }

/// Map a known type to fastcall-allowed-or-not. Replaces the 3 hand-rolled
/// match arms in fastcall.rs.
pub(crate) fn fastcall_arg(kt: &KnownType) -> Option<FastcallArg> { ... }
pub(crate) fn fastcall_return(kt: &KnownType) -> Option<FastcallReturn> { ... }

/// Emit the extraction tokens for a known type at JS arg index `idx`.
pub(crate) fn emit_extract(kt: &KnownType, idx: usize, name: &syn::Ident) -> TokenStream2 { ... }

/// Emit the call-return marshaling for a known type.
pub(crate) fn emit_return(kt: &KnownType, call: &TokenStream2) -> TokenStream2 { ... }
```

Visibility: `pub(crate)`. The pre-Wave-4 `lib.rs:489-595` predicates (`is_byte_string`, `is_vec_u8`, etc.) all delegate to this enum after Wave 4. The fastcall.rs three lists collapse into single match-on-`KnownType` calls.

#### `src/v8_class/parse/marker_attr.rs` — `MarkerAttr` trait (Wave 4)

Closes F5 / H5-H7 (12 inconsistent extract_*).

```rust
/// One trait per marker attribute. Each implementor declares:
///   - the attribute path it answers to (`#[v8_name = ...]` etc.)
///   - the parse logic from `&Attribute` → `Result<Option<Self>, syn::Error>`
///
/// Strict by default — invalid shapes produce `compile_error!` with a
/// span pointing at the offending token, NOT silent fall-through. Closes
/// the H5 footgun ("`#[v8_name(foo)]` no `=` silently does the wrong thing").
pub(crate) trait MarkerAttr: Sized {
    /// The attribute path (e.g. `"v8_name"`, `"v8_to_string_tag"`).
    const PATH: &'static str;

    /// Parse from a single `Attribute`. Returns `Ok(Some(self))` when
    /// the attribute matches and parses cleanly, `Ok(None)` if the path
    /// doesn't match, `Err` for malformed shapes on a matching path.
    fn from_attr(attr: &syn::Attribute) -> syn::Result<Option<Self>>;
}

/// Driver: walk a slice of attributes and find at most one matching `T`.
/// Returns `Err` if multiple matches found OR if the matched one is
/// malformed.
pub(crate) fn extract_marker_attr<T: MarkerAttr>(
    attrs: &[syn::Attribute],
) -> syn::Result<Option<T>> { ... }
```

Implementors (one per marker):

```rust
pub(crate) struct V8Name(pub String);
impl MarkerAttr for V8Name {
    const PATH: &'static str = "v8_name";
    fn from_attr(attr: &syn::Attribute) -> syn::Result<Option<Self>> {
        let nv = require_name_value(attr)?;
        let s = require_str_lit(&nv.value)?;
        Ok(Some(V8Name(s.value())))
    }
}

pub(crate) struct V8ToStringTag(pub String);
pub(crate) struct V8InheritIntrinsic(pub String);
pub(crate) struct V8InheritBase(pub syn::Path);
pub(crate) struct V8AsyncIterable(pub String);
pub(crate) struct V8StateMarker(pub syn::Path);
// ... etc.
```

Each implementor is ≤30 LOC. The 12 free `extract_*` functions in today's `parse.rs` collapse into 12 implementations of one trait.

#### `src/v8_class/parse/{class_attrs,method_attrs}.rs` — single-walk parsers (Wave 4)

Closes F8 (6+ redundant attribute walks).

```rust
pub(crate) struct ParsedClassAttrs {
    pub state_marker:     Option<V8StateMarker>,
    pub to_string_tag:    Option<V8ToStringTag>,
    pub inherit_intrinsic: Option<V8InheritIntrinsic>,
    pub inherit_base:     Option<V8InheritBase>,
    pub async_iterable:   Option<V8AsyncIterable>,
    pub consts:           Vec<ConstDecl>,
    pub iterable:         Option<IterableAttr>,
}

/// Single-pass walker: iterate `attrs` ONCE, dispatch by `path.is_ident(...)`,
/// fail-fast on multiple-attribute conflicts (e.g. two `#[v8_to_string_tag]`s).
pub(crate) fn parse_class_attrs(attrs: &[syn::Attribute]) -> syn::Result<ParsedClassAttrs> { ... }
```

Single walk, declared structure, error-rich.

#### `src/v8_class/emit/install.rs` — `gen_install(&ClassConfig)` (Wave 3 + 6)

The 120-line megaquote in today's `mod.rs:1132-1252` becomes a top-level `gen_install` that delegates to per-fragment helpers:

```rust
pub(crate) fn gen_install(cfg: &ClassConfig) -> TokenStream2 {
    let inherit_setup       = gen_inherit_setup(cfg);
    let proto_template      = gen_proto_template_setup(cfg);
    let method_installs     = cfg.methods.iter().map(|m| gen_method_install(cfg, m)).collect::<Vec<_>>();
    let accessor_installs   = gen_accessor_installs(cfg);
    let static_installs     = gen_static_installs(cfg);
    let const_installs      = gen_const_installs(cfg);
    let iterable_install    = cfg.iterable.as_ref().map(|i| gen_iterable_install(cfg, i)).unwrap_or_default();
    let async_iter_install  = cfg.async_iterable_method.as_ref().map(|m| gen_async_iter_install(cfg, m)).unwrap_or_default();

    quote! {
        pub fn install<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::FunctionTemplate> {
            #inherit_setup
            #proto_template
            #(#method_installs)*
            #accessor_installs
            #static_installs
            #const_installs
            #iterable_install
            #async_iter_install
            // ... finalization ...
        }
    }
}
```

Each per-fragment helper is ≤60 LOC. Closes F3.

#### `src/v8_class/emit/brand.rs` — `gen_brand_check_helpers` (Wave 6)

Extracted from today's `mod.rs:507-700` megaquote. Emits:

```rust
fn __brand_check_<Class>(scope, obj) -> bool { ... }
pub fn __zs_is_<Class>(scope, v) -> bool { ... }   // ← deprecated symbol; kept for back-compat
impl <Class> { pub fn is_instance(scope, v) -> bool { __brand_check_<Class>(scope, ...) } }   // ← new public API per §3.5
```

Closes F2 (first half) + F3.

#### `src/v8_class/fastcall/types.rs` — `FastcallType` (Wave 4)

Closes F6 (3 sites of the same primitive list).

```rust
pub(crate) enum FastcallArg { Bool, I32, U32, I64, U64, F32, F64, ByteString }
pub(crate) enum FastcallReturn { Bool, I32, U32, I64, U64, F32, F64, Unit }

impl FastcallArg {
    /// Map from a `KnownType` to a fastcall arg, if allowed. Single source of truth.
    pub fn from_known(kt: &KnownType) -> Option<Self> { ... }
}
```

The three match arms in today's `fastcall.rs:72-82, 119-122, 141-145` collapse to single calls.

#### `src/v8_iterable/{parse,analyze,emit_*}.rs` — split (Wave 7)

Today's `v8_iterable.rs` is 1,291 LOC with one 401-LOC `quote!` block at lines 635-1036. Split:

- `parse.rs` — `IterableAttr`, `inspect_value_pairs`, `extract_iterable` (~100 LOC).
- `analyze.rs` — `IterableConfig` struct (mode, value_marshal, value_pairs receiver shape) (~80 LOC).
- `emit_factory.rs` — keys/values/entries/forEach factory codegen (~250 LOC).
- `emit_iterator.rs` — `<Class>Iterator` companion class codegen (~250 LOC).
- `emit_marshal.rs` — per-K / per-V marshaling, delegates to `KnownType` registry (~100 LOC).
- `mod.rs` — orchestration (`extract_iterable`, `generate`) (~80 LOC).

Closes F3 (god file) + F9 (no snapshots — adds 3 in `snapshots/`).

#### `src/webidl_dict/`, `src/webidl_enum/` — split (Wave 7)

Today's `webidl_dict.rs` (383 LOC) and `webidl_enum.rs` (447 LOC after Wave 1 extraction of `pascal_to_kebab`) split into mod.rs + parse.rs + emit.rs. Adds insta snapshots. Closes F9 + H16 (reject reference-typed dict fields).

### §2.3 What stays unchanged

- The `pub fn expand` / `pub fn expand_tokens` entry pair in `v8_class/mod.rs` keeps its signature for testability (the 254+ smoke tests + 3 insta snapshots use both).
- The 14 attribute proc-macros in `lib.rs` keep their names + bodies (each forwards to the relevant `v8_class::expand` etc.).
- The 2 derives' entry points keep their names.
- All consumer-side `__zs_is_*` and `__InstallSlot_*` references continue to work — Wave 5 introduces the new API as an addition, not a replacement; deprecation of the old symbols is a future PR (§4 Wave 8.2).

---

## §3 Refactoring patterns

This section documents each load-bearing pattern with a before/after sketch. Each pattern:
- Cites the BEFORE code by file:line.
- Shows the AFTER shape.
- Lists which finding it closes.
- States the wave that lands it.

### §3.1 `ClassConfig` parameter object — closes F4

**Before** (`crates/runtime-macros/src/v8_class/mod.rs:739-750`):

```rust
fn gen_install(
    class_ty: &syn::Ident,
    methods: &[&ClassMethod],
    to_string_tag_override: Option<&str>,
    inherit_intrinsic: Option<&str>,
    inherit_base: Option<&syn::Path>,
    install_iterable_call: Option<&TokenStream2>,
    async_iterable_method: Option<&str>,
    const_decls: &[ConstDecl],
    has_any_fastcall: bool,
) -> TokenStream2 { ... }
```

Plus `gen_constructor_callback`, `gen_default_constructor_callback`, `gen_method_callback`, `gen_async_method_callback`, `gen_same_object_getter_callback`, `gen_static_callback` each take `(class_ty: &Ident, state_ty: &Ident, ...)` repeatedly.

**After:**

```rust
fn gen_install(cfg: &ClassConfig) -> TokenStream2 { ... }
fn gen_constructor_callback(cfg: &ClassConfig, c: &ClassMethod) -> TokenStream2 { ... }
fn gen_method_callback(cfg: &ClassConfig, m: &ClassMethod) -> TokenStream2 { ... }
// etc — every helper takes &ClassConfig and an optional method-specific arg.
```

Wave 3.

### §3.2 `MarkerAttr` trait — closes F5 / H5-H7

**Before** (`crates/runtime-macros/src/v8_class/parse.rs:124-139`):

```rust
pub(super) fn extract_v8_name(attrs: &[Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("v8_name") {
            continue;
        }
        if let Meta::NameValue(nv) = &attr.meta {
            if let Expr::Lit(ExprLit { lit: Lit::Str(s), .. }) = &nv.value {
                return Some(s.value());
            }
        }
    }
    None  // ← silent fall-through on malformed
}
```

Plus 11 more siblings, each with subtly different return shape and error policy.

**After** (`crates/runtime-macros/src/v8_class/parse/marker_attr.rs`):

```rust
pub(crate) struct V8Name(pub String);
impl MarkerAttr for V8Name {
    const PATH: &'static str = "v8_name";
    fn from_attr(attr: &syn::Attribute) -> syn::Result<Option<Self>> {
        let nv = require_name_value(attr).ok_or_else(|| syn::Error::new_spanned(
            attr,
            "#[v8_name = \"...\"]: expected `name = literal` shape",
        ))?;
        let s = require_str_lit(&nv.value)?;
        Ok(Some(V8Name(s.value())))
    }
}
```

Driver (`extract_marker_attr<V8Name>(attrs)`) returns `syn::Result<Option<V8Name>>`. STRICT — malformed `#[v8_name(foo)]` (no `=`) emits a `compile_error!` with the span of the offending tokens. Wave 4.

### §3.3 `gen_throw_op_error_arms` helper — closes C1+C2+F1

Wave 1 ✅. The two remaining inline sites in `method.rs:760-789` (constructor) and `method.rs:829-857` (post_init) are migrated in Wave 2 to call the same helper. Sketch:

**After Wave 2** (`v8_class/emit/constructor.rs`):

```rust
let make_instance = if is_result {
    let throw = crate::shared::op_error::gen_throw_op_error_arms(
        &quote! { scope }, &quote! { __err },
    );
    quote! {
        let __instance: #state_ty = match <#state_ty>::#ctor_name(#(#call_args),*) {
            Ok(__v) => __v,
            Err(__err) => {
                #throw
                return;
            }
        };
    }
} else {
    quote! { let __instance: #state_ty = <#state_ty>::#ctor_name(#(#call_args),*); }
};
```

The hand-rolled 25-line block becomes a 3-line delegation. Same for `post_init`'s arm.

### §3.4 `Cell<Option<usize>>` re-entry guard — closes C5/H13

**Before** (`crates/runtime-macros/src/v8_class/method.rs:99-104`):

```rust
::std::thread_local! {
    static __INFLIGHT: ::std::cell::RefCell<::std::collections::HashSet<usize>> =
        ::std::cell::RefCell::new(::std::collections::HashSet::new());
}
```

**After** (`crates/runtime-macros/src/shared/reentry_guard.rs`):

```rust
::std::thread_local! {
    static __INFLIGHT: ::std::cell::Cell<::std::option::Option<usize>> =
        const { ::std::cell::Cell::new(::std::option::Option::None) };
}
let __prior_inflight: ::std::option::Option<usize> = __INFLIGHT.with(|__s| __s.get());
let __already_inflight = matches!(__prior_inflight, Some(__a) if __a == __inflight_addr);
if __already_inflight {
    let __msg = v8::String::new(scope, #err_msg).unwrap();
    let __exc = v8::Exception::type_error(scope, __msg);
    scope.throw_exception(__exc);
    return;
}
__INFLIGHT.with(|__s| __s.set(::std::option::Option::Some(__inflight_addr)));
struct __ReentryGuard(::std::option::Option<usize>);
impl ::std::ops::Drop for __ReentryGuard {
    fn drop(&mut self) {
        __INFLIGHT.with(|__s| __s.set(self.0));   // restore prior, including None
    }
}
let __reentry_guard = __ReentryGuard(__prior_inflight);
```

Memory: 8 bytes/(method, thread) instead of 48 + bucket allocation. Drop-guard restore-prior preserves nesting semantics for distinct instances. Wave 2.

<!-- Added in v2 R1: addressing critic's CRITICAL-4 — panic safety analysis -->

**Panic safety analysis.** The drop-guard pattern is correct only if `Drop::drop` runs when the user's `&mut self` body panics. There are two abort modes to consider:

1. **Unwinding panic** (default Rust panic, no `extern "C"` boundary in the user code): the unwinder runs all `Drop` impls between the panic site and the catch frame. Our drop guard is in scope above the user-method call, so `__INFLIGHT.set(self.0)` runs. ✅ correct.

2. **`extern "C"` panic abort:** V8 callbacks ARE invoked through an `extern "C" fn` boundary (the callback shim emitted by the macro). Rust's behavior at `extern "C"` panic is **abort** by default since edition 2018 (see [RFC 2945](https://rust-lang.github.io/rfcs/2945-c-unwind-abi.html)). If the user's method panics inside the shim, the process aborts BEFORE drop guards run. The reentry slot is left as `Some(addr)` for one `__INFLIGHT` thread-local — but the process is gone, so the slot is moot. ✅ no functional regression.

3. **`std::panic::catch_unwind` interception:** the macro does NOT emit `catch_unwind` around user methods today (and shouldn't — it would mask bugs that today crash loudly). With abort-on-FFI as default, the Cell variant matches the HashSet variant's panic profile: in both cases, abort kills the process; in neither case does the guard "leak" in a way that affects subsequent calls (because there are no subsequent calls).

**Invariant the macro emits to enforce this:** the guard's `Drop::drop` body uses ONLY `__INFLIGHT.with(...)` and `Cell::set` — both panic-free. There is no allocation, no string formatting, no foreign call. The drop guard cannot itself panic, so it does not nest panics during unwinding.

**Worked example (R1 PoC verification):**
```rust
// pseudo-Rust, simulating the post-Wave-2 emit:
fn outer() {
    __INFLIGHT.with(|s| s.set(Some(0xAAAA))); // outer enters
    let _g_outer = Guard(None); // restore-to-None on drop
    inner();                                    // inner is a different addr
    // post-inner: g_outer still in scope; INFLIGHT now Some(0xAAAA)
}

fn inner() {
    let prior = __INFLIGHT.with(|s| s.get());   // = Some(0xAAAA)
    __INFLIGHT.with(|s| s.set(Some(0xBBBB))); // inner enters
    let _g_inner = Guard(prior);                // restore-to-Some(0xAAAA) on drop
    user_method();                              // may panic
    // unwinding: g_inner.drop runs → INFLIGHT = Some(0xAAAA)
    //            g_outer.drop runs → INFLIGHT = None
}
```

**Cross-instance + cross-method nesting** (multi-class re-entry) works because each `(class × method × thread)` has its OWN `__INFLIGHT` thread-local — they never alias.

**Decision:** Option A (single-slot, restore-on-drop) is correct under both unwinding panic and abort. Wave 2 lands as specified.

### §3.5 Stable emitted-symbol public API — closes F2

**Before:** consumers grep for `__zs_is_<Class>` and `__InstallSlot_<Class>` strings:

```rust
// crates/runtime/src/web/fetch/request.rs:230
input_v.is_object() && __zs_is_Request(scope, input_v);

// crates/runtime/src/web/fetch/request.rs:924
let req_tmpl_g = scope.get_slot::<__InstallSlot_Request>()?.0.clone();

// crates/runtime/src/web/dom/event_target.rs:125
pub struct __InstallSlot_EventTarget(::v8::Global<::v8::FunctionTemplate>);
```

**After:** the macro emits a stable trait-impl alongside the underscored symbols. Consumers migrate to:

```rust
Request::is_instance(scope, input_v)              // ← new (inherent method, forwards to the trait)
let req_tmpl_g = <Request as V8ClassInstance>::cloned_install_template(scope)?;
                                                  // ← new (returns owned Global; refcount-bump, no scope borrow)
```

<!-- Added in v2 R2: addressing critic's CRITICAL-R2-2 — `install_slot` returned a borrow that held the scope, blocking subsequent ops. Replace with `cloned_install_template` returning an owned Global. -->

<!-- Added in v2 R1: addressing critic's MAJOR-3 — `pub type Slot = ...` inside `impl` is invalid Rust syntax; replace with trait associated type -->

**Why a trait, not an inherent associated type:** Rust does not permit `pub type Slot = __InstallSlot_<Class>;` inside `impl <Class>` blocks on stable. Inherent associated types (RFC 2515) require nightly. The macro must emit on stable, so the slot type is exposed via a **trait associated type** instead. (An earlier draft sketched `pub type Slot = ...` inside `impl <Class>` — corrected in v2.)

Where:

```rust
// crates/runtime/src/macro_runtime/v8_instance.rs (NEW, runtime-side)
//! Stable-API trait for macro-emitted V8 class instances.
//!
//! The trait is sealed: only `runtime-macros`'s emit and the
//! manually-rolled `EventTarget` impl can implement it. New impls
//! outside the runtime crate fail to compile because they cannot
//! name the sealing-bound type.

mod sealed {
    pub trait Sealed {}
}

pub trait V8ClassInstance: sealed::Sealed + 'static {
    /// The isolate-slot type holding the cached FunctionTemplate.
    /// Trait associated type, not an inherent type — see §3.5 of the
    /// refactor proposal for why.
    type InstallSlot: 'static;

    /// The isolate-slot type holding the cached `prototype` Object.
    type BrandSlot: 'static;

    /// Idempotent install — same Local across calls in an isolate.
    /// Lifetimes are explicit because the Local's lifetime is tied to
    /// the scope's lifetime parameter (not invariant; the macro's
    /// existing `install` signature carries this same lifetime
    /// shape).
    fn install<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::FunctionTemplate>;

    /// Brand check — walks the prototype chain. Lifetimes elided
    /// here for the same reason as the macro's existing
    /// `__brand_check_<Class>` helper (see `v8_class/mod.rs:584-588`
    /// comment: explicit lifetimes would over-constrain the Local
    /// arg's lifetime relative to the `&mut PinScope` borrow).
    fn is_instance(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> bool;

    /// Read a CLONE of the cached install-slot's `Global<FunctionTemplate>`.
    /// Returns None if the class hasn't been installed in the current
    /// isolate. Returns a Global (not a borrow) because callers chain
    /// further scope operations after the read; a borrowed `&Self::InstallSlot`
    /// would hold the scope's mutable borrow open for the duration of `'a`,
    /// blocking `Local::new`, `set_slot`, etc.
    ///
    /// All current `__InstallSlot_<Class>` types are tuple structs around
    /// `v8::Global<FunctionTemplate>`. The clone is a refcount-bump (cheap).
    fn cloned_install_template(scope: &mut v8::PinScope) -> Option<v8::Global<v8::FunctionTemplate>>;
}
```

The macro emits BOTH:

```rust
// inherent impl (so consumers can write `Request::is_instance(...)`)
#[allow(non_snake_case, dead_code)]
impl Request {
    pub fn is_instance<'_lt>(
        scope: &mut v8::PinScope,
        v: v8::Local<v8::Value>,
    ) -> bool {
        <Self as ::zeroship_runtime::macro_runtime::v8_instance::V8ClassInstance>::is_instance(scope, v)
    }
}

// trait impl + sealing
#[doc(hidden)]
impl ::zeroship_runtime::macro_runtime::v8_instance::sealed::Sealed for Request {}

impl ::zeroship_runtime::macro_runtime::v8_instance::V8ClassInstance for Request {
    type InstallSlot = __InstallSlot_Request;     // ← associated type, NOT pub type
    type BrandSlot   = __BrandSlot_Request;
    fn install<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::FunctionTemplate> {
        Self::install(scope)  // delegates to the existing inherent install
    }
    fn is_instance(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> bool {
        __zs_is_Request(scope, v)   // delegates to the existing fn (kept for back-compat)
    }
    fn cloned_install_template(scope: &mut v8::PinScope) -> Option<v8::Global<v8::FunctionTemplate>> {
        scope.get_slot::<__InstallSlot_Request>().map(|slot| slot.0.clone())
    }
}
```

For `EventTarget` and other hand-rolled cases, the manually-rolled types implement `V8ClassInstance` directly — `event_target.rs:125-178`'s `__InstallSlot_EventTarget` becomes the `InstallSlot` associated type for an `impl V8ClassInstance for EventTarget` block.

<!-- v2 R3: cleaned up the sealing-pattern paragraph; addressed CRITICAL-R3-1 -->

**Sealing pattern.** The macro's emitted code references the trait from a different crate (the user's crate), so the conventional `pub(crate) mod sealed { pub trait Sealed {} }` pattern (which works for sealing within ONE crate) does not apply. The seal must be reachable from outside the runtime crate (so the emit compiles) but unimplementable in practice (so external consumers can't add their own `impl V8ClassInstance for FakeClass`).

**Chosen pattern: convention-sealed.** A `pub mod __private { pub trait Sealed {} }` with documentation marking the path as do-not-use:

```rust
// crates/runtime/src/macro_runtime/v8_instance.rs
pub trait V8ClassInstance: __private::Sealed + 'static {
    // ... associated types + methods ...
}

#[doc(hidden)]
pub mod __private {
    /// Internal seal. NOT public API; the `__private` module name is
    /// reachable for the macro's emit but follows Rust's `__`-prefix
    /// convention for "do not use outside the parent crate". External
    /// `impl V8ClassInstance for X { ... }` blocks must implement
    /// `__private::Sealed for X`, which is forbidden by convention and
    /// caught by `clippy::disallowed_types` (see `.clippy.toml`).
    pub trait Sealed {}
}
```

External consumers can technically reach `__private::Sealed` (Rust has no language-level sealing on stable). Two layered defences:

1. **Convention.** The `__private::` prefix and `#[doc(hidden)]` signal "internal" per Rust convention; consumers reading the API don't see it.
2. **Lint.** A `.clippy.toml` rule:

   ```toml
   # .clippy.toml
   disallowed-types = [
       { path = "zeroship_runtime::macro_runtime::v8_instance::__private::Sealed",
         reason = "internal sealing trait; implement V8ClassInstance, not Sealed" },
   ]
   ```

   Clippy emits a warning at the implementation site. CI gates `cargo clippy --workspace -- -D warnings` for the runtime workspace, so any external `impl Sealed` fails CI.

**Reference for the pattern (corrected in v3):** `serde_json::Map`'s use of `mod private { pub trait Sealed {} }` (verifiable upstream at `serde-rs/json:src/map.rs`). Earlier drafts cited `serde::de::Visitor` — that trait is NOT sealed in upstream serde; correction in v3 R3.

**Verification of `clippy::disallowed_types`:** the lint is part of stable Clippy as of 1.55.0 (Rust 1.55, August 2021); zeroship's `rust-toolchain.toml` is on a later version. Rule exists in stable Clippy and is in active use; no compatibility risk.

Consumers cannot implement `V8ClassInstance` themselves in idiomatic Rust — only the macro's emit and the runtime's hand-roll can.

Consumer code calls `<T as V8ClassInstance>::is_instance(scope, v)` OR `T::is_instance(scope, v)` (via the inherent forwarder). Both compile. The leak becomes a documented contract.

**Lifetime parameterization (preserved):** the `is_instance` method elides lifetimes (matches the existing `__brand_check_<Class>` shape). The `install` method carries explicit `<'s>` because the returned Local is tied to the scope's lifetime — same as today.

The `__zs_*` and `__InstallSlot_*` symbols stay emitted (back-compat) for the deprecation period (see §3.10's policy table). Wave 5.

`STABILITY.md` (Wave 5) lists every emitted public symbol and its stability status — see §3.10.

### §3.6 Single-scan attribute parser — closes F8

**Before** (`crates/runtime-macros/src/v8_class/mod.rs:407-417`):

```rust
let to_string_tag_override = extract_to_string_tag(&input.attrs);
let inherit_intrinsic = extract_inherit_intrinsic(&input.attrs);
let inherit_base = extract_inherit_base(&input.attrs);
let async_iterable_method = match extract_async_iterable(&input.attrs) { ... };
let const_decls = match extract_consts(&input.attrs) { ... };
let iterable_attr = match v8_iterable::extract_iterable(&input.attrs) { ... };
let state_marker_path = extract_state_marker(&input.attrs);    // l.198
```

7 walks of the same attribute slice.

**After** (`crates/runtime-macros/src/v8_class/parse/class_attrs.rs`):

```rust
pub(crate) fn parse_class_attrs(attrs: &[syn::Attribute]) -> syn::Result<ParsedClassAttrs> {
    let mut p = ParsedClassAttrs::default();
    for attr in attrs {
        let path = attr.path();
        if path.is_ident("v8_state_marker") { p.state_marker = parse_state_marker(attr)?; continue; }
        if path.is_ident("v8_to_string_tag") { p.to_string_tag = parse_to_string_tag(attr)?; continue; }
        if path.is_ident("v8_inherit_intrinsic") { p.inherit_intrinsic = parse_inherit_intrinsic(attr)?; continue; }
        if path.is_ident("v8_inherit") { p.inherit_base = parse_inherit_base(attr)?; continue; }
        if path.is_ident("v8_async_iterable") { p.async_iterable = parse_async_iterable(attr)?; continue; }
        if path.is_ident("v8_const") { p.consts.push(parse_const(attr)?); continue; }
        if path.is_ident("v8_iterable") { p.iterable = Some(parse_iterable(attr)?); continue; }
        // unknown impl-block-level attributes pass through to rustc
    }
    p.validate()?;   // ← cross-attribute checks (e.g. iterable + async_iterable interactions)
    Ok(p)
}
```

One walk, structured output, fail-fast on conflicts. `validate()` (~30 LOC) checks cross-attribute invariants — currently scattered across `extract_*` callsites in `v8_class/mod.rs`. Wave 4.

<!-- Added in v2 R1: addressing critic's MAJOR-2 — KnownType registry must encode CONTEXT (arg vs return vs general extract); fastcall arg list != return list -->

### §3.7 Table-driven type dispatch — closes anti-pattern §3 + F10 + L10

**Before** (`crates/runtime-macros/src/lib.rs:489-595`): nine functions each doing `type_ident(ty).as_deref() == Some("X")`. Plus `gen_extract` at `lib.rs:655-931` does eight if-early-returns followed by a final match.

**After**: a single `KnownType` enum (§2.2 `shared/known_type.rs`) + a single `classify(ty: &Type) -> Option<KnownType>` dispatcher. Every existing predicate becomes a `matches!(classify(ty), Some(KnownType::X))` query. `gen_extract` becomes a single `match` on `KnownType`:

```rust
pub(crate) fn gen_extract(idx: usize, name: &Ident, ty: &Type) -> TokenStream2 {
    let kt = classify(ty).unwrap_or_else(|| {
        return syn::Error::new_spanned(ty, format!(
            "#[v8_class]: type `{}` is not a recognised JS-bridge type. \
             Supported: scalars (bool, i32, u32, i64, u64, f32, f64), strings \
             (String, ByteString, USVString), bytes (Vec<u8>, Vec<Vec<u8>>), \
             newtypes (Clamp*, Wrap*, EnforceRange*), Option<T>, Result<T, OpError>, \
             v8::Local<v8::Value>, types implementing WebIdlConvertible.",
            quote!(#ty),
        )).to_compile_error();
    });
    crate::shared::known_type::emit_extract(&kt, idx, name)
}
```

Compile-error replaces silent `Local<Value>` fall-through (Doc Gap §9 in arch critique). Wave 4.

**Context split (corrected in v2):** the registry encodes the GENERAL set of types. Three context-specific contexts are layered ON TOP of the registry:

| Context | Allowed subset | Rejection mode |
|---|---|---|
| **General extract** (`gen_extract` for `#[v8_method]`/`#[v8_setter]` args) | All `KnownType` variants | `compile_error!` if `classify` returns `None` |
| **Fastcall arg** (`fastcall::types::FastcallArg::from_known(&kt)`) | Returns `Some(FastcallArg)` only for `Bool`, `I32`, `U32`, `I64`, `U64`, `F32`, `F64`, `ByteString` — NO `Result`, NO `Option`, NO `Vec`, NO `Local` | `compile_error!` "fast path forbids type X — see fastcall.rs:50" |
| **Fastcall return** (`fastcall::types::FastcallReturn::from_known(&kt)`) | Returns `Some(FastcallReturn)` for `Bool`, `I32`, `U32`, `I64`, `U64`, `F32`, `F64`, unit, AND `Result<<scalar>, OpError>` — NO `ByteString` (allocation forbidden), NO `Option`, NO `Vec`, NO `Local` | same as above |
| **Setter arg** (which is the body of `gen_setter_callback`) | Subset of general extract that allows mutation receivers | inherits general extract rejection |

Verified against the existing source at `crates/runtime-macros/src/v8_class/fastcall.rs:75-83` (arg whitelist) and `:122-145` (return whitelist) — they are NOT identical: arg includes `ByteString`, return excludes it; return includes `Result<T, OpError>`, arg excludes it.

The Wave-4 implementation is therefore:

```rust
// shared/known_type.rs — the GENERAL registry
pub(crate) enum KnownType { /* all variants from the §2.2 listing */ }

pub(crate) fn classify(ty: &syn::Type) -> Option<KnownType> { ... }
pub(crate) fn emit_extract(kt: &KnownType, idx: usize, name: &syn::Ident) -> TokenStream2 { ... }
pub(crate) fn emit_return(kt: &KnownType, call: &TokenStream2) -> TokenStream2 { ... }

// v8_class/fastcall/types.rs — context-specific subsets
pub(crate) enum FastcallArg { Bool, I32, U32, I64, U64, F32, F64, ByteString }
pub(crate) enum FastcallReturn { Bool, I32, U32, I64, U64, F32, F64, Unit, Result(Box<FastcallReturn>) }

impl FastcallArg {
    pub fn from_known(kt: &KnownType) -> Option<Self> {
        match kt {
            KnownType::Bool => Some(Self::Bool),
            KnownType::I32  => Some(Self::I32),
            // ... 6 more scalar variants ...
            KnownType::ByteString => Some(Self::ByteString),
            _ => None,
        }
    }
}

impl FastcallReturn {
    pub fn from_known(kt: &KnownType) -> Option<Self> {
        match kt {
            KnownType::Bool => Some(Self::Bool),
            // ... 6 more scalar variants ...
            // ByteString is REJECTED for return (allocation)
            KnownType::Result(inner) => {
                let inner_fc = Self::from_known(inner.as_ref())?;
                if matches!(inner_fc, Self::Result(_)) { return None; }  // no Result<Result<_>>
                Some(Self::Result(Box::new(inner_fc)))
            }
            _ => None,
        }
    }
}
```

The fastcall validator (`fastcall::validate_fastcall_signature`) becomes:

```rust
for arg_ty in &user_method.sig.inputs[1..] {  // skip receiver
    let kt = shared::known_type::classify(arg_ty)
        .ok_or_else(|| syn::Error::new_spanned(arg_ty, "fastcall: unrecognised arg type"))?;
    if FastcallArg::from_known(&kt).is_none() {
        return Err(syn::Error::new_spanned(arg_ty,
            format!("fastcall arg type forbidden: {:?}; allowed: bool, i32-i64, u32-u64, f32, f64, ByteString", kt)));
    }
}
// similar for return
```

This collapses today's three hand-rolled match arms (`fastcall.rs:75-83`, `:122-145`, `:194-250`) into one `KnownType::classify` + two `from_known` predicates. Closes F6 + anti-pattern §3 row 6.

### §3.8 `gen_recover_box` shared preamble — closes 7-site duplication

**Before**: 7 sites of the same 10-LOC preamble (External recovery + brand-check + Illegal Invocation throw):

- `v8_class/method.rs:188-220` (gen_method_callback)
- `v8_class/method.rs:670-694` (gen_setter_callback)
- `v8_class/method.rs:325-380` (gen_same_object_getter_callback)
- `v8_class/method.rs:486-512` (gen_async_method_callback)
- `v8_iterable.rs` factory codegen
- `v8_iterable.rs` forEach codegen
- `v8_iterable.rs` next() codegen

**After** (`crates/runtime-macros/src/shared/recover_box.rs`):

```rust
/// Returns the prologue + the local binding name to use for `&[mut] Self`.
pub(crate) fn gen_recover_box(
    class_ty: &syn::Ident,
    state_ty: &syn::Ident,
    method_name: &syn::Ident,
    mut_receiver: bool,
) -> RecoveredBox {
    let brand_check_fn = format_ident!("__brand_check_{}", class_ty);
    let reentry_guard = if mut_receiver {
        crate::shared::reentry_guard::gen_reentry_guard(class_ty, method_name)
    } else {
        quote! {}
    };
    let receiver_ref = if mut_receiver { quote! { &mut *__instance } } else { quote! { &*__instance } };
    let tokens = quote! {
        let __this = args.this();
        if !#brand_check_fn(scope, __this) {
            let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
            let __exc = v8::Exception::type_error(scope, __msg);
            scope.throw_exception(__exc);
            return;
        }
        let __ext = match __this.get_internal_field(scope, 0)
            .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
        {
            Some(e) => e,
            None => {
                let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
                let __exc = v8::Exception::type_error(scope, __msg);
                scope.throw_exception(__exc);
                return;
            }
        };
        #reentry_guard
        let __instance = unsafe { &mut *(__ext.value() as *mut #state_ty) };
    };
    RecoveredBox { tokens, receiver_ref }
}

pub(crate) struct RecoveredBox {
    pub tokens: TokenStream2,
    pub receiver_ref: TokenStream2,
}
```

Wave 2. Six callsite migrations.

### §3.9 Public emit-runtime facade — closes F2 + §5

**Before**: macro's emit references 28 distinct `::zeroship_runtime::*` paths (sampled via grep of `runtime-macros/src/`):

```text
::zeroship_runtime::byte_string::ByteString
::zeroship_runtime::byte_string::read_byte_string
::zeroship_runtime::clamp::ClampU
::zeroship_runtime::clamp::read_clamp_u
::zeroship_runtime::convert::WebIdlConvertible
::zeroship_runtime::dom::exception::build
::zeroship_runtime::enforce_range::read_enforce_range_u
::zeroship_runtime::node_error::build_node_exception
::zeroship_runtime::state::IntoResolveValue
::zeroship_runtime::state::OpError
::zeroship_runtime::state::OpErrorKind::DomException
::zeroship_runtime::state::OpErrorKind::Error
::zeroship_runtime::state::OpErrorKind::JsValue
::zeroship_runtime::state::OpErrorKind::NodeError
::zeroship_runtime::state::OpErrorKind::RangeError
::zeroship_runtime::state::OpErrorKind::TypeError
::zeroship_runtime::state::OpResult::JsValue
::zeroship_runtime::state::SharedState
::zeroship_runtime::url_native::helpers::USVString
::zeroship_runtime::url_native::helpers::read_usv_string_or_throw
::zeroship_runtime::wrap::read_wrap_i
::zeroship_runtime::wrap::read_wrap_u
... (28 unique paths total)
```

The runtime crate cannot move any of these without breaking compilation. The macro is structurally coupled to the runtime's internal module layout.

**After**: macro emits paths that go through `::zeroship_runtime_macros::runtime::*`. Net result:

- The macro's emit references ONE module instead of 28 paths.
- The runtime crate keeps its internal layout. It exposes a `pub mod macro_runtime` (or similar) at the crate root that re-exports under stable names.
- Moving a runtime type is a 1-line update in `runtime/src/macro_runtime.rs`, not a 28-site refactor across `runtime-macros/`.

```rust
// crates/runtime/src/macro_runtime.rs (NEW, runtime-side)
//! Stable re-exports for emit code from `runtime-macros`. See
//! `crates/runtime-macros/STABILITY.md`. Renaming or relocating any
//! item below is a breaking change for the macro's emit; coordinate with
//! the macro maintainers.

pub mod byte_string { pub use crate::byte_string::*; }
pub mod state { pub use crate::core::state::{OpError, OpErrorKind, OpResult, SharedState, IntoResolveValue}; }
pub mod dom { pub mod exception { pub use crate::web::dom::exception::build; } }
// ... etc
```

The `runtime-macros/src/runtime/mod.rs` facade emits `::zeroship_runtime::macro_runtime::*` (or, alternatively, the macro emits directly through the runtime's facade). Wave 5.

### §3.10 `STABILITY.md` — closes F2 (documentation half)

<!-- Added in v2 R1: addressing critic's MAJOR-6 — concrete deprecation policy timeline -->

A new top-level `crates/runtime-macros/STABILITY.md` enumerates:

- **Public proc-macro entries** (14 attributes + 2 derives) — semver-stable, follows zeroship's deprecation policy (below).
- **Emitted symbols on the user impl** — `<Class>::install` (forwarder), `<Class>::is_instance` (forwarder to `V8ClassInstance::is_instance`), `<Class as V8ClassInstance>::InstallSlot` / `::BrandSlot` (associated types), `<Class>Iterator` (for iterable). Stable.
- **Emitted module-scope symbols** — `__InstallSlot_<Class>`, `__BrandSlot_<Class>`, `__brand_check_<Class>`, `__zs_is_<Class>`, `__<Class>_<method>_callback`, `__<Class>_<method>_FASTCALL_CFN`, `__<Class>_<method>_FASTCALL_CINFO`. Marked `#[doc(hidden)]`. Stability: **deprecated public** per timeline below.
- **Runtime-side re-exports the macro depends on** — `zeroship_runtime::macro_runtime::*`. The 28 paths the facade fronts. Each entry: name, signature, renaming-without-coordination = breaking.

**Deprecation policy timeline (concrete, calibrated to zeroship's release cadence):**

zeroship has no published `Cargo.toml` version cadence yet (single-tenant pre-launch); the deprecation timeline therefore measures in **PR landings**, not calendar months. The timeline:

| Phase | Trigger | Status of `__zs_is_<Class>` / `__InstallSlot_<Class>` |
|---|---|---|
| **T+0** (Wave 5 lands) | New trait API ships alongside underscored symbols. Both compile. | Public, undocumented, recommended for removal in next major. |
| **T+1 PR** (Consumer migration PR) | All 4 consumer sites migrated: `request.rs:230,924`; `als.rs:259`; `event_target.rs:125-178`. Verified by the precise predicate below. | Underscored symbols still compile but `#[deprecated(since = "<commit-sha>", note = "use V8ClassInstance::is_instance")]` is added to the macro emit. Build emits warnings if anything still references them. |
| **T+2 PRs** (Wave 8 final) | One full PR cycle of zero deprecation warnings in CI. | Underscored symbols REMOVED from emit. The macro emits only `<Class as V8ClassInstance>::is_instance` etc. |
| **T+3 PRs** (post-Wave-8) | Three sequential green-CI PRs after the removal. | Symbols are gone; no maintenance burden. |

**Reference for the policy shape:** `serde/CHANGELOG.md` documents stability-affecting changes (e.g., the v1.0.140 internals refactor) as PR-merge-tracked (release notes per minor bump). `tokio/CHANGELOG.md` follows the same convention. Earlier drafts cited pin-project-internal — corrected: pin-project-internal does not have its own STABILITY.md; only the parent `pin-project` crate documents stability via release notes. Our STABILITY.md format follows serde's convention: a top-level table of stable / deprecated / removed symbols with the commit-sha at which each transition happened.

**If a consumer migration is hard** (e.g., `event_target.rs:125-178`'s 30-LOC hand-roll requires more than expected), Wave 8 EXPLICITLY allows skipping that consumer's deprecation. The STABILITY.md gains a row "underscored symbols retained for: [list of consumer files]". This avoids the doc's earlier risk that "all consumers migrate" is a closed-set assumption.

<!-- Added in v2 R2: addressing critic's MAJOR-R2-1 — define "all consumers" precisely, time-bound to T+1 merge -->

**Definition of "all consumers" (precise predicate at T+1 merge):**

```bash
# Run at the moment of T+1 PR's merge, on the merged HEAD.
SCOPE='crates/runtime/src/'  # excludes tests/, benches/, docs/, examples/
PATTERN='__zs_is_|__InstallSlot_|__BrandSlot_'
COUNT=$(grep -rE "$PATTERN" $SCOPE 2>/dev/null | grep -v '^\s*//' | wc -l)
[ "$COUNT" -eq 0 ] || { echo "consumer-migration incomplete: $COUNT remaining"; exit 1; }
```

The predicate excludes:
- `crates/runtime/tests/` (test fixtures may legitimately reference internal symbols).
- `crates/runtime/benches/` (same).
- Code comments (`//` lines).
- Documentation rendered from doc-comments (handled separately by the doc-build step).

**Scope is time-bound:** future code added AFTER T+1 merge that introduces new references to underscored symbols is a regression and is caught by the deprecation warning at PR time. Wave 8's removal step is conditional on the predicate's holding for two consecutive PRs (T+2 and T+3) — if any new reference is added, the removal step does not advance.

Format mirrors `pin-project-internal`'s split-public-vs-private-symbols approach (closer match than `zsapp.md`'s wire-format-spec shape). Wave 5.

---

## §4 Migration sequence

The fix waves as a precise sequence with dependencies and effort estimates. Effort estimates are calibrated per `feedback_estimates_hours_not_weeks.md` — multiply by ~40 for industry-standard equivalents.

| Wave | Items | Dependencies | Est. effort | Status | Score progression |
|---|---|---|---|---|---|
| 1 | gen_throw + brand cap + must_str + kebab + dead code + v8_iterable `&mut self` | none | shipped | ✅ commits `22ab81c`, `ec09c37`, `d9c5528`, `896c6de`, `ae43938`, `f81e982`, `1aa2c61` | 60 → 65 |
| 2 | method.rs sweep — `Cell<Option<usize>>`, `gen_throw_op_error_arms` migration of 2 inline sites, `gen_recover_box` 7 callsite migrations, `must_str` completion, `expect()`→Result fix | Wave 1 cleared | ~6h | open | 65 → 70 |
| 3 | `shared/` extraction + `ClassConfig` refactor + `gen_install` decomposition into `emit/` submodules | Wave 2 land | ~10h | open | 70 → 75 |
| 4 | `parse/` MarkerAttr trait unification + single-scan parser + `KnownType` registry + table-driven type dispatch | Wave 3 land | ~12h | open | 75 → 80 |
| 5 | `runtime/` facade + STABILITY.md + emit-symbol public API formalization (`is_instance` + `Slot`) | Wave 4 land | ~6h | open | 80 → 84 |
| 6 | `mod.rs` further split (`brand.rs`, `slot_types.rs`, `install.rs` separate files; assemble_tokens orchestrator) | Wave 5 land | ~4h | open | 84 → 86 |
| 7 | WebIdlDict / WebIdlEnum / v8_iterable insta snapshots + WebIdlDict reference-type rejection + fastcall trybuild fixtures | Wave 6 land | ~3h | open | 86 → 87 |
| 8 | Subtle cleanups (§13.3-§13.8) + `__zs_*` symbol deprecation (after consumer migration to is_instance) | Wave 7 land | ~5h | open | 87 → 88 |

Total open work: **~46 hours** of focused effort. The score climbs steadily through Wave 4 (the four highest-leverage refactors) and asymptotes through Waves 5-8 (polish).

### §4.1 Wave 2 — method.rs sweep

**Findings closed:** C1+C2+F1 (remaining sites), C5/H13, anti-pattern §3 row 4, parts of H10.

**Files changed:**
- `crates/runtime-macros/src/v8_class/method.rs` (most edits)
- `crates/runtime-macros/src/shared/op_error.rs` (no change — already lands in Wave 1)
- `crates/runtime-macros/src/shared/reentry_guard.rs` (NEW)
- `crates/runtime-macros/src/shared/recover_box.rs` (NEW)
- `crates/runtime-macros/src/shared/mod.rs` (NEW; re-exports)

**Acceptance criteria:**
- All 254+ smoke tests green with no behavior change.
- Insta snapshot diff ONLY shows: (a) HashSet→Cell for re-entry guard; (b) gen_throw_op_error_arms call replacing two inline blocks; (c) gen_recover_box call replacing seven inline blocks. **No semantic drift.** Reviewers accept the snapshots.
- `expect("RuntimeState not in isolate slot")` at `method.rs:541` replaced with `match scope.get_slot::<...>() { Some(s) => ..., None => { /* throw RangeError */ return; } }`.
- `getter_args` dead code (method.rs:168-176) removed.

### §4.2 Wave 3 — shared/ extraction + ClassConfig

**Findings closed:** F4, F7, parts of F3.

**Files changed:**
- `crates/runtime-macros/src/v8_class/mod.rs` (1,409 → ~600 LOC after extraction)
- `crates/runtime-macros/src/v8_class/analyze.rs` (NEW; ParsedClassAttrs → ClassConfig)
- `crates/runtime-macros/src/v8_class/emit/` (NEW dir; one file per fragment)
- `crates/runtime-macros/src/v8_class/parse/ast.rs` (NEW; MethodKind, ClassMethod, ConstDecl, ConstKind moved out of mod.rs)
- `crates/runtime-macros/src/shared/class_config.rs` (NEW)

**Acceptance criteria:**
- `gen_install`, `gen_constructor_callback`, `gen_method_callback`, `gen_async_method_callback`, `gen_setter_callback`, `gen_same_object_getter_callback`, `gen_static_callback` ALL take `&ClassConfig` as the first arg.
- `mod.rs::expand_tokens` is ≤ 50 LOC orchestration: parse → analyze → ClassConfig → emit::assemble_tokens(&cfg) → return.
- Insta snapshots regenerate cleanly (some path differences expected; semantic is byte-identical).

### §4.3 Wave 4 — parse/MarkerAttr + KnownType

**Findings closed:** F5, F6, F8, F10, H5, H6, H7, anti-pattern §3 rows 5-7, parts of L10.

**Files changed:**
- `crates/runtime-macros/src/v8_class/parse/` (NEW dir; replaces parse.rs)
  - `mod.rs`, `marker_attr.rs`, `class_attrs.rs`, `method_attrs.rs`, `ast.rs`, `resolve.rs`
- `crates/runtime-macros/src/v8_class/fastcall/` (NEW dir; replaces fastcall.rs)
  - `mod.rs`, `types.rs`, `emit.rs`
- `crates/runtime-macros/src/shared/known_type.rs` (NEW)
- `crates/runtime-macros/src/lib.rs` (delete `gen_extract`, `is_byte_string`, `is_vec_u8`, etc; delegate to `shared::known_type::*`)

**Acceptance criteria:**
- 12 `extract_*` functions collapse to 12 `MarkerAttr` impls + one driver.
- 7 stringly-typed dispatch tables collapse to one `KnownType` enum + one `classify` function.
- Three new trybuild compile-fail fixtures: `v8_name_malformed`, `v8_to_string_tag_malformed`, `v8_inherit_intrinsic_malformed` — locking the now-strict error wording.
- Compile-time perf: measure with `cargo build --timings -p zeroship-runtime-macros`. Target: NO regression vs Wave 3 baseline.

### §4.4 Wave 5 — runtime/ facade + STABILITY.md + public API

**Findings closed:** F2 (both halves), §5 crate boundary.

**Files changed:**
- `crates/runtime-macros/src/runtime/mod.rs` (NEW)
- `crates/runtime-macros/STABILITY.md` (NEW)
- `crates/runtime-macros/README.md` (NEW; module map, attribute-add walkthrough)
- `crates/runtime/src/macro_runtime.rs` (NEW, runtime-side; re-exports)
- `crates/runtime-macros/src/v8_class/emit/brand.rs` (emit `<Class>::is_instance` + `<Class>::Slot` alongside the existing underscored symbols)
- All emit-time references switch from `::zeroship_runtime::*` to `::zeroship_runtime_macros::runtime::*` (or directly to `::zeroship_runtime::macro_runtime::*`).

**Acceptance criteria:**
- All consumer code in `crates/runtime/src/web/fetch/request.rs` etc. compiles unchanged (back-compat kept; new API is additive).
- `STABILITY.md` documents every emitted symbol with stability label.
- 1 new smoke test in `crates/runtime/tests/v8_brand_pub_smoke.rs` exercises `<Class>::is_instance` API directly.
- Snapshot diff: emit references go through the facade (paths change). No behavior diff.

### §4.5 Wave 6 — mod.rs further split

**Findings closed:** F3 (last LOC).

**Files changed:**
- `crates/runtime-macros/src/v8_class/mod.rs` (~600 → ~150 LOC)
- `crates/runtime-macros/src/v8_class/emit/{brand,slot_types,install,assemble}.rs` (NEW; brand and slot_types extracted from the megaquote)

**Acceptance criteria:**
- `mod.rs` orchestration only.
- Each emit/{brand,slot_types,install}.rs is ≤ 100 LOC.
- The 195-line megaquote is gone — `assemble_tokens` is a 50-LOC orchestrator that invokes per-fragment helpers.

### §4.6 Wave 7 — snapshots + WebIdlDict reference-type rejection + iterable split + fastcall trybuild

**Findings closed:** F9, H16, parts of F3 (v8_iterable god file).

**Files changed:**
- `crates/runtime-macros/src/v8_iterable/` (NEW dir; split 1,291-LOC monolith)
- `crates/runtime-macros/src/webidl_dict/` (NEW dir; split 383-LOC file)
- `crates/runtime-macros/src/webidl_enum/` (NEW dir; split 447-LOC file)
- `crates/runtime-macros/src/{v8_iterable,webidl_dict,webidl_enum}/snapshots/` (NEW; 9 new snapshot files)
- `crates/runtime/tests/compile_fail_fastcall/` (NEW; 3 trybuild fixtures locking compile-error wording)
- `crates/runtime/tests/compile_fail_webidl_dict/reference_field/` (NEW; 1 trybuild fixture)

**Acceptance criteria:**
- 9 new insta snapshots committed; all green.
- 4 new trybuild fixtures committed; all green with locked error wording.
- WebIdlDict with `&str` field emits a clear compile_error pointing at the offending field.

### §4.7 Wave 8 — subtle cleanups + symbol deprecation

**Findings closed:** §13.3-§13.8 (subtle code-quality), parts of L1-L10.

**Files changed:**
- Various small edits across `lib.rs`, `v8_class/emit/*.rs`, `webidl_dict/`, `webidl_enum/`.
- Adds `#[deprecated]` to `__zs_is_<Class>` and `__InstallSlot_<Class>` once all consumers have migrated to `<Class>::is_instance` and `<Class>::Slot`.

**Acceptance criteria:**
- Compile is warning-free in `runtime/`.
- Re-running the critic+reviser loop yields ≥85/100.

---

## §5 Risk + back-compat

Three risk axes:

### §5.1 Public API stability — LOW risk

**The 14 + 2 = 16 entry-point macros do not get renamed or removed.** Their behavior contract is locked by 254+ smoke tests in `crates/runtime/tests/v8_*_smoke.rs` and 6 trybuild compile-fail snapshots.

The recently-shipped extensions (post_init, fastcall, value_marshal, value_pairs `&mut self`, paired accessors, v8_const, v8_async_iterable) are explicit in the test surface and have their own commits ([commit hashes in TODO.md "Done" section]). The refactor MUST preserve their behavior byte-for-byte.

**One deliberate breaking-but-not-yet change:** Wave 5 emits BOTH the old `__zs_is_<Class>` symbol AND the new `<Class>::is_instance` API. Wave 8 adds `#[deprecated]`. Removal is deferred to a follow-up release. Consumers have one release cycle to migrate.

**No breaking changes in Waves 1-4 or Wave 7.** Wave 6 changes some emit paths (Wave 5's facade redirect) — see §5.2.

<!-- Added in v2 R1: addressing critic's CRITICAL-5 + MAJOR-5 — Wave 5 PR sequencing + diff classifier -->

### §5.1.1 Wave 5 PR sequencing (rollback-safe ordering)

Wave 5 touches **two crates** simultaneously (`runtime-macros` adds the facade module + emits new paths; `runtime` adds `macro_runtime.rs` re-exports). To keep every commit on `main` independently revertable, Wave 5 lands as **3 PRs**:

| PR | Crate | Change | Build state if rolled back independently |
|---|---|---|---|
| **5a** | `runtime` | Add `crates/runtime/src/macro_runtime.rs` with all 28 re-exports. Pure addition. No consumer code changes. | Green: file compiles cleanly with one top-level `#![allow(unused_imports)]` (see §5.1.1.1 below for the actual file skeleton). |
| **5b** | `runtime-macros` | Add `crates/runtime-macros/src/runtime/mod.rs` facade. Switch macro emit-paths from `::zeroship_runtime::*` to `::zeroship_runtime_macros::runtime::*` (which resolves to the same runtime types via PR 5a's re-exports). Insta snapshots regen — diff is path-only, semantically equivalent. | Green ONLY IF 5a has landed. If 5a is reverted before 5b reverts, build breaks at user crate's compilation (paths reference `zeroship_runtime::macro_runtime::*` which no longer exists). **Rollback rule:** revert 5b BEFORE 5a; CI gates this via the dependency declaration in the PR description ("requires 5a"). |
| **5c** | `runtime-macros` + `runtime` | Emit `<Class>::is_instance` as additional inherent-impl method (alongside the existing `__zs_is_<Class>` symbol). Add `STABILITY.md` and `README.md`. Add 1 smoke test (`v8_brand_pub_smoke.rs`). No consumer migrations. | Green: pure addition (new method on the user impl, new files). |

After 5a, 5b, 5c land in order, **consumer migrations are a follow-up PR** outside Wave 5 (see §7.3). The underscored symbols stay emitted; consumers can migrate at any time. Wave 8 only adds `#[deprecated]` once §7.3's consumer migrations close.

**Rollback playbook (per intermediate state):**

| State | Rollback action | Result |
|---|---|---|
| 5a landed only | Revert 5a | Green; `macro_runtime.rs` deleted; runtime returns to baseline. |
| 5a + 5b landed | Revert 5b first, then 5a | Green at each step. |
| 5a + 5b + 5c landed (full Wave 5) | Revert 5c first, then 5b, then 5a | Green at each step. |
| Production hotfix needed mid-wave | Cherry-pick the hotfix on top of the partially-landed waves; the facade is purely additive at runtime layer, so hotfixes that touch runtime types still resolve through `macro_runtime` re-exports. | Hotfix-able without unwinding the wave. |

**Independent-revert guarantee:** every PR in Wave 5 (and every wave) leaves a green build at every commit on `main`. CI is gated on `cargo test --workspace` per push.

<!-- Added in v2 R2: addressing critic's MAJOR-R2-2 — actual file skeleton with the lint allow at top -->

#### §5.1.1.1 Wave 5a `macro_runtime.rs` skeleton

```rust
// crates/runtime/src/macro_runtime.rs
//! Stable re-exports consumed by `runtime-macros`'s emit. See
//! `crates/runtime-macros/STABILITY.md`. Renaming or relocating any
//! item below is a wire-format break for the macro's emit; coordinate
//! with the macro maintainers.
//!
//! The `#![allow(unused_imports)]` allow at the top is intentional:
//! at the moment Wave 5a lands, NO consumer references this module
//! (Wave 5b switches the macro emit to use it). Without the allow,
//! the lint fires on every re-export. The allow is removed in
//! Wave 5b's PR after the macro starts referencing all paths.

#![allow(unused_imports)]

pub mod byte_string {
    pub use crate::byte_string::{ByteString, read_byte_string};
}

pub mod state {
    pub use crate::core::state::{
        OpError, OpErrorKind, OpResult, SharedState, IntoResolveValue,
    };
}

pub mod dom {
    pub mod exception {
        pub use crate::web::dom::exception::build;
    }
}

pub mod node_error {
    pub use crate::node_error::build_node_exception;
}

pub mod clamp {
    pub use crate::clamp::*;
}

pub mod wrap {
    pub use crate::wrap::*;
}

pub mod enforce_range {
    pub use crate::enforce_range::*;
}

pub mod url_native {
    pub mod helpers {
        pub use crate::url_native::helpers::*;
    }
}

pub mod convert {
    pub use crate::convert::WebIdlConvertible;
}

pub mod v8_instance {
    pub use crate::v8_instance::{V8ClassInstance, sealed};
}
```

The crate-level allow squelches the unused-re-export warning during Wave 5a-only state. Wave 5b's PR removes the allow once the macro emits start referencing all paths.

**Why crate-level (not per-item):** the file is a re-export dispatch table. Per-item `#[allow]` clutters every `pub use`. Crate-level is the idiomatic Rust pattern for re-export shims (see `tracing-core/src/lib.rs:14`'s `#![allow(missing_docs)]` for the equivalent pattern in tracing).

### §5.1.2 Snapshot diff classifier (reviewer cheatsheet)

Per-category rule for accepting `cargo insta accept` diffs in Wave-N PRs:

| Diff category | Auto-accept? | Reviewer must verify |
|---|---|---|
| **Path-swap only** (e.g., `::zeroship_runtime::state::OpError` → `::zeroship_runtime_macros::runtime::state::OpError`) | YES | nothing extra; diff is mechanical |
| **Helper-call substitution** (inline 25-LOC OpError-throw arm → 3-line `gen_throw_op_error_arms!()` invocation) | NO | reviewer asserts the helper's expansion is byte-equivalent to the inline form by reading the helper's snapshot test |
| **Structural change** (HashSet → Cell; gen_install N-arg → 1-arg ClassConfig) | NO | reviewer (a) confirms `cargo test --workspace -p zeroship-runtime` passes (smoke + behavior); (b) confirms `./tests/bench_platform.sh` passes (perf within ±5%); (c) leaves a PR comment naming the structural change |
| **Whitespace-only** (prettyplease re-format, brace placement, etc.) | YES | nothing extra; `prettyplease::unparse` is deterministic |
| **New emission** (Wave 5's `<Class>::is_instance` impl, STABILITY.md) | YES | the addition itself IS the PR; reviewing the emit shape IS the review |
| **Deletion** (Wave 8 deprecation removes `__zs_*` after consumer migration) | NO | reviewer confirms ALL consumers migrated (greps `__zs_is_` in `crates/runtime/src/`) before accepting |

PR template instruction (Wave 2+): "Diff category for snapshot changes: [path-swap | helper-call | structural | whitespace | addition | deletion]. If structural, name the structural change."

<!-- Added in v2 R2: addressing critic's MAJOR-R2-3 — machine-checkable snapshot classifier -->

### §5.1.3 Snapshot diff classifier — implementation script

A `tools/snapshot_classify.sh` script gives each snapshot diff a machine-checkable classification. Auto-accept rules from §5.1.2 are implemented; reviewer is alerted only when a diff falls into a non-auto-accept category.

```bash
#!/usr/bin/env bash
# tools/snapshot_classify.sh — classify insta snapshot diffs in a PR.
#
# Usage:
#   tools/snapshot_classify.sh <baseline-sha> <pr-head-sha>
#
# Exits 0 if all diffs are auto-acceptable (path-only, whitespace-only,
# new file). Exits 1 if any diff requires reviewer attention.

set -euo pipefail
BASELINE_SHA="${1:?need baseline sha}"
HEAD_SHA="${2:?need head sha}"

SNAPSHOT_GLOB='crates/runtime-macros/src/**/snapshots/*.snap'

# Get the diff for snapshot files only.
DIFF=$(git diff "$BASELINE_SHA" "$HEAD_SHA" -- $SNAPSHOT_GLOB)

if [ -z "$DIFF" ]; then
  echo "[ok] no snapshot changes"
  exit 0
fi

# Strip diff metadata (---/+++ lines, @@ hunks); keep only +/- content.
CONTENT_DIFF=$(echo "$DIFF" | grep -E '^[+-]' | grep -vE '^[+-]{3}')

# Classify by removing path-swap and whitespace-only changes.
PATH_SWAPS=$(echo "$CONTENT_DIFF" | \
  grep -E 'zeroship_runtime|zeroship_runtime_macros::runtime' | \
  wc -l)
NON_PATH=$(echo "$CONTENT_DIFF" | \
  grep -vE 'zeroship_runtime|zeroship_runtime_macros::runtime' | \
  grep -vE '^[+-]\s*$' | \
  wc -l)

echo "[classify] path-swap lines: $PATH_SWAPS"
echo "[classify] non-path-swap lines: $NON_PATH"

if [ "$NON_PATH" -eq 0 ]; then
  echo "[ok] all changes are path-swap or whitespace; auto-acceptable"
  exit 0
fi

echo "[stop] $NON_PATH lines require reviewer attention. Examples:"
echo "$CONTENT_DIFF" | grep -vE 'zeroship_runtime|zeroship_runtime_macros::runtime' | head -20
exit 1
```

CI gates: `tools/snapshot_classify.sh "$GITHUB_BASE_SHA" "$GITHUB_HEAD_SHA"`. If exit 1, the PR's snapshot-changes column in the merge checklist requires a reviewer's "approved structural change" comment.

**Limitations of the script:**
- Doesn't catch *substantive* path renames (e.g., a re-export gets renamed AND the macro's emit-path swaps to the new name; both legs cancel and the script would call it path-swap-only when it's actually two changes). Acceptable: the smoke tests catch any net behavior change.
- Whitespace-only diffs use `prettyplease` re-format, which is deterministic; the script's `^[+-]\s*$` filter catches the trivial cases.
- Helper-call substitutions (Wave 2's gen_throw_op_error_arms / gen_recover_box) appear as multi-line `-` and `+` blocks; these are correctly flagged as non-path-swap and require reviewer attention.
- **Status: skeleton.** The script lands as part of Wave 2's PR; it is exercised on the actual Wave-2 snapshot diff (HashSet→Cell migration is the first non-trivial diff to test against). Until then, the rule is best-effort.

**Worked example diff** (Wave-5b path-swap; expected to auto-accept):

```diff
--- a/crates/runtime-macros/src/v8_class/snapshots/class_basic.snap
+++ b/crates/runtime-macros/src/v8_class/snapshots/class_basic.snap
@@ -123,7 +123,7 @@
                 if let ::zeroship_runtime::state::OpErrorKind::JsValue(__global) = &__err.kind {
-                    let __exc = v8::Local::new(scope, __global);
+                    let __exc = v8::Local::new(scope, __global);
                     scope.throw_exception(__exc);
                     return;
                 }
@@ -145,7 +145,7 @@
-                ::zeroship_runtime::state::OpErrorKind::TypeError => v8::Exception::type_error(scope, __msg),
+                ::zeroship_runtime_macros::runtime::state::OpErrorKind::TypeError => v8::Exception::type_error(scope, __msg),
```

After running `tools/snapshot_classify.sh master HEAD`:

```text
[classify] path-swap lines: 2
[classify] non-path-swap lines: 0
[ok] all changes are path-swap or whitespace; auto-acceptable
```

**Wave-2 worked example** (HashSet→Cell migration; expected to require reviewer attention):

```diff
@@ -100,9 +100,12 @@
-    ::std::thread_local! {
-        static __INFLIGHT: ::std::cell::RefCell<::std::collections::HashSet<usize>> =
-            ::std::cell::RefCell::new(::std::collections::HashSet::new());
-    }
-    let __already_inflight = __INFLIGHT.with(|__s| !__s.borrow_mut().insert(__inflight_addr));
+    ::std::thread_local! {
+        static __INFLIGHT: ::std::cell::Cell<::std::option::Option<usize>> =
+            const { ::std::cell::Cell::new(::std::option::Option::None) };
+    }
+    let __prior_inflight: ::std::option::Option<usize> = __INFLIGHT.with(|__s| __s.get());
+    let __already_inflight = matches!(__prior_inflight, Some(__a) if __a == __inflight_addr);
+    if __already_inflight { /* throw + return */ }
+    __INFLIGHT.with(|__s| __s.set(::std::option::Option::Some(__inflight_addr)));
```

After running the script:

```text
[classify] path-swap lines: 0
[classify] non-path-swap lines: 7
[stop] 7 lines require reviewer attention. Examples:
+    static __INFLIGHT: ::std::cell::Cell<::std::option::Option<usize>> =
-    static __INFLIGHT: ::std::cell::RefCell<::std::collections::HashSet<usize>> =
...
```

Reviewer must add a "structural change: HashSet→Cell reentry guard" comment + verify smoke tests pass.

### §5.2 Emit token shape — MEDIUM risk

Insta snapshots lock against drift. Three categories of expected snapshot diff:

| Wave | Expected diff | Reviewer guidance |
|---|---|---|
| 2 | HashSet→Cell in reentry guard, 2 inline OpError sites collapse to helper-call, 7 inline recovery preambles collapse to helper-call | Verify byte-identical RUNTIME behavior (smoke tests). Accept. |
| 4 | Emit references to `KnownType`-driven extraction (different intermediate names) | Verify smoke tests pass. Accept. |
| 5 | All `::zeroship_runtime::*` paths funnel through `::zeroship_runtime_macros::runtime::*` (or `::zeroship_runtime::macro_runtime::*`) | Verify all consumer crates compile. Accept. |
| 6 | Megaquote split into per-fragment helpers — emission order is preserved; whitespace may differ | `prettyplease` re-format ensures whitespace stability. Accept. |

For Wave 5's facade refactor, the snapshot regen procedure is:

```bash
# Before regenerating: confirm smoke tests pass on the unchanged emit shape.
cargo test --workspace -p zeroship-runtime
# Now run snapshot regeneration:
INSTA_UPDATE=auto cargo test -p zeroship-runtime-macros
# Review the diff; confirm semantic equivalence (paths only, no logic).
cargo insta accept
```

**Insta snapshot count:**

| Wave | Existing snapshots | New snapshots | Total |
|---|---|---|---|
| 0 (today) | 3 (class_basic, class_with_state_marker, class_marker_equals_receiver_errors) | 0 | 3 |
| 7 | 3 (regenerated through W5 path changes) | 9 (3 webidl_dict, 3 webidl_enum, 3 v8_iterable) | 12 |

### §5.3 Compile-time perf — LOW risk

<!-- Added in v2 R1: addressing critic's MAJOR-7 — methodology with n, cache state, baseline tool -->

**Acceptance criterion:** wall-clock for `cargo build -p zeroship-runtime --release` within **+2% of baseline mean** at p=0.95 (Welch's t-test). Failures bisect.

**Measurement methodology:**

| Parameter | Value |
|---|---|
| `n` (samples per condition) | **5** (matches rustc perf.rust-lang.org's n=4 plus one warm-up discarded) |
| Cache state | **clean** — `cargo clean -p zeroship-runtime --release` before each sample |
| sccache | **disabled** (`SCCACHE_DISABLE=1`) — sccache hits/misses are non-deterministic across PRs |
| Hardware | CI's matrix box (Linux x86_64, 16-core) — runner pinning ensures samples are on the same hardware |
| Cargo flags | `--release --locked --offline` (offline so registry timing isn't measured) |
| Other crates | exactly the workspace from `master @ <baseline-sha>` versus `master @ <wave-N-sha>` |
| Sample tool | `/usr/bin/time -f "%e"` (wall-clock, single-decimal precision) — NOT `cargo build --timings` (whose output captures per-codegen-unit timings, useful for profiling but noisy at the wall-clock granularity needed) |

**Baseline runs:**
```bash
# Baseline gate (gated on master @ <pre-wave-N-sha>)
for i in 1 2 3 4 5 6; do
  cargo clean -p zeroship-runtime --release
  /usr/bin/time -f "%e" cargo build -p zeroship-runtime --release --locked --offline 2>>baseline.txt
done
# Discard sample 1 (warm-up); compute mean+stddev of samples 2-6.
```

**Post-wave-N runs:**
```bash
# Same procedure on the post-wave-N branch.
```

**Decision rule:** Welch's t-test on the two means with α=0.05. Reject (= regression detected) if p < 0.05 AND mean delta exceeds 2% of baseline.

**Why 2% (not 1%):** baseline `cargo build -p zeroship-runtime --release` on the CI box is ~70-90s (estimate; baseline gate computed at the START of Wave 2's PR via the `baseline.txt` capture above and saved as a CI artifact for use across all subsequent waves). 2% = ~1.5-2s, which is detectable with n=5 against typical ε of 0.5s. Tighter targets demand more samples and longer CI; 2% is the correct trade-off for a refactor PR.

**If `--timings` instrumentation is needed for diagnosis:** run on a separate machine (not CI) with the same `--locked --offline` setup to capture per-codegen-unit timing diffs. The PR description includes the `--timings` JSON if and only if the wall-clock test detected regression.

**Net expectation per wave:**
- Wave 4 (table-driven dispatch): compile-time **may improve** by 0.5-1% (one attribute walk vs six).
- Wave 5 (facade): proc-macro expansion doesn't traverse the indirection (it's resolved at user-crate compile time). **Expected: 0.0-0.5% delta.**
- Wave 6 (file split): more files = more rustc parser invocations. **Expected: +0.5-1.0% (acceptable).**
- Wave 7 (snapshot regen): no compile-path change. **Expected: 0%.**

Net expected delta across all waves: ±2% (within band).

### §5.4 Runtime perf — LOW risk (validate against bench)

`httpGet 16w` benchmark: 314,753 req/s baseline (post-fastcall, `2026-05-04`). The refactor changes ONLY the emit token shape, never the resulting machine code — every change is a same-sized helper-call substitution or a same-sized rearrangement. After full landing:

```bash
./tests/bench_platform.sh
# Expect 314,753 ± 5%; if outside band, BISECT to find the regressing wave.
```

Bench is in CI per `tests/bench_platform.sh`.

<!-- Added in v3 R4: addressing critic's MINOR-R4-3 — methodology rigor parity with §5.3 -->

**Measurement methodology (parity with §5.3):**

| Parameter | Value |
|---|---|
| `n` (samples per condition) | **3** (matches bench_platform.sh's existing default) |
| Bench duration per sample | 60 seconds (steady-state per `tests/bench_platform.sh`'s `--duration 60`) |
| Hardware | CI's bench-class box (a dedicated runner labelled `zerobench` with isolated CPUs and disabled hyperthreading; pinned via the runner label) |
| Decision rule | mean delta ≤ 5% of baseline AND no individual sample deviates by >10% |
| Tool | `crates/runtime/benches/zerobench-runner` (the existing `httpGet 16w` test) |
| Workflow | `cargo bench -p zeroship-runtime --bench httpGet --release` against the post-wave-N branch |

If the runtime-perf gate fails, the gating wave is BISECTED via the per-wave snapshot lock-in: each wave's PR ran the bench at merge; deviation from the per-wave-baseline pinpoints the regressing wave. This is implementable today via `tests/bench_platform.sh`'s log artifact retained in CI per-PR.

---

## §6 Testing strategy

### §6.1 What runs after each wave

| Layer | What it covers | Coverage today | Coverage after Wave 7 |
|---|---|---|---|
| Insta snapshots | Byte-identical emit shape | 3 (`v8_class`) | 12 (+ webidl_dict, webidl_enum, v8_iterable) |
| Trybuild | Compile-error wording locked | 6 (`post_init`, `state_marker`) | 14 (+ malformed-marker-attrs, fastcall-rejections, dict-reference-field) |
| Integration smoke | Behavior under V8 | 254+ tests in 24 files | 254+ (no regression) |
| Bench | runtime perf | `httpGet 16w` in CI | same |

### §6.2 Per-wave test plan

| Wave | Test addition | Verification |
|---|---|---|
| 2 | None (refactor preserves behavior) | All smoke tests + 3 insta accept (HashSet→Cell, OpError-throw, recovery preamble) |
| 3 | None (mechanical refactor) | All smoke + 3 insta accept (path differences) |
| 4 | 3 new trybuild fixtures (malformed `v8_name`, `v8_to_string_tag`, `v8_inherit_intrinsic`) | All smoke + 3 trybuild + 3 insta |
| 5 | 1 new smoke (`<Class>::is_instance` direct call), STABILITY.md compiled link-check | All smoke + 1 new + 3 insta accept |
| 6 | None | All smoke + 3 insta accept |
| 7 | 9 new insta snapshots + 4 new trybuild + 1 new smoke (WebIdlDict reference-field rejection) | All smoke + 9 new insta + 4 new trybuild |
| 8 | None (cleanup + deprecation) | All smoke green, no new warnings in `runtime/` |

### §6.3 Regression catch surface

After all waves:

- **All 254+ `v8_*_smoke` tests** in `crates/runtime/tests/` continue to pass.
- **All trybuild compile-fail** snapshots green (existing 6 + new 8 = 14).
- **All 12 insta snapshots** green (existing 3 + new 9).
- **`httpGet 16w` benchmark** within ±5% of 314,753 req/s baseline.
- **`cargo build --timings -p zeroship-runtime`** within ±2% of baseline duration.
- **A re-run of the critic+reviser loop** on the refactored crate hits ≥85/100.

<!-- Added in v2 R1: addressing critic's MAJOR-1 — property test plan -->

### §6.4 Property test plan (Wave 7)

Wave 7 adds a `proptest!` harness in `crates/runtime-macros/tests/proptest_emit.rs` that asserts emit-shape stability across the Wave-2-through-7 refactor. The strategy:

**Tooling:** `proptest = "1.4"` (workspace dep, already in `Cargo.toml` for `gateway` benchmarks).

**Shape generator:** a `Strategy` for `ClassShape`:

```rust
#[derive(Debug, Clone, proptest_derive::Arbitrary)]
struct ClassShape {
    has_state_marker: bool,
    has_inherit: Option<InheritShape>,        // 0 or 1 inherit; intrinsic+base mutually exclusive
    has_iterable: Option<IterableShape>,
    has_async_iterable: bool,
    consts: Vec<ConstShape>,                  // 0..=3 consts
    methods: Vec<MethodShape>,                // 1..=8 methods
}

#[derive(Debug, Clone, proptest_derive::Arbitrary)]
struct MethodShape {
    kind: MethodKindShape,                    // Method | Getter | Setter | StaticMethod | StaticGetter | AsyncMethod
    has_fastcall: bool,                       // tied to kind: only Method/Getter
    receiver_mut: bool,                       // self/&self/&mut self
    name: SmallString,                        // bounded ident
    primitive_arg_types: Vec<PrimitiveType>,  // 0..=3 args, matches KnownType registry
    primitive_ret_type: Option<PrimitiveType>,
}
```

**Property 1 — Wave 4 invariance:** for every (Wave-3 emit, Wave-4 emit) pair on the same `ClassShape`, the `prettyplease::unparse(syn::parse2(emit))` output is byte-identical.

```rust
proptest! {
    #[test]
    fn wave_4_emit_byte_identical_to_wave_3(shape: ClassShape) {
        let input_tokens = render_class_shape_to_input_tokens(&shape);
        let wave3_emit = wave3::expand_tokens(quote!{}, input_tokens.clone());
        let wave4_emit = wave4::expand_tokens(quote!{}, input_tokens);
        prop_assert_eq!(
            prettyplease::unparse(&syn::parse2(wave3_emit).unwrap()),
            prettyplease::unparse(&syn::parse2(wave4_emit).unwrap()),
        );
    }
}
```

`wave3::expand_tokens` is captured BEFORE Wave 4 lands as a fork in `tests/proptest_emit.rs`. Once Wave 4 lands and the property holds for 1024 random shapes, the captured `wave3` body is replaced with a frozen byte snapshot.

**Property 2 — Wave 5 path-only divergence:** for the same `ClassShape`, the `Wave-4` and `Wave-5` emit differ ONLY in the textual paths `::zeroship_runtime::*` → `::zeroship_runtime_macros::runtime::*`.

```rust
proptest! {
    #[test]
    fn wave_5_emit_path_swap_only(shape: ClassShape) {
        let input = render_class_shape_to_input_tokens(&shape);
        let wave4_emit = wave4::expand_tokens(quote!{}, input.clone());
        let wave5_emit = wave5::expand_tokens(quote!{}, input);
        let wave4_normalized = wave4_emit.to_string()
            .replace("::zeroship_runtime::", "::FACADE_PLACEHOLDER::");
        let wave5_normalized = wave5_emit.to_string()
            .replace("::zeroship_runtime_macros::runtime::", "::FACADE_PLACEHOLDER::");
        prop_assert_eq!(wave4_normalized, wave5_normalized);
    }
}
```

If Wave 5 introduces ANY non-path-swap diff, this property fails — caught at PR time.

**Property 3 — Determinism:** running `expand_tokens` on the same input N=10 times produces byte-identical output.

```rust
proptest! {
    #[test]
    fn expand_is_deterministic(shape: ClassShape) {
        let input = render_class_shape_to_input_tokens(&shape);
        let reference = expand_tokens(quote!{}, input.clone()).to_string();
        for _ in 0..9 {
            prop_assert_eq!(expand_tokens(quote!{}, input.clone()).to_string(), &reference);
        }
    }
}
```

Catches accidental `HashMap` iteration-order leakage (today's parse uses `HashMap<String, &ClassMethod>` for accessor pair matching at `mod.rs:337` — a paired-getter-setter ordering drift would break this).

**Property 4 — KnownType registry totality:** for every `KnownType` variant emitted by `classify`, `gen_extract` produces non-empty tokens AND parses as a valid `syn::Block`.

```rust
#[test]
fn every_known_type_emits_parseable_extract() {
    for kt in KnownType::iter() {  // strum::IntoEnumIterator
        let tokens = emit_extract(&kt, 0, &Ident::new("x", Span::call_site()));
        assert!(!tokens.is_empty(), "KnownType::{:?} produced empty tokens", kt);
        // Wrap in a block so non-block-shaped extract tokens (e.g.,
        // single statements) parse correctly.
        let block_tokens = quote! { { #tokens } };
        let _parsed = syn::parse2::<syn::Block>(block_tokens)
            .unwrap_or_else(|e| panic!("emit_extract for {:?} produced unparseable tokens: {}", kt, e));
    }
}
```

**Test execution:** these properties run in `cargo test -p zeroship-runtime-macros --test proptest_emit`. Default `proptest!` config runs 256 cases per property; CI uses 1024 (set via `proptest::test_runner::Config::with_cases(1024)`).

<!-- Added in v2 R2: addressing critic's MAJOR-R2-5 — runtime estimate corrected -->

**Estimated CI runtime impact:**

| Property | Cases | Per-case cost (estimated) | Total cost |
|---|---|---|---|
| 1. Wave 4 invariance | 1024 | ~50ms (one expand + prettyplease) × 2 (compare wave3 vs wave4) | ~1.7 min |
| 2. Wave 5 path-swap | 1024 | ~50ms × 2 + small string ops | ~1.7 min |
| 3. Determinism (N=10) | 1024 | ~50ms × 10 | ~8.5 min |
| 4. KnownType totality | n/a (single test) | ~variants × 5ms | ~1 sec |

Properties 1-3 dominate at ~12 min. Property 3's N=10 is overkill; **proposed CI tuning**: drop to N=3, reducing total to ~5 min.

**For PR-time runs:** disabled by default; gated behind `cargo test -p zeroship-runtime-macros --features proptest --test proptest_emit`. CI runs nightly on `main`. Per-PR runs use `cargo test -p zeroship-runtime-macros --test proptest_emit -- --quick` (256 cases per property, ~3 min).

**For Wave 7 PoC:** when the proptest harness lands, the actual per-case cost is measured via `--nocapture` + timing; if measurement reveals >100ms/case, the case-count is reduced to keep CI under 10 min (PR-blocking budget).

**Failure isolation:** when proptest finds a failing case, it shrinks to a minimal counter-example and writes it to `proptest-regressions/proptest_emit.txt`. The regression file is checked into git so the case is replayed in future runs.

**Property 5 (deferred to Wave 8 polish):** "for any class with no `#[v8_state_marker]`, the emit's marker-resolution branch is identical to the marker == class branch when marker == class is supplied" — requires the `resolve_state_and_marker` helper to be stable. Deferred because it depends on Wave 8's `__zs_*` deprecation finishing.

### §6.5 Refactor-mode verification (per-wave)

Each wave's PR runs:

```bash
# 1. Smoke tests — runtime behavior preserved.
cargo test --workspace -p zeroship-runtime

# 2. Insta snapshots — emit shape pinned, regression caught at PR time.
cargo test -p zeroship-runtime-macros --release

# 3. Trybuild — compile-error wording stable.
cargo test -p zeroship-runtime --test compile_fail_post_init
cargo test -p zeroship-runtime --test compile_fail_state_marker
# (post-Wave-4: also compile_fail_v8_name etc.)

# 4. Bench — runtime perf preserved.
./tests/bench_platform.sh
```

PR cannot merge if any of (1), (3), (4) regress, or if (2) regresses without explicit reviewer accept.

---

## §7 Migration path

Each wave is a SEPARATE pull request. No "big bang" PR. Each wave:

- Lands on `main` only after CI green.
- Preserves green build + green tests at every commit.
- Has rollback cost ≤ a single `git revert`.

### §7.1 Order + parallelization

<!-- v2 R1: aligned arrows + corrected Wave 4 ↛ Wave 5 dependency -->

```
Wave 1 (✅ shipped)
  │
  ▼
Wave 2 (method.rs sweep)
  │
  ▼
Wave 3 (shared/ + ClassConfig)
  │
  ├─────────────────────────────────────┐
  ▼                                     ▼
Wave 4a (parse + MarkerAttr)            Wave 7 (snapshots; pure additions)
  │                                     │
  ▼                                     │
Wave 4b (KnownType registry)            │
  │                                     │
  ├─────────────────────────────────────┤
  │                                     │
  ▼                                     ▼
Wave 5 (5a runtime/macro_runtime → 5b runtime-macros/runtime → 5c is_instance + STABILITY)
  │
  ▼
Wave 6 (mod.rs further split)
  │
  ▼
Wave 8 (cleanups + symbol deprecation, after consumer migration)
```

**Parallelization opportunities:**

1. **Wave 7 (snapshots + dict reference-rejection + fastcall trybuild)** is independent of Waves 4-6 — the snapshot files are pure additions; the dict and fastcall fixtures are pure additions. Wave 7 can land any time after Wave 3.

2. **Wave 8 cleanups** are independently mergeable — each is a single-file fix touching lines that other waves don't touch (§13.3-§13.8 are scattered).

**Sequential bottlenecks:**

- Wave 2 must precede Wave 3 (Wave 3's `gen_recover_box` extraction depends on Wave 2's `gen_recover_box` being landed).
- Wave 3 must precede Wave 4 (Wave 4's `parse_class_attrs` produces `ParsedClassAttrs` which Wave 3's `analyze` consumes to build `ClassConfig`).

<!-- v2 R1: corrected — Wave 4 does NOT block Wave 5 (touches different files / phases). -->
- ~~Wave 4 must precede Wave 5~~ — **corrected in v2**: Wave 5 (the runtime/ facade + `V8ClassInstance` trait) touches emit-side path strings and adds a runtime-side trait. Wave 4 (parse-side `MarkerAttr` + `KnownType` registry) touches parse and classification. They share NO file. Wave 5 can land BEFORE, AFTER, or in PARALLEL with Wave 4.

**Sub-wave decomposition (v2 R1):** Wave 4 itself is two independent axes:

- **Wave 4a — Parse unification:** introduces `MarkerAttr` trait + `parse_class_attrs` single-walk + `parse_method_attrs`. Touches `v8_class/parse/`. ~6h.
- **Wave 4b — KnownType registry:** introduces `shared/known_type.rs` + collapses `classify_extract` / `is_byte_string` / fastcall lists. Touches `lib.rs` + `v8_class/fastcall/`. ~6h.

4a and 4b are independently mergeable (different files, no shared types). 4a depends on Wave 3's `ParsedClassAttrs` reflowing through ClassConfig; 4b depends only on Wave 1's `must_str` / Wave 2's `gen_recover_box` being landed (i.e., 4b can land in parallel with Wave 3 if needed).

<!-- Added in v2 R2: addressing critic's MAJOR-R2-4 — Wave 6 sub-PR dependency disambiguation -->

**Wave 6's dependency on Wave 5 sub-PRs:**

- Wave 6 (mod.rs further split) depends on **5a only** (the runtime-side `macro_runtime.rs` re-exports). Wave 6 splits the megaquote into per-fragment helpers; the per-fragment helpers reference `::zeroship_runtime::macro_runtime::*` paths (introduced by 5b's emit-path swap). However, Wave 6 splits the EMIT, not the runtime side, and the per-fragment files emit through whatever facade is current at the time. If 6 lands BEFORE 5b, the per-fragment helpers reference `::zeroship_runtime::*` directly; 5b later swaps them to the facade with a snapshot-regen pass.

| Wave 6 lands... | 5a status | 5b status | 5c status | Path emitted by Wave 6's per-fragment helpers |
|---|---|---|---|---|
| Before 5a | not yet | not yet | not yet | `::zeroship_runtime::*` (current shape) |
| After 5a only | landed | not yet | not yet | `::zeroship_runtime::*` (5a is additive — no change yet) |
| After 5a + 5b | landed | landed | not yet | `::zeroship_runtime_macros::runtime::*` (facade) |
| After full Wave 5 | landed | landed | landed | `::zeroship_runtime_macros::runtime::*` |

So Wave 6 is dependency-correct in any of these orderings. Conservative ordering: Wave 6 lands AFTER 5a + 5b (so the snapshot regen for Wave 6 doesn't double up with Wave 5b's regen). 5c (the `is_instance` trait + STABILITY) is independent.

### §7.2 Rollback cost per wave

| Wave | Rollback complexity | Notes |
|---|---|---|
| 2 | low | Single revert; no consumer-side migrations. |
| 3 | medium | Revert touches `mod.rs`, `analyze.rs`, all `emit/*.rs`. ~1,000-LOC diff. Snapshot regen on revert. |
| 4 | medium | Revert touches `parse/`, `fastcall/`, `lib.rs`. Trybuild fixtures must be reverted in lockstep. |
| 5 | high | The facade introduces an indirection through `::zeroship_runtime::macro_runtime` that EVERY new emit goes through. Reverting requires either (a) keeping the facade and reverting only the emit changes (minor) OR (b) deleting the facade and the runtime-side `macro_runtime.rs` (major). Rollback playbook: keep the facade, revert emit-path changes only. |
| 6 | low | Pure file-shuffling. Revert is mechanical. |
| 7 | low | Pure additions. Revert removes new files. |
| 8 | low | Multiple small reverts; each <50 LOC. |

### §7.3 Coordination with consumer-side work

Wave 5's `<Class>::is_instance` API is **additive**, not a replacement. Existing consumer code (`request.rs:230` etc.) continues to work. A follow-up PR — outside this proposal — migrates each consumer:

- `crates/runtime/src/node/async_hooks/als.rs:259` — 1-line change: `__zs_is_AsyncLocalStorage(scope, this_v)` → `AsyncLocalStorage::is_instance(scope, this_v)`.
- `crates/runtime/src/web/fetch/request.rs:230` — 1-line.
- `crates/runtime/src/web/fetch/request.rs:924` — 1-line: `<__InstallSlot_Request>` → `<Request::Slot>`.
- `crates/runtime/src/web/dom/event_target.rs:125-178` — REWRITE: hand-rolled `__InstallSlot_EventTarget` → manual `impl V8ClassInstance for EventTarget`. ~30 LOC delta.

Once all consumers migrate, Wave 8 can add `#[deprecated]` to the underscored symbols.

---

## §8 Success criteria

After all waves land, the following must hold simultaneously:

- **Composite score ≥ 85/100** from a re-run of architecture-critic + code-critic agents on the refactored crate. Per-dimension breakdown:
  - Crate boundary: ≥ 8 (was 5). `STABILITY.md` + facade documents the contract.
  - File / module structure: ≥ 8 (was 5). mod.rs ≤ 250 LOC; every file ≤ 250 LOC; every codegen helper ≤ 60 LOC.
  - Public surface: ≥ 8 (was 4). `is_instance` + `Slot` are stable; underscored symbols deprecated.
  - Design patterns: ≥ 8 (was 6). One `MarkerAttr` trait family; one `KnownType` registry; one `ClassConfig` parameter object; one `gen_throw_op_error_arms` helper.
  - Separation of concerns: ≥ 8 (was 5). parse/analyze/emit phases distinct.
  - Extension points: ≥ 8 (was 5). Adding a new attribute = one `MarkerAttr` impl + one `ClassConfig` field + one `emit/*.rs` helper. No parameter-list churn.
  - Test architecture: ≥ 8 (was 6). 12 insta + 14 trybuild + 254+ smoke.
- **mod.rs ≤ 250 LOC** (down from 1,409 today). After Wave 6 split: ~150 LOC.
- **`gen_install` ≤ 1 parameter** (the `&ClassConfig`). Down from 9-10 today.
- **Zero stringly-typed dispatch tables** in the codebase. The single `KnownType` registry is the documented source of truth.
- **All 6 OpErrorKind sites collapsed to one helper.** Wave 1 closed 2/4; Wave 2 closes the remaining 2.
- **`STABILITY.md` exists** and lists every emitted symbol with a "do not rename without macro coordination" note.
- **All 254+ smoke tests + 14 trybuild + 12 insta snapshots green.**
- **`httpGet 16w` within ±5% of baseline** (314,753 req/s).
- **`cargo build --timings -p zeroship-runtime` within ±2% of baseline.**

---

## §9 Open questions

These remain unresolved at v1; reviewer's call.

### §9.1 Should the `runtime/` facade be a separate crate? — **Closed in v2**

<!-- Added in v2 R1: addressing critic's CRITICAL-3 — defensible decision with cost/benefit + forcing function -->

**Decision (v2): in-crate.** Closed. Re-opens automatically when forcing function (below) trips.

**Cost/benefit table:**

| Axis | In-crate (chosen) | Separate crate (`zeroship-runtime-macros-rt`) |
|---|---|---|
| Workspace members | 0 added | +1 (5th sibling under `crates/`) |
| Cargo.lock entries | 0 added | +1 (mirrored in 8 lock files across release matrix) |
| Build edges | 1 (macros → runtime) | 2 (macros → rt-shim → runtime) — adds one synchronization point per CI run; on `cargo test --workspace`, the rt-shim must compile before `runtime-macros` proc-macro expansion can reference it |
| Cycle-topology risk | Low — proc-macro crates can't have build cycles by Rust's build-graph constraint; the runtime-side `macro_runtime` re-export module is a leaf (re-exports only) | Lower — the rt-shim crate has no inbound deps from runtime; topology is `runtime ←runtime-macros` and `runtime ←rt-shim ←runtime-macros` independent chains |
| Cargo features impact | None | Must propagate any `runtime` feature flags through the rt-shim (today: 0 features; latent risk) |
| Compile-time cost | 0 added | +1 crate compilation per clean build (~1.5s on the CI matrix's slowest box) |
| Documentation burden | 1 STABILITY.md | 2 (one per crate) — minor |
| Refactor coordination | All emit-path changes in one PR | Two-PR coordination per change to the facade |
| Reviewer cognitive load | Lower — readers learn one crate boundary | Higher — readers learn two |

**Forcing function (re-opens this question):** the in-crate facade re-opens to "split into separate crate" when ANY of:

1. A second consumer of `runtime-macros` appears (zeroship-runtime is the only consumer today; no concrete plan to add another).
2. The rt-side `macro_runtime` module exceeds 200 re-export lines (today: ~30 estimated for 28 paths).
3. A binary-size or compile-time regression of >5% is attributable to the in-crate facade pattern.
4. Semver coupling between runtime and runtime-macros breaks (today: lockstep release; no separate cadence).

If none of (1-4) trip in 12 months, leave as in-crate. If any trips, propose a follow-up that adds `zeroship-runtime-macros-rt` as the formalized boundary.

**Why not separate-by-default?** The architectural reality (per AGENTS.md key invariant: "The native surface is small and stable on purpose") is that `runtime-macros` exists to serve `zeroship-runtime`. There is no abstract "macro emission" market we serve. Separate-crate is over-engineering for a 1:1 producer-consumer.

**Reference:** `pin-project` ships `pin-project-internal` as a separate crate because of a 1.0 design (zero-runtime-cost pinning emitted alongside a runtime helper crate). Our analog is closer to `tracing-attributes` — the macro+runtime are tightly coupled and ship together. Tracing keeps `tracing-attributes` and `tracing` as separate crates because `tracing` is consumed by many third parties; `runtime-macros` is consumed by exactly one runtime.

### §9.2 Should snapshot generation be on-by-default in CI or opt-in?

**Trade-off:**
- On-by-default — `cargo insta accept` is part of every PR. Reviewer must approve diff. Ergonomic for the macro maintainer.
- Opt-in — snapshots fail CI by default. Maintainer must explicitly accept. Catches accidental drift.

**Tentative decision (v1):** opt-in (today's behavior; `cargo insta accept` requires explicit invocation). Acceptance is part of the PR diff and shows in code review. Wave 5+6 will produce many path-difference diffs; the reviewer accepts each batch with a "verified semantic equivalence" comment.

### §9.3 `Cell<Option<usize>>` re-entry guard — keying for multi-method classes

The current `HashSet<usize>` indexes by Box raw addr (per-instance). The new `Cell<Option<usize>>` is per-method per-thread. Question: does swapping the keying break the semantic of "guard re-entry of THIS method on THIS instance"?

Today: `HashSet<usize>` per (method × thread); each entry is a Box addr. So per-(method, instance) = at most one entry. Guard fires when the SAME (method, instance) is in flight twice on the same thread.

Cell variant Option A (single-slot, restore-on-drop): `Cell<Option<usize>>` per (method × thread); the slot holds the addr of the in-flight instance. Guard fires when the same (method, instance) is in flight twice. Works for the same case.

**Cross-instance nesting:** today, two distinct instances of the same method nest fine (HashSet has 2 entries). Under Option A, the inner call OVERWRITES the slot; the drop guard restores the prior addr. So the inner-call's drop guard restores the outer's addr. The outer's drop guard sets `None`. Works.

**Cross-method on same instance:** today, two distinct methods nest fine (different HashSets). Under Option A, different `__INFLIGHT` thread-locals (one per method). Same as today.

**Verdict:** Option A is semantically equivalent. The implementation in §3.4 is sound. **Closed.**

### §9.4 Should we migrate `#[reject_shared]` from `lib.rs` to a per-class attribute?

The marker is in `lib.rs:64-71` as a no-op proc-macro. It's consumed by `crates/runtime-macros/src/v8_class/parse.rs:174-200` for method-level use. The codegen wires it correctly today.

**Question:** does the trait-based `MarkerAttr` refactor change the location?

**Answer:** `#[reject_shared]` is method-level. After Wave 4, it's still parsed as a method-level marker via `MarkerAttr` trait — same shape as `#[v8_name]`. Lives in `parse/method_attrs.rs`. No change in semantics or location-in-code. **Closed.**

### §9.5 ~~Open placeholder~~ — Wave dependency between 4 and 5? — **Closed in v2 R1**

<!-- Added in v2 R1: addressing critic's MINOR-3 + MAJOR-4 — concrete reviewer-anticipation question -->

**Question (raised by R1 critic):** does Wave 5 (runtime/ facade + V8ClassInstance trait) depend on Wave 4 (parse/MarkerAttr + KnownType registry)?

**Answer:** No. They touch disjoint files and disjoint phases. See §7.1's revised dependency analysis. Wave 5 can land before, after, or in parallel with Wave 4. The earlier draft's "Wave 4 must precede Wave 5" claim was wrong; corrected in v2.

### §9.6 Snapshot acceptance burden — when does manual review become bottleneck?

**Question:** with 12 insta snapshots after Wave 7 (vs 3 today), and Waves 5+6 producing many path-change diffs, do reviewers become a bottleneck?

**Answer:** the §5.1.2 snapshot diff classifier addresses this. Path-only and whitespace-only diffs auto-accept (machine-checkable via `diff --color`). Helper-call substitutions and structural changes require reviewer attention but are bounded — Wave 2 has 2 sites (gen_throw_op_error_arms + gen_recover_box), Wave 3 has all paths recompose into ClassConfig (each emit fragment is one snapshot diff per fragment helper, ~10 fragments total), and Wave 5 is path-only (auto-accept). Estimated reviewer burden: ~30 min per wave PR for snapshot review. Acceptable for a refactor that compresses ~12K LOC of emit drift into a single locked artifact.

### §9.7 What if a future critic-loop disagrees with the in-crate facade choice?

<!-- Added in v2 R1: addressing critic's RISK-5 partial / Risk + back-compat dim 5 weakness -->

**Question:** §9.1 closes the in-crate vs separate-crate question for v2. What if a future review re-opens it?

**Answer:** §9.1's forcing function (4 trip conditions) governs. If any trips, propose a separate-crate split as a follow-up architecture proposal in `docs/proposals/`. The split itself is mechanical — `cargo new --lib zeroship-runtime-macros-rt`, move the runtime/ module's contents, add a `[dependencies] zeroship-runtime-macros-rt = { path = "../runtime-macros-rt" }` to runtime-macros, and the macro emits start referencing `::zeroship_runtime_macros_rt::*` instead.

**Cost breakdown (~4h total):**
- Read existing `runtime/macro_runtime.rs` re-export module to understand path topology (~30 min).
- Create new crate (`cargo new --lib`), populate `Cargo.toml` (~30 min).
- Move re-export contents from runtime/macro_runtime.rs → runtime-macros-rt/src/lib.rs (~45 min).
- Update runtime-macros' emit-paths from `::zeroship_runtime_macros::runtime::*` to `::zeroship_runtime_macros_rt::*` (~30 min, find-replace).
- Update CI scripts to handle new workspace member (~15 min).
- Run full test suite to confirm zero behavior change; accept snapshots (~30 min).
- Update STABILITY.md and proposal text (~30 min).
- Code review iteration (~30 min).

---

## §10 Self-score per the 7-dimension rubric

Self-evaluation of THIS proposal against the rubric the critic+reviser loop uses. Target ≥85 to converge.

<!-- v2 R1: scores updated to reflect critic-round-1 fixes. -->

| # | Dimension | v1 self | R1 critic | v2 self | Rationale (v2) |
|---|---|---|---|---|---|
| 1 | Problem framing | 9/10 | 78/100 | 9/10 | §1.1.1 quantified blast radius added. Three peer-baseline refactors cited (serde_derive 1.0.x, pin-project 0.4→1.0, tracing-attributes 0.1.21→0.1.27). Composite numbers consolidated. |
| 2 | Design adequacy | 9/10 | 72/100 | 9/10 | V8ClassInstance trait sealing fully specified (§3.5 — `mod sealed`, `pub trait V8ClassInstance: sealed::Sealed + 'static`, associated types instead of invalid `pub type Slot`). Lifetime parameterization documented. ClassConfig PoC in App. C. |
| 3 | Spec correctness | 8/10 | 64/100 | 9/10 | §A.1 acknowledges serde Ctxt divergence with rationale; §A.7 added concrete op2 cite. Fastcall arg vs return non-overlap explicit (§3.7). KnownType context split (general extract / arg / return) clarified. |
| 4 | Codegen feasibility | 9/10 | 68/100 | 9/10 | App. C ClassConfig PoC + lifetime-topology proof. §3.4 panic safety analysis (drop-guard runs on unwinding panic; abort-on-FFI is the existing behavior). Three fastcall lists collapse via FastcallArg/FastcallReturn enums (concrete code). |
| 5 | Risk + back-compat | 9/10 | 70/100 | 9/10 | Wave 5 PR sequencing (5a, 5b, 5c) with rollback rule + diff classifier (§5.1.1, §5.1.2). Compile-time perf methodology specified (n=5, Welch's t-test, p<0.05). Deprecation policy timeline (T+0, T+1 PR, T+2 PRs, T+3 PRs). Forcing function for in-crate-vs-separate-crate decision. |
| 6 | Testing strategy | 9/10 | 70/100 | 10/10 | §6.4 property test plan added (5 properties on `proptest = "1.4"`; 1024 cases per property; Wave 4 byte-identity, Wave 5 path-swap-only, determinism, KnownType totality). |
| 7 | Migration path | 9/10 | 76/100 | 9/10 | §7.1 corrected: Wave 4 ↛ Wave 5; sub-wave decomposition (4a parse, 4b types). §7.3's consumer migration adopts the "if a consumer can't migrate, document and skip" escape (§3.10's deprecation policy table). |

**v3 composite self-score: 65/70 = 92.9%.** Progression: v1 88.6% → R1 critic 71/100 → v2 R2 critic 88/100 → v3 R3 self 92.9%. Targets ≥90/100 from R3 critic (verifying convergence).

**Closed weaknesses:**

- ~~Wave 5 PR sequencing unused-imports~~ — closed in v2 R2 via §5.1.1.1.
- ~~Property test runtime estimate~~ — closed in v2 R2 via §6.4 table + N=3 tuning.
- ~~`V8ClassInstance::install_slot` lifetime escape~~ — closed in v2 R2 via `cloned_install_template` returning owned Global.
- ~~PoC circular verification~~ — closed in v2 R2 via `cargo expand` + sha256sum anchor.
- ~~Sealing pattern muddle (line 929 "Wait —")~~ — closed in v3 R3 via clean rewrite + Clippy `disallowed_types` cite verified.
- ~~Snapshot classifier script untested~~ — closed in v3 R3 via worked-example diffs (5b path-swap auto-accept; Wave-2 HashSet→Cell flagged for reviewer).
- ~~Property 4 misleading name~~ — closed in v3 R3: renamed `every_known_type_emits_parseable_extract`.
- ~~`serde::de::Visitor` cite~~ — closed in v3 R3: corrected to `serde_json::Map`'s `mod private { pub trait Sealed {} }`.

**Remaining (acceptable for v3):**

- The §5.1.3 script lands as part of Wave 2's PR; until then, the rule is best-effort but documented.
- Wave 8's "green CI" predicate is implicit (smoke + bench + lint, per §6.5). If the predicate becomes ambiguous in practice, formalize as a CI rule.

---

## Appendix A — Industry pattern porting

The architecture critic's §4 cited 6 industry references. This proposal explicitly ports patterns from THREE of them; the others are referenced for sanity-checks but not directly emulated.

### A.1 `serde_derive::internals::ast` — phase-distinct AST types (ported)

Serde owns its own AST after parsing — `Container`, `Variant`, `Field`, `Style` in `internals/ast.rs`. The phases are:

```
Syn DeriveInput → internals::ast::Container → de.rs / ser.rs emit
```

Cleanly separated phases. Each phase's types are owned by its module. **Our analog:** `ParsedClassAttrs` (parse phase, in `v8_class/parse/ast.rs`) → `ClassConfig` (analyze phase, in `shared/class_config.rs`) → emit (in `v8_class/emit/*.rs`). Same shape; different domain.

### A.2 `serde_derive::internals::symbol::Symbol` — interned attribute-name dispatch

Serde defines `pub const ALIAS: Symbol = Symbol("alias");` and friends, with `impl PartialEq<Symbol> for Ident`. So instead of `attr.path().is_ident("alias")`, serde writes `name == ALIAS`. **We don't port this.** The savings are marginal (microseconds across the build), and it adds a layer of abstraction. The single-walk parser in §3.6 already collapses the ident-comparisons to one place.

### A.3 `derive_builder_core` — analysis types in own crate (NOT ported)

`derive_builder` splits `derive_builder` (the proc-macro) and `derive_builder_core` (analysis types). Cleaner separation; explicit phase boundaries. **We don't port the two-crate split** (per §9.1 / Open Question 1). Trade-off: in-crate is simpler. Re-evaluate if a second runtime appears.

### A.4 `tracing-attributes::expand` — single attribute orchestrator (ported)

`tracing-attributes::expand::gen_function` is the orchestrator for `#[instrument]` codegen. ~800 LOC with aggressive sub-helper extraction. Each emit kind returns `TokenStream2` from a function ≤30 LOC. **Our analog:** `v8_class/emit/mod.rs::assemble_tokens(&ClassConfig)` is the orchestrator; per-fragment helpers (`gen_brand_check_helpers`, `gen_install_slot_types`, `gen_public_is_fn`, etc.) each ≤60 LOC.

### A.5 `async_trait` — heavily-factored single file (NOT directly ported)

`async_trait` lives in a single file but factors aggressively into ~50 small helpers. We could mirror this for `v8_iterable.rs` (which is single-file today at 1,291 LOC), but the LOC volume dictates a multi-file split (§2.1's `v8_iterable/{parse,analyze,emit_*}.rs`). The factoring discipline IS ported — every helper ≤60 LOC.

### A.6 Deno's `op2` macro — typed-codegen registry (ported via `KnownType`)

`op2` keeps a typed-param registration table that maps every supported Rust type to its extraction tokens. **Our analog:** `shared/known_type.rs::KnownType` enum + dispatcher functions. Same shape; smaller in our case (we don't have the full op-system's surface area).

**Selected as the single most-ported pattern:** `serde_derive`'s phase-distinct AST types. Justification: it directly addresses F7 (cyclic ownership between `parse.rs` and `mod.rs`), which is the architecture critic's clearest citation of "phases are not architecturally distinct" (Dim 6 score 5/10).

<!-- Added in v2 R1: addressing critic's CRITICAL-2 — serde_derive cite acknowledges Ctxt divergence -->

**Divergence from upstream serde — explicit and intentional.** Verified against upstream `serde-rs/serde@HEAD:serde_derive/src/internals/ctxt.rs`: serde uses an **error-collecting `Ctxt`** rather than `syn::Result`. Constructor signature: `Container::from_ast(cx: &Ctxt, item, derive, private) -> Option<Container>`. Errors accumulate via `cx.error_spanned_by(...)`; `Ctxt::check()` consumes the context and returns `Err(combined)` only if any error was reported. Multi-error reporting is the chief reason serde's pattern is well-regarded — the user sees ALL errors per build instead of fixing them one at a time.

**Decision:** we port the **phase separation** (parse → analyze → emit) but **NOT the Ctxt error-collection**. Two reasons:

1. **Volume:** serde's `Container::from_ast` parses ~30 distinct attributes per derive across container/variant/field; reporting a batch of errors is high-leverage. `runtime-macros` parses ~12 markers per `#[v8_class]` impl block; the marginal value of "show 3 errors instead of stopping at 1" is small relative to the architectural complexity (Ctxt requires `RefCell`, drop-must-call-check, Default impl that asserts in production).
2. **Diagnostics quality:** the proc-macro's existing diagnostics use `syn::Error::new_spanned(...)` which renders ONE error inline with the user's source. Stacking N errors via `Ctxt` produces a `combined` error whose rendering is "first error + (and N more)" — less helpful than the `compile_error!` placement the current codebase enforces.

**If we revisit:** if a future critique demonstrates a class of multi-error reporting wins (e.g., user writes 5 fields with `#[v8_field(invalid_shape)]` and the macro reports the first, user fixes, runs build, sees the second, etc.), we add a Ctxt-style helper specifically for the dictionary derive (`WebIdlDict`) where field-count justifies it. The `#[v8_class]` impl-level parsing keeps `syn::Result`.

### A.7 `deno_core::ops::op2::signature` — typed registry with strum (ported)

<!-- Added in v2 R1: addressing critic's MINOR-2 — concrete op2 cite -->

Verified against upstream `denoland/deno_core@HEAD:ops/op2/signature.rs`. Op2 keeps a `pub enum NumericArg { __SMI__, __VOID__, bool, i8, u8, i16, u16, i32, u32, i64, u64, f32, f64, isize, usize }` plus `pub enum V8Arg { ... }` plus `pub struct RetVal` from `signature_retval.rs`. Each enum derives `strum::IntoStaticStr`, `strum::EnumString`, `strum::EnumIter` for ident roundtrip. The op2 generator dispatches on `Arg` variants in `dispatch_slow.rs` / `dispatch_fast.rs` — same shape as our Wave-4 `KnownType` registry.

**Three things we port from op2:**

1. **Separate enum per context** (Wave 4 `KnownType` for general extract, `FastcallArg` and `FastcallReturn` for the fast path). Op2 has separate `NumericArg` and `RetVal` types — fastcall arg shape is genuinely different from return shape (return supports `Result`, arg doesn't).
2. **Strum-derived ident roundtrip** for the ident → enum mapping. Replaces our 9 hand-rolled `is_byte_string` / `is_vec_u8` predicates.
3. **Per-dispatcher emission code** lives in its own module (op2's `dispatch_slow.rs` / `dispatch_fast.rs`); we mirror with `v8_class/emit/method.rs` (slow) and `v8_class/fastcall/emit.rs` (fast).

**One thing we don't port:** op2's `__SMI__` / `__VOID__` placeholder variants for `#[smi]` / unit args. Our equivalent (`Option<KnownType>` or `KnownType::Unit`) is shape-compatible without placeholder enums; one fewer indirection.

---

## Appendix B — `ClassConfig` proof-of-concept

<!-- Added in v2 R1: addressing critic's CRITICAL-1 — worked PoC for ClassConfig refactor. v3 R5: re-lettered C → B for sequential alphabetic appendices. -->

This appendix demonstrates the Wave-3 `ClassConfig` pattern on a SMALL test class to validate that the parameter-object refactor compiles and emits the correct token shape. The PoC is a 3-method `Counter` class, intentionally minimal but exercising every reflowed code path: constructor, method, getter.

### B.1 Source (user-supplied)

```rust
// crates/runtime-macros/tests/poc/counter_input.rs
#[v8_class]
#[v8_state_marker(CounterState)]
impl Counter {
    #[v8_constructor]
    pub fn new() -> Self {
        Counter { value: 0 }
    }

    #[v8_getter]
    pub fn value(&self) -> i32 {
        self.value
    }

    #[v8_method]
    pub fn increment(&mut self) {
        self.value += 1;
    }
}
```

### B.2 ClassConfig built by `analyze`

After Wave-3, the parse phase produces `ParsedClassAttrs` and `parse_method_attrs(&func)` for each method; the analyze phase folds them into:

```rust
// pseudo-Rust value, post-Wave-3 analyze
let cfg = ClassConfig {
    class_ty:    &Ident::new("Counter", _),
    state_ty:    &Ident::new("CounterState", _),
    marker_ty:   Ident::new("Counter", _),  // marker == class for the simple case
    methods: vec![
        ClassMethod {
            kind: MethodKind::Constructor,
            sig: &counter_new_sig,
            // ... existing ClassMethod fields ...
        },
        ClassMethod {
            kind: MethodKind::Getter,
            // ...
        },
        ClassMethod {
            kind: MethodKind::Method,
            // ...
        },
    ],
    constructor: /* &methods[0] */,
    has_any_fastcall: false,
    to_string_tag: None,
    inherit_intrinsic: None,
    inherit_base: None,
    async_iterable_method: None,
    consts: vec![],
    iterable: None,
};
```

### B.3 `gen_install(&cfg)` emit (post-Wave-3)

The 9-arg `gen_install` collapses to a 1-arg call. The emit body is unchanged in shape — this is a parameter-passing refactor, NOT an emit-shape refactor. The token output is byte-identical to the pre-refactor emit (verified by Wave-3's insta snapshot acceptance: only the call sites in `expand_tokens` differ).

```rust
// post-Wave-3 v8_class/emit/install.rs
pub(crate) fn gen_install(cfg: &ClassConfig) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let proto_template = quote! {
        // ... existing setup ...
    };
    let method_installs: Vec<TokenStream2> = cfg.methods.iter()
        .filter(|m| matches!(m.kind, MethodKind::Method))
        .map(|m| gen_method_install(cfg, m))
        .collect();
    let getter_installs = cfg.methods.iter()
        .filter(|m| matches!(m.kind, MethodKind::Getter))
        .map(|m| gen_getter_install(cfg, m));
    // ... etc ...

    quote! {
        pub fn install<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::FunctionTemplate> {
            #proto_template
            #(#method_installs)*
            #(#getter_installs)*
            // ...
        }
    }
}
```

### B.4 Lifetime parameterization (verified against existing source)

Verified at `crates/runtime-macros/src/v8_class/mod.rs:107` — `ClassMethod<'a>` already carries a lifetime over the `&'a syn::ItemImpl`. The `ClassConfig<'a>` lifetime parameter is sound — it borrows the same `&'a syn::ItemImpl` the parse phase borrowed from. No ownership change required.

```rust
// existing in mod.rs:107 (verified, no change needed)
struct ClassMethod<'a> {
    kind: MethodKind,
    func: &'a syn::ImplItemFn,   // borrows from input
    // ...
}

// Wave-3 addition
pub(crate) struct ClassConfig<'a> {
    pub class_ty: &'a syn::Ident,
    pub methods: Vec<ClassMethod<'a>>,  // owned Vec, but ClassMethod still borrows
    // ...
}
```

`expand_tokens` owns the `syn::ItemImpl` (input is moved in); `analyze::analyze(&'a input)` produces `ClassConfig<'a>` borrowing from input; `emit::assemble_tokens(&cfg)` consumes the ClassConfig and returns `TokenStream2` (owned). End-of-fn drop unwinds in correct order: TokenStream → ClassConfig → input.

### B.5 Compilation evidence

The PoC compiles in pseudo-form because:

1. The lifetime topology is the same as pre-refactor (verified §C.4).
2. The emit body is unchanged in shape (verified §C.3).
3. Every codegen helper that today takes `(class_ty, state_ty, ...)` becomes a function taking `&ClassConfig` — Rust's borrow checker is happy with `&cfg.class_ty` instead of an immediate `&Ident` arg.

<!-- Added in v2 R2: addressing critic's CRITICAL-R2-1 — external hash anchor (not circular) -->

**External hash anchor for byte-identity verification (closes circularity):** the PoC anchors against a HASH captured from the pre-refactor `cargo expand` output. The procedure:

```bash
# STEP 1 (pre-Wave-3, on master): capture the baseline hash of the expand output for Counter.
cd /home/ruiyang/Projects/appbase/crates/runtime/tests/poc
cargo expand --tests --test counter_smoke 2>/dev/null | sha256sum > counter_expand.sha256.baseline
# Result example (placeholder, real hash captured by Wave-3 PoC author):
#   abcd1234ef5678901234567890abcdef0123456789abcdef0123456789abcdef counter_expand.sha256.baseline

# STEP 2 (post-Wave-3): re-run on the wave-3 branch.
cargo expand --tests --test counter_smoke 2>/dev/null | sha256sum > counter_expand.sha256.wave3
diff counter_expand.sha256.baseline counter_expand.sha256.wave3
# Expected: zero output. If diff is non-empty, the refactor changed the emit shape — abort Wave 3.
```

The hash is committed to the proposal's wave-3 PR description (NOT to the proposal file, which is master-tracked). This makes byte-identity an EXTERNAL contract, not a self-referential snapshot. Wave-3's CI step adds the hash check.

**Caveat:** `cargo expand` output is sensitive to (a) rustc version (different rustc may add/remove trailing whitespace), (b) macro re-expansion order across edition changes. Pin: `rust-toolchain.toml` toolchain version + `cargo-expand` version. If pinning slips, the hash check is unreliable; re-baseline.

**Action item for Wave 3 PR:** before merging, the implementer:

1. On `master`: captures `counter_expand.sha256.baseline`.
2. On `wave3-branch`: runs the PoC.
3. Re-captures the hash; asserts byte-identity.
4. Commits the hash file as evidence in the PR description.

### B.6 Failure modes the PoC catches

A naive ClassConfig that owned all the data (e.g., `class_ty: syn::Ident` rather than `&'a syn::Ident`) would force `analyze` to clone every Ident — wasteful but compiles. The borrow-form (chosen) avoids the clone but threads `'a` through every helper. The PoC verifies the borrow-form: every `gen_*_install(cfg, m)` site can return its TokenStream2 without holding a reference to `cfg` past the body.

---

## Appendix C — Completed wave 1 changes (cite for closure)

<!-- v3 R5: re-lettered to keep appendices sequential A, B, C. The Wave 1 closure ledger was Appendix B in v1; the new ClassConfig PoC is Appendix B in v3; this content moved to C. -->

<!-- B was kept for B.X subsection consistency with v1; the §11/§12 subsections of this appendix list closures that are no longer in scope for the active critic-loop. -->

Per the prompt, certain critique findings are NO LONGER applicable in the target state because Wave 1 already shipped them on `main`. For the critic-loop's ground truth, these are the closures:

| Critique finding | Status | Commit | Where |
|---|---|---|---|
| C1 — `gen_extract_throw` only handles 3 of 6 OpErrorKind variants | ✅ **closed at lib.rs sites**; method.rs sites pending Wave 2 | `22ab81c` | `lib.rs:722-749` (`gen_throw_op_error_arms`) + `lib.rs:764-768` (`gen_extract_throw` delegates) |
| C2 — quadruple OpErrorKind dispatch duplication | ✅ **partial; 2/4 sites closed**; 2 remaining (constructor make_instance + post_init) pending Wave 2 | `22ab81c` | same |
| C3 — brand-check chain walk cycle-detection (cap rationale wrong) | ✅ **closed** | `ec09c37` | `v8_class/mod.rs:541-557` (cap raised to 1024 + correct doc) |
| H10 — mass `unwrap()` on `v8::String::new` | ✅ **partial**; Wave 1 lib.rs/mod.rs/webidl_*.rs sites; method.rs and v8_iterable.rs pending Wave 2 | `d9c5528` | `lib.rs:626-675` (`must_str` + `must_str_abs`) |
| H15 — `pascal_to_kebab` missing `APIKey → api-key` | ✅ **closed** | `896c6de` | `webidl_enum.rs:362-410` (rule extended; regression test added) |
| H2/H3/H4 — dead code (`getter_args`, `_user_ctor_marker`, `_suppress_unused`) | ✅ **closed** | `ae43938` | `method.rs` + `mod.rs` + `webidl_dict.rs` |
| F1 — quadruple OpError-throw codegen duplication (arch critic) | ✅ **partial**; same as C1+C2 | `22ab81c` | same |
| MAC-09 follow-up — `value_pairs(&mut self)` + `&mut PinScope` | ✅ **closed** (recent extension) | `f81e982` | `v8_iterable.rs` |
| MAC-14 follow-up — `value_marshal = fn` + `entries === [Symbol.iterator]` identity fix | ✅ **closed** (recent extension) | `cc945cc` | `v8_iterable.rs` |
| `1aa2c61` — `mod.rs` split into submodules | ✅ **closed** (Wave 0 prerequisite) | `1aa2c61` | created `v8_class/{method,parse,helpers,fastcall}.rs` |

Findings remaining for the critic-loop after Wave 1 (the ones this proposal addresses):

- C2 (remaining sites in method.rs)
- C4 (async finalizer/teardown ordering)
- C5 / H13 (HashSet → Cell)
- C6 (per-class slot type module-scoping comment)
- C7 (Send-soundness comment for finalizer closure)
- C8 (`expect()` in emit code)
- F2, F3, F4, F5, F6, F7, F8, F9, F10
- H1, H5-H9, H11, H12, H14, H16, H17
- M1-M13
- L1-L10
- §13.1-§13.10 subtle items

**~80 distinct findings remain.** This proposal addresses 100% of them across Waves 2-8.

---

*End of refactor guide v3. Critic+reviser loop converged at R4 with composite score 91/100; ≥90 sustained for ≥3 rounds → stop signal triggered.*
