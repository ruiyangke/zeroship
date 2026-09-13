//! Procedure capabilities belong to the V8 continuation invoking native work.
//!
//! The frame is stored under an isolate-private key in continuation-preserved
//! embedder data. Updating a frame clones the surrounding map so pending
//! requests and user `AsyncLocalStorage` stores keep their captured context.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::node::async_hooks::als::{clone_map, read_context_map};

/// Procedure kind checked by native operations before they enqueue work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcedureKind {
    Query,
    Mutation,
    Action,
    Stream,
    Subscription,
}

impl ProcedureKind {
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "query" => Some(Self::Query),
            "mutation" => Some(Self::Mutation),
            "action" => Some(Self::Action),
            "stream" => Some(Self::Stream),
            "subscription" => Some(Self::Subscription),
            _ => None,
        }
    }

    fn as_wire(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Mutation => "mutation",
            Self::Action => "action",
            Self::Stream => "stream",
            Self::Subscription => "subscription",
        }
    }
}

struct FrameKey(v8::Global<v8::Symbol>);

fn frame_key<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Symbol> {
    if let Some(key) = scope.get_slot::<FrameKey>() {
        return v8::Local::new(scope, key.0.clone());
    }
    let description = v8::String::new(scope, "zs:ProcedureFrame").unwrap();
    let key = v8::Symbol::new(scope, Some(description));
    let saved = v8::Global::new(scope, key);
    scope.set_slot(FrameKey(saved));
    key
}

fn current_frame<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Array>> {
    let map = read_context_map(scope)?;
    let key = frame_key(scope);
    let value = map.get(scope, key.into())?;
    v8::Local::<v8::Array>::try_from(value).ok()
}

/// Opaque storage owned by the current procedure frame. Native plugins may
/// attach isolate-private state whose lifetime must follow that continuation.
pub fn current_procedure_frame<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Option<v8::Local<'s, v8::Object>> {
    current_frame(scope).map(Into::into)
}

fn set_frame<'s>(scope: &mut v8::PinScope<'s, '_>, value: v8::Local<'s, v8::Value>) {
    let next = match read_context_map(scope) {
        Some(map) => clone_map(scope, map),
        None => v8::Map::new(scope),
    };
    let key = frame_key(scope);
    if value.is_null_or_undefined() {
        next.delete(scope, key.into());
    } else {
        next.set(scope, key.into(), value);
    }
    scope.set_continuation_preserved_embedder_data(next.into());
}

/// Kind carried by the currently executing continuation, including after await.
pub fn current_kind(scope: &mut v8::PinScope) -> Option<ProcedureKind> {
    let frame = current_frame(scope)?;
    let kind = frame.get_index(scope, 0)?;
    if !kind.is_string() {
        return None;
    }
    ProcedureKind::from_wire(&kind.to_rust_string_lossy(scope))
}

/// Identity of the current procedure frame. The caller's identity is restored
/// after a nested invocation; different isolates never share a frame identity.
pub fn dispatch_generation(scope: &mut v8::PinScope) -> u64 {
    let Some(frame) = current_frame(scope) else {
        return 0;
    };
    let Some(value) = frame.get_index(scope, 1) else {
        return 0;
    };
    v8::Local::<v8::BigInt>::try_from(value)
        .ok()
        .map_or(0, |value| value.u64_value().0)
}

fn enter_frame(scope: &mut v8::PinScope, kind: Option<ProcedureKind>) -> u64 {
    static NEXT_FRAME: AtomicU64 = AtomicU64::new(1);
    // Internal JS dispatcher tokens must round-trip through Number exactly.
    const MAX_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;
    let generation = NEXT_FRAME
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            (value < MAX_SAFE_INTEGER).then_some(value + 1)
        })
        .expect("procedure frame identity exhausted");
    let parent = current_frame(scope)
        .map(Into::into)
        .unwrap_or_else(|| v8::null(scope).into());
    let kind = kind
        .map(|kind| v8::String::new(scope, kind.as_wire()).unwrap().into())
        .unwrap_or_else(|| v8::null(scope).into());
    let id = v8::BigInt::new_from_u64(scope, generation);
    let frame = v8::Array::new_with_elements(scope, &[kind, id.into(), parent]);
    set_frame(scope, frame.into());
    generation
}

/// Enter a native procedure call, restoring the exact caller context when the
/// synchronous V8 call returns. Pending continuations retain their own frame.
pub fn with_kind<'s, R>(
    scope: &mut v8::PinScope<'s, '_>,
    kind: ProcedureKind,
    body: impl FnOnce(&mut v8::PinScope<'s, '_>) -> R,
) -> R {
    let previous = scope.get_continuation_preserved_embedder_data();
    let previous = v8::Global::new(scope, previous);
    enter_frame(scope, Some(kind));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(scope)));
    let previous = v8::Local::new(scope, previous);
    scope.set_continuation_preserved_embedder_data(previous);
    match result {
        Ok(value) => value,
        Err(error) => std::panic::resume_unwind(error),
    }
}

