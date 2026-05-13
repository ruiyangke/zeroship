//! B3 runtime capability enforcement — the thread-local `CURRENT_KIND`
//! marker + RAII guard + JS-side native callbacks (`__zsEnterKind` /
//! `__zsExitKind`) that the synthetic SSR entry calls around each user
//! procedure invocation.
//!
//! ## Threat model
//!
//! Defense-in-depth on top of the TypeScript types shipped in commit
//! `7b8074e`. The TS layer catches misuse at compile time:
//!
//!   - `query()` handler tries `ctx.db.users.create(...)` → TS2339
//!   - `mutation()` handler tries `await fetch(...)` → TS2339
//!
//! The runtime layer enforces the same boundaries at request time, so
//! handlers compiled without strict type-checking (e.g. plain JS) still
//! hit the rail. Consumers:
//!
//!   - `crates/plugin-db/src/callbacks.rs` — write callbacks refuse
//!     when `current_kind() == Some(Query)`.
//!   - `crates/runtime/src/web/fetch/mod.rs` — fetch callback refuses
//!     when `current_kind() == Some(Mutation)`.
//!
//! ## Plumbing
//!
//! The synthetic SSR entry (vite-plugin's `_zsRpc`) knows the procedure
//! `kind` synchronously from `fn.config.kind` / `fn.__zsKind`. Around
//! the user handler invocation it calls:
//!
//!   const tok = globalThis.__zsEnterKind("query");
//!   try { return await fn(input, ctx); }
//!   finally { globalThis.__zsExitKind(tok); }
//!
//! Enter pushes the previous kind onto a per-thread stack and sets
//! `CURRENT_KIND`. Exit pops back. Nested calls (an `action` doing
//! `ctx.runMutation` which internally dispatches another procedure)
//! restore correctly. A bad token (mismatched / out-of-order) is a
//! no-op — defense in depth, not a strict protocol.
//!
//! ## Why not a Rust-side guard around `call_rpc_inner`?
//!
//! The Rust dispatcher sees only the wire id, not the procedure kind —
//! the kind lives in `fn.config` on the JS side. The SSR entry is the
//! one place that resolves id → fn and has cheap access to kind. JS
//! invoking the marker is the right layer; this is defense-in-depth
//! over already-typed TS, not a sandbox boundary.

#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};

/// Procedure kind, runtime-local. Wire shape lives in
/// `zeroship_bundle::ProcedureKind`; we don't depend on bundle here to
/// keep the dependency graph flat (bundle pulls in tar/zstd/compio-fs;
/// runtime is upstream of bundle).
///
/// `Action`, `Stream`, `Subscription` all map to "no capability
/// restriction" — they're listed individually only so callers can
/// inspect the active kind for diagnostics / future gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcedureKind {
    Query,
    Mutation,
    Action,
    Stream,
    Subscription,
}

impl ProcedureKind {
    /// Parse a wire-kind string. Returns `None` for unknown variants
    /// (silently — the marker is best-effort; on unknown kinds the
    /// guard is simply not installed and capability checks pass).
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "query" => Some(Self::Query),
            "mutation" => Some(Self::Mutation),
            "action" => Some(Self::Action),
            "stream" => Some(Self::Stream),
            "subscription" => Some(Self::Subscription),
            _ => None,
        }
    }
}

thread_local! {
    /// Per-thread active kind. `None` outside any RPC dispatch frame.
    static CURRENT_KIND: Cell<Option<ProcedureKind>> = const { Cell::new(None) };

    /// Stack of saved kinds — pushed on `enter`, popped on matching
    /// `exit`. Used by the JS-side enter/exit callbacks so nested
    /// procedure calls (e.g. action → runMutation → mutation handler)
    /// restore the outer kind correctly. The token returned to JS is
    /// the stack depth before push (`u32` rather than `usize` so it
    /// round-trips through a V8 `Integer` cleanly).
    static KIND_STACK: RefCell<Vec<Option<ProcedureKind>>> =
        const { RefCell::new(Vec::new()) };
}

/// Snapshot of `CURRENT_KIND`. `None` outside any RPC dispatch.
#[inline]
pub fn current_kind() -> Option<ProcedureKind> {
    CURRENT_KIND.with(|c| c.get())
}

/// RAII guard. Sets `CURRENT_KIND` to `kind` on construct, restores
/// the prior value on drop — survives panics in the handler. Use the
/// Rust-side guard from any Rust code path that needs the marker
/// transactionally; the JS-side `__zsEnterKind` is the equivalent
/// for the SSR entry.
pub struct KindGuard {
    previous: Option<ProcedureKind>,
}

impl KindGuard {
    pub fn enter(kind: ProcedureKind) -> Self {
        let previous = CURRENT_KIND.with(|c| {
            let prev = c.get();
            c.set(Some(kind));
            prev
        });
        Self { previous }
    }
}