fn enter_kind_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let value = args.get(0);
    let kind = if value.is_string() {
        ProcedureKind::from_wire(&value.to_rust_string_lossy(scope))
    } else {
        None
    };
    let kind = kind.or_else(|| current_kind(scope));
    rv.set_double(enter_frame(scope, kind) as f64);
}

fn exit_kind_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let value = args.get(0);
    if !value.is_number() {
        return;
    }
    let Some(token) = value.number_value(scope) else {
        return;
    };
    if token < 1.0 || token.fract() != 0.0 || token as u64 != dispatch_generation(scope) {
        return;
    }
    if let Some(frame) = current_frame(scope)
        && let Some(parent) = frame.get_index(scope, 2)
    {
        set_frame(scope, parent);
    }
}

fn clear_kind_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set_double(enter_frame(scope, None) as f64);
}

/// Install callbacks captured by the internal dispatcher and dev transport.
/// Creator evaluation starts after the bootstrap removes these globals.
pub fn install_globals<'s>(scope: &mut v8::PinScope<'s, '_>, global: v8::Local<v8::Object>) {
    let f = v8::Function::new(scope, enter_kind_callback).unwrap();
    let key = v8::String::new(scope, "__zsEnterKind").unwrap();
    global.set(scope, key.into(), f.into());
    let f = v8::Function::new(scope, exit_kind_callback).unwrap();
    let key = v8::String::new(scope, "__zsExitKind").unwrap();
    global.set(scope, key.into(), f.into());
    let f = v8::Function::new(scope, clear_kind_callback).unwrap();
    let key = v8::String::new(scope, "__zsClearKind").unwrap();
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

    let message =
        format!("capability_violation: {wrapper} handlers cannot call {violated}. {remediation}");
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
        assert_eq!(
            ProcedureKind::from_wire("query"),
            Some(ProcedureKind::Query)
        );
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

    fn in_context(body: impl FnOnce(&mut v8::PinScope<'_, '_>)) {
        crate::init_v8();
        let mut isolate = v8::Isolate::new(Default::default());
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        body(scope);
    }

    #[test]
    fn outside_dispatch_has_no_frame() {
        in_context(|scope| {
            assert_eq!(current_kind(scope), None);
            assert_eq!(dispatch_generation(scope), 0);
        });
    }

    #[test]
    fn native_call_restores_the_callers_frame() {
        in_context(|scope| {
            with_kind(scope, ProcedureKind::Action, |scope| {
                let parent = dispatch_generation(scope);
                assert_ne!(parent, 0);
                with_kind(scope, ProcedureKind::Query, |scope| {
                    assert_eq!(current_kind(scope), Some(ProcedureKind::Query));
                    assert_ne!(dispatch_generation(scope), parent);
                });
                assert_eq!(current_kind(scope), Some(ProcedureKind::Action));
                assert_eq!(dispatch_generation(scope), parent);
            });
            assert_eq!(current_kind(scope), None);
            assert_eq!(dispatch_generation(scope), 0);
        });
    }

    #[test]
    fn native_call_restores_context_after_panic() {
        in_context(|scope| {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_kind(scope, ProcedureKind::Query, |scope| {
                    assert_eq!(current_kind(scope), Some(ProcedureKind::Query));
                    panic!("simulated native caller panic");
                });
            }));
            assert!(result.is_err());
            assert_eq!(current_kind(scope), None);
            assert_eq!(dispatch_generation(scope), 0);
        });
    }

    #[test]
    fn successive_calls_have_distinct_frame_identities() {
        in_context(|scope| {
            let first = with_kind(scope, ProcedureKind::Query, dispatch_generation);
            let second = with_kind(scope, ProcedureKind::Query, dispatch_generation);
            assert_ne!(first, second);
            assert_ne!(first, 0);
            assert_ne!(second, 0);
        });
    }

    #[test]
    fn sibling_contexts_keep_immutable_frames() {
        in_context(|scope| {
            let capture = |scope: &mut v8::PinScope<'_, '_>| {
                (
                    dispatch_generation(scope),
                    crate::core::invocation::capture_context(scope),
                )
            };
            let (query_id, query) = with_kind(scope, ProcedureKind::Query, capture);
            let (mutation_id, mutation) = with_kind(scope, ProcedureKind::Mutation, capture);
            for (context, kind, id) in [
                (&query, ProcedureKind::Query, query_id),
                (&mutation, ProcedureKind::Mutation, mutation_id),
                (&query, ProcedureKind::Query, query_id),
            ] {
                crate::core::invocation::with_captured_context(scope, context, |scope| {
                    assert_eq!(current_kind(scope), Some(kind));
                    assert_eq!(dispatch_generation(scope), id);
                });
            }
            assert_eq!(current_kind(scope), None);
        });
    }
}