impl Drop for KindGuard {
    fn drop(&mut self) {
        let prev = self.previous;
        CURRENT_KIND.with(|c| c.set(prev));
    }
}

// ---------------------------------------------------------------------------
// V8 native callbacks: __zsEnterKind / __zsExitKind
// ---------------------------------------------------------------------------

/// `globalThis.__zsEnterKind(kindStr): number`
///
/// Pushes the current kind onto the stack and sets `CURRENT_KIND` to
/// the parsed kind. Returns a numeric token that `__zsExitKind` uses to
/// restore. Unknown / non-string arguments are silently ignored — the
/// returned token still works (best-effort).
fn enter_kind_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let kind = if args.length() >= 1 {
        let v = args.get(0);
        if v.is_string() {
            ProcedureKind::from_wire(&v.to_rust_string_lossy(scope))
        } else {
            None
        }
    } else {
        None
    };

    let token = KIND_STACK.with(|s| {
        let prev = CURRENT_KIND.with(|c| c.get());
        let mut stack = s.borrow_mut();
        let depth = stack.len();
        stack.push(prev);
        depth as u32
    });
    if let Some(k) = kind {
        CURRENT_KIND.with(|c| c.set(Some(k)));
    } else {
        // Unknown kind → leave CURRENT_KIND as-is. The stack push is
        // still recorded so the matching exit pops correctly. This keeps
        // the protocol symmetric (every enter has an exit) without
        // changing the active gate.
    }

    rv.set(v8::Integer::new_from_unsigned(scope, token).into());
}

/// `globalThis.__zsExitKind(token: number): void`
///
/// Pops the stack down to (and not including) `token`, restoring
/// `CURRENT_KIND` to the value saved at that depth. Out-of-range or
/// non-numeric tokens are treated as no-ops — defense in depth, not a
/// strict protocol.
fn exit_kind_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    if args.length() < 1 {
        return;
    }
    let v = args.get(0);
    let Some(tok_i64) = v.integer_value(scope) else {
        return;
    };
    if tok_i64 < 0 {
        return;
    }
    let tok = tok_i64 as usize;

    KIND_STACK.with(|s| {
        let mut stack = s.borrow_mut();
        if tok >= stack.len() {
            // Out-of-range token — the matching enter must have
            // happened on a different frame, or two exits ran for one
            // enter. Either way: clamp to the current depth and do
            // nothing.
            return;
        }
        // Pop everything down to depth `tok`, restoring CURRENT_KIND
        // from the value saved AT depth `tok` (the value present
        // BEFORE the matching enter pushed).
        let restored = stack[tok];
        stack.truncate(tok);
        CURRENT_KIND.with(|c| c.set(restored));
    });
}

/// Wire `__zsEnterKind` / `__zsExitKind` onto `globalThis`. Called from
/// `crate::rpc::dispatch::install_globals`.
pub fn install_globals<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let f = v8::Function::new(scope, enter_kind_callback).unwrap();
    let key = v8::String::new(scope, "__zsEnterKind").unwrap();
    global.set(scope, key.into(), f.into());

    let f = v8::Function::new(scope, exit_kind_callback).unwrap();
    let key = v8::String::new(scope, "__zsExitKind").unwrap();
    global.set(scope, key.into(), f.into());
}

// ---------------------------------------------------------------------------
// capability_violation error envelope
// ---------------------------------------------------------------------------

/// Build a plain V8 object matching the `capability_violation` error
/// envelope from the B3 proposal:
///
/// ```json
/// {
///   "code": "capability_violation",
///   "wrapper": "query"  | "mutation" | ...,
///   "violated": "ctx.db.insert" | "fetch" | ...,
///   "remediation": "..." ,
///   "name": "Error",
///   "message": "<wrapper> handlers cannot call <violated> ..."
/// }
/// ```
///
/// `.name` + `.message` are populated so the existing dispatch error
/// path (which JSONifies these into the response body) renders the
/// envelope as the response. `.status` is set to 500 — the call shape
/// itself wasn't malformed (400 would be misleading); the handler
/// asked the runtime to do something the wrapper forbids.
pub fn build_capability_violation<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    wrapper: &str,
    violated: &str,
    remediation: &str,
) -> v8::Local<'s, v8::Object> {
    let obj = v8::Object::new(scope);

    let set_str = |scope: &mut v8::PinScope<'s, '_>, k: &str, v: &str| {
        let key = v8::String::new(scope, k).unwrap();
        let val = v8::String::new(scope, v).unwrap();
        obj.set(scope, key.into(), val.into());
    };

    let message = format!(
        "capability_violation: {wrapper} handlers cannot call {violated}. {remediation}"
    );
    set_str(scope, "name", "Error");
    set_str(scope, "message", &message);
    set_str(scope, "code", "capability_violation");
    // The B3 envelope wants wrapper/violated/remediation as
    // top-level body fields. Set them as own properties — JS-side
    // catch blocks that copy own props (the SSR-entry default error
    // body builder) pick them up. The Rust-side dispatcher's error
    // extraction only knows about {message,name,stack,status,code,
    // details,retryable}, so ALSO mirror these three into a `details`
    // object: that round-trips through the dispatcher unchanged
    // (it's `JSON.stringify`'d into `details_json`) and lets clients
    // that only ever see the kernel-rendered error envelope still
    // observe the full B3 shape.
    set_str(scope, "wrapper", wrapper);
    set_str(scope, "violated", violated);
    set_str(scope, "remediation", remediation);

    // `details` mirror — keeps the structured fields reachable when
    // the dispatcher renders the error.
    let details = v8::Object::new(scope);
    {
        let k = v8::String::new(scope, "wrapper").unwrap();
        let v = v8::String::new(scope, wrapper).unwrap();
        details.set(scope, k.into(), v.into());
    }
    {
        let k = v8::String::new(scope, "violated").unwrap();
        let v = v8::String::new(scope, violated).unwrap();
        details.set(scope, k.into(), v.into());
    }
    {
        let k = v8::String::new(scope, "remediation").unwrap();
        let v = v8::String::new(scope, remediation).unwrap();
        details.set(scope, k.into(), v.into());
    }
    let details_key = v8::String::new(scope, "details").unwrap();
    obj.set(scope, details_key.into(), details.into());

    let key = v8::String::new(scope, "status").unwrap();
    let val = v8::Integer::new_from_unsigned(scope, 500);
    obj.set(scope, key.into(), val.into());

    obj
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `from_wire` mirrors the SDK's `__zsKind` tag values.
    #[test]
    fn from_wire_parses_all_kinds() {
        assert_eq!(ProcedureKind::from_wire("query"), Some(ProcedureKind::Query));
        assert_eq!(
            ProcedureKind::from_wire("mutation"),
            Some(ProcedureKind::Mutation)
        );
        assert_eq!(
            ProcedureKind::from_wire("action"),
            Some(ProcedureKind::Action)
        );
        assert_eq!(
            ProcedureKind::from_wire("stream"),
            Some(ProcedureKind::Stream)
        );
        assert_eq!(
            ProcedureKind::from_wire("subscription"),
            Some(ProcedureKind::Subscription)
        );
        assert_eq!(ProcedureKind::from_wire("procedure"), None);
        assert_eq!(ProcedureKind::from_wire(""), None);
    }

    /// Baseline: no guard means no kind.
    #[test]
    fn current_kind_outside_dispatch_is_none() {
        // Note: tests share a thread, so we reset the stack first to
        // guard against pollution from earlier failed tests.
        KIND_STACK.with(|s| s.borrow_mut().clear());
        CURRENT_KIND.with(|c| c.set(None));
        assert_eq!(current_kind(), None);
    }

    /// `KindGuard::enter` sets the kind; drop restores `None`.
    #[test]
    fn b3_runtime_kind_guard_basic() {
        KIND_STACK.with(|s| s.borrow_mut().clear());
        CURRENT_KIND.with(|c| c.set(None));
        {
            let _g = KindGuard::enter(ProcedureKind::Query);
            assert_eq!(current_kind(), Some(ProcedureKind::Query));
        }
        assert_eq!(current_kind(), None);
    }

    /// Nested guards stack — inner kind overrides; outer kind restores
    /// on inner drop.
    #[test]
    fn b3_runtime_nested_calls_preserve_kind() {
        KIND_STACK.with(|s| s.borrow_mut().clear());
        CURRENT_KIND.with(|c| c.set(None));
        let _outer = KindGuard::enter(ProcedureKind::Action);
        assert_eq!(current_kind(), Some(ProcedureKind::Action));
        {
            let _inner = KindGuard::enter(ProcedureKind::Mutation);
            assert_eq!(current_kind(), Some(ProcedureKind::Mutation));
        }
        assert_eq!(current_kind(), Some(ProcedureKind::Action));
    }

    /// A panicking handler must still restore CURRENT_KIND. The RAII
    /// guard runs in the panic's unwinding pass; this exercises that
    /// path.
    #[test]
    fn b3_runtime_kind_guard_restores_on_panic() {
        KIND_STACK.with(|s| s.borrow_mut().clear());
        CURRENT_KIND.with(|c| c.set(None));
        let result = std::panic::catch_unwind(|| {
            let _g = KindGuard::enter(ProcedureKind::Query);
            assert_eq!(current_kind(), Some(ProcedureKind::Query));
            panic!("simulated handler panic");
        });
        assert!(result.is_err(), "panic should propagate");
        assert_eq!(
            current_kind(),
            None,
            "CURRENT_KIND must restore after panic unwinds the guard"
        );
    }
}
