//! Invocation-local request identity carried by V8 continuations.
//!
//! V8 drains an isolate-wide microtask queue. The Rust field describing the
//! host turn that initiated a checkpoint therefore cannot identify every JS
//! continuation that runs inside it. This module stores the owning request and
//! its authenticated user in `ContinuationPreservedEmbedderData` so V8 switches
//! them together with each continuation.
//!
//! A request is carried by one token object, minted when the request is
//! dispatched and entered by every turn of it: its dispatch, the settlement of
//! each op it issued, and the collection of its result. The token holds the
//! request's [`RequestControl`]. It rides in the context rather than in a table
//! keyed by request id, because a request's continuations outlive its entry in
//! the runtime: an op it issued can settle after the request was answered, and
//! the code that runs then is still that request's. A continuation that holds
//! the context holds the token, and a request whose last continuation is
//! collected leaves nothing behind.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::{self, Rc};

use crate::channel::CancelFlag;
use crate::node::async_hooks::als::{clone_map, read_context_map};
use crate::state::SharedState;

// ---------------------------------------------------------------------------
// The request a token carries
// ---------------------------------------------------------------------------

/// An action waiting on a request's early end.
type EndAction = Box<dyn FnOnce()>;

/// What the runtime and everything a request started share about it: whether
/// it has ended early, the commits it has in flight, and what waits on its
/// end.
pub struct RequestControl {
    id: u64,
    /// The host's flag, which the host sets to cancel the request.
    cancel: CancelFlag,
    phase: Cell<Phase>,
    /// Commits whose check passed and whose outcome is not known yet.
    commits: Cell<usize>,
    on_end: RefCell<Vec<(u64, EndAction)>>,
    next_listener: Cell<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Not answered yet. Its host can still end it early by setting its flag.
    Running,
    /// Answered with its own result. Code it left running is never refused.
    Answered,
    /// Ended early. `mid_commit` records that a commit it asked for was in
    /// flight at that moment.
    Stopped { mid_commit: bool },
}

impl RequestControl {
    pub(crate) fn new(id: u64, cancel: CancelFlag) -> Rc<Self> {
        Rc::new(Self {
            id,
            cancel,
            phase: Cell::new(Phase::Running),
            commits: Cell::new(0),
            on_end: RefCell::new(Vec::new()),
            next_listener: Cell::new(0),
        })
    }

    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    pub(crate) const fn cancel_flag(&self) -> &CancelFlag {
        &self.cancel
    }

    /// Whether the request has ended early: the runtime stopped it, or its
    /// host cancelled it before it was answered.
    pub(crate) fn has_ended(&self) -> bool {
        match self.phase.get() {
            Phase::Running => self.cancel.is_cancelled(),
            Phase::Answered => false,
            Phase::Stopped { .. } => true,
        }
    }

    /// Record that the request was answered with its own result. From now on
    /// nothing it left running is refused, and nothing waits on its end.
    pub(crate) fn answer(&self) {
        if self.phase.get() == Phase::Running {
            self.phase.set(Phase::Answered);
            drop(self.on_end.take());
        }
    }

    /// End the request early. From now on every op it makes is refused, and
    /// what waits on its end runs now. Ending it again changes nothing.
    /// Returns whether a commit it asked for was in flight when it ended.
    pub(crate) fn stop(&self) -> bool {
        match self.phase.get() {
            Phase::Running => {}
            Phase::Answered => return false,
            Phase::Stopped { mid_commit } => return mid_commit,
        }
        let mid_commit = self.commits.get() > 0;
        self.phase.set(Phase::Stopped { mid_commit });
        // Taken before any runs, so an action that withdraws a listener finds
        // nothing to withdraw.
        let waiting = self.on_end.take();
        for (_, action) in waiting {
            // An action is native cleanup a binding registered. One that
            // panics must not keep the actions after it, or whoever is ending
            // the request, from finishing: a stop still answers the isolate's
            // other requests and quarantines it.
            if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)) {
                tracing::error!(
                    request = self.id,
                    panic = %crate::panic_util::panic_message(&*panic),
                    "panic in an action waiting on a request's early end; the request still ends"
                );
            }
        }
        mid_commit
    }

    /// Run `action` when the request ends early: now, if it already has. An
    /// answered request never ends early, so `action` is dropped unrun.
    pub(crate) fn on_end(self: &Rc<Self>, action: EndAction) -> Option<EndListener> {
        if self.has_ended() {
            action();
            return None;
        }
        if self.phase.get() == Phase::Answered {
            return None;
        }
        let id = self.next_listener.get();
        self.next_listener.set(id + 1);
        self.on_end.borrow_mut().push((id, action));
        Some(EndListener {
            control: Rc::downgrade(self),
            id,
        })
    }

    /// Admit a commit: `None` once the request has ended, and otherwise the
    /// guard the commit holds until its outcome is known.
    pub(crate) fn commit_guard(self: &Rc<Self>) -> Option<CommitGuard> {
        if self.has_ended() {
            return None;
        }
        self.commits.set(self.commits.get() + 1);
        Some(CommitGuard(Some(Rc::clone(self))))
    }
}

impl std::fmt::Debug for RequestControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestControl")
            .field("id", &self.id)
            .field("phase", &self.phase.get())
            .field("commits", &self.commits.get())
            .finish_non_exhaustive()
    }
}

/// An action waiting on a request's early end. Dropping it withdraws the
/// action.
#[derive(Debug)]
pub struct EndListener {
    control: rc::Weak<RequestControl>,
    id: u64,
}

impl Drop for EndListener {
    fn drop(&mut self) {
        if let Some(control) = self.control.upgrade() {
            control.on_end.borrow_mut().retain(|(id, _)| *id != self.id);
        }
    }
}

/// Held by a commit from its admission until its outcome is known. A request
/// that ends while one is held is answered that its commit's outcome is
/// unknown.
#[derive(Debug)]
pub struct CommitGuard(Option<Rc<RequestControl>>);

impl CommitGuard {
    /// The guard of a commit made for no request, which has no one to answer.
    pub(crate) const fn unowned() -> Self {
        Self(None)
    }
}

impl Drop for CommitGuard {
    fn drop(&mut self) {
        if let Some(control) = &self.0 {
            control.commits.set(control.commits.get() - 1);
        }
    }
}

// ---------------------------------------------------------------------------
// The token object
// ---------------------------------------------------------------------------

/// The native state of a request's token object.
struct TokenState {
    control: Rc<RequestControl>,
    /// The handle Rust holds the token by while anything Rust-side holds it,
    /// so every holder shares one handle rather than making its own.
    handle: RefCell<rc::Weak<v8::Global<v8::Object>>>,
}

/// A request's token: its [`RequestControl`] and the token object its
/// invocation contexts carry.
#[derive(Clone)]
pub struct RequestToken {
    control: Rc<RequestControl>,
    object: Rc<v8::Global<v8::Object>>,
}

impl RequestToken {
    /// Mint the token of a request about to be dispatched. `None` only when
    /// V8 cannot allocate, which happens with a termination pending.
    pub(crate) fn mint(scope: &mut v8::PinScope, control: Rc<RequestControl>) -> Option<Self> {
        let (_, template) = invocation_locals(scope);
        let token = template.new_instance(scope)?;
        let object = Rc::new(v8::Global::new(scope, token));
        crate::brand::wrap(
            scope,
            token,
            TokenState {
                control: Rc::clone(&control),
                handle: RefCell::new(Rc::downgrade(&object)),
            },
        );
        Some(Self { control, object })
    }

    pub(crate) const fn control(&self) -> &Rc<RequestControl> {
        &self.control
    }

    pub(crate) fn id(&self) -> u64 {
        self.control.id()
    }
}

impl std::fmt::Debug for RequestToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_tuple("RequestToken").field(&self.control).finish()
    }
}

/// The token of the request the running code belongs to, read from its
/// invocation context.
pub struct CurrentRequest<'s> {
    object: v8::Local<'s, v8::Object>,
    state: NonNull<TokenState>,
}

impl<'s> CurrentRequest<'s> {
    const fn state(&self) -> &TokenState {
        // SAFETY: the brand matched, so `state` is the `TokenState` this module
        // boxed into `object`, and the box lives until V8 collects `object`,
        // which `self.object` keeps reachable for as long as `self` lives.
        unsafe { self.state.as_ref() }
    }

    pub(crate) fn control(&self) -> &RequestControl {
        &self.state().control
    }

    pub(crate) fn id(&self) -> u64 {
        self.control().id()
    }

    pub(crate) const fn object(&self) -> v8::Local<'s, v8::Object> {
        self.object
    }

    /// This request's token, sharing the handle other Rust holders hold it by.
    pub(crate) fn token(&self, scope: &mut v8::PinScope) -> RequestToken {
        let state = self.state();
        let shared = state.handle.borrow().upgrade();
        let object = shared.unwrap_or_else(|| {
            let object = Rc::new(v8::Global::new(scope, self.object));
            *state.handle.borrow_mut() = Rc::downgrade(&object);
            object
        });
        RequestToken {
            control: Rc::clone(&state.control),
            object,
        }
    }
}

// ---------------------------------------------------------------------------
// Invocation contexts
// ---------------------------------------------------------------------------

/// Platform-owned identity for one user-code invocation.
#[derive(Clone, Debug, Default)]
pub(crate) struct InvocationContext {
    pub user_json: Option<String>,
    /// The request the invocation belongs to; `None` for code that belongs to
    /// no request, such as module evaluation or a connection's event handler.
    pub request: Option<RequestToken>,
}

impl InvocationContext {
    pub(crate) const fn request(token: RequestToken, user_json: Option<String>) -> Self {
        Self {
            user_json,
            request: Some(token),
        }
    }

    pub(crate) const fn connection(user_json: Option<String>) -> Self {
        Self {
            user_json,
            request: None,
        }
    }

    /// The context a turn of `token`'s request enters, whether or not that
    /// request is still pending.
    pub(crate) fn of_request(state: &SharedState, token: &RequestToken) -> Self {
        let user_json = state.borrow().per_request_user.get(&token.id()).cloned();
        Self::request(token.clone(), user_json)
    }
}

struct InvocationKeySlot {
    key: v8::Global<v8::Symbol>,
    /// Template for request token objects.
    token: v8::Global<v8::ObjectTemplate>,
}

fn invocation_locals<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> (v8::Local<'s, v8::Symbol>, v8::Local<'s, v8::ObjectTemplate>) {
    if let Some(slot) = scope.get_slot::<InvocationKeySlot>() {
        return (v8::Local::new(scope, &slot.key), v8::Local::new(scope, &slot.token));
    }

    let description = v8::String::new(scope, "zs:InvocationContext").unwrap();
    let key = v8::Symbol::new(scope, Some(description));
    let template = v8::ObjectTemplate::new(scope);
    template.set_internal_field_count(1);
    let slot = InvocationKeySlot {
        key: v8::Global::new(scope, key),
        token: v8::Global::new(scope, template),
    };
    scope.set_slot(slot);
    (key, template)
}

fn invocation_key<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Symbol> {
    invocation_locals(scope).0
}

fn encode_context<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    context: &InvocationContext,
) -> v8::Local<'s, v8::Array> {
    let user_json: v8::Local<v8::Value> = match context.user_json.as_deref() {
        Some(json) => v8::String::new(scope, json)
            .map(Into::into)
            .unwrap_or_else(|| v8::null(scope).into()),
        None => v8::null(scope).into(),
    };
    let token: v8::Local<v8::Value> = context.request.as_ref().map_or_else(
        || v8::null(scope).into(),
        |token| v8::Local::new(scope, &*token.object).into(),
    );
    v8::Array::new_with_elements(scope, &[user_json, token])
}

/// Install an invocation context while `body` enters user code.
///
/// The existing Map is cloned so user `AsyncLocalStorage` entries remain
/// visible and sibling continuations retain immutable snapshots.
pub(crate) fn with_context<'s, R>(
    scope: &mut v8::PinScope<'s, '_>,
    context: &InvocationContext,
    body: impl FnOnce(&mut v8::PinScope<'s, '_>) -> R,
) -> R {
    let previous = enter_context(scope, context);

    let result = body(scope);

    let previous = v8::Local::new(scope, &previous);
    scope.set_continuation_preserved_embedder_data(previous);
    result
}

/// Install an invocation frame and return the exact prior CPED value.
fn enter_context(
    scope: &mut v8::PinScope,
    context: &InvocationContext,
) -> v8::Global<v8::Value> {
    let previous = scope.get_continuation_preserved_embedder_data();
    let previous = v8::Global::new(scope, previous);

    let next = match read_context_map(scope) {
        Some(map) => clone_map(scope, map),
        None => v8::Map::new(scope),
    };
    let key = invocation_key(scope);
    let value = encode_context(scope, context);
    next.set(scope, key.into(), value.into());
    scope.set_continuation_preserved_embedder_data(next.into());
    previous
}

/// Run module initialization under an authoritative invocation context while
/// retaining ambient ALS updates made by module code.
///
/// The post-call Map is cloned before the invocation key is removed. Async
/// work created by `body` therefore keeps its captured invocation frame, while
/// later dispatches inherit module-level `AsyncLocalStorage.enterWith()` data.
pub(crate) fn with_context_preserving_ambient<'s, R>(
    scope: &mut v8::PinScope<'s, '_>,
    context: &InvocationContext,
    body: impl FnOnce(&mut v8::PinScope<'s, '_>) -> R,
) -> R {
    let previous = enter_context(scope, context);
    let result = body(scope);

    if let Some(current) = read_context_map(scope) {
        let ambient = clone_map(scope, current);
        let key = invocation_key(scope);
        ambient.delete(scope, key.into());
        scope.set_continuation_preserved_embedder_data(ambient.into());
    } else {
        let previous = v8::Local::new(scope, &previous);
        scope.set_continuation_preserved_embedder_data(previous);
    }

    result
}

/// Snapshot the complete continuation context for a host callback registered
/// by the current invocation.
pub(crate) fn capture_context(scope: &mut v8::PinScope) -> v8::Global<v8::Value> {
    let current = scope.get_continuation_preserved_embedder_data();
    v8::Global::new(scope, current)
}

/// Enter a previously captured continuation context for a host callback.
pub(crate) fn with_captured_context<'s, R>(
    scope: &mut v8::PinScope<'s, '_>,
    captured: &v8::Global<v8::Value>,
    body: impl FnOnce(&mut v8::PinScope<'s, '_>) -> R,
) -> R {
    let previous = scope.get_continuation_preserved_embedder_data();
    let previous = v8::Global::new(scope, previous);
    let captured = v8::Local::new(scope, captured);
    scope.set_continuation_preserved_embedder_data(captured);

    let result = body(scope);

    let previous = v8::Local::new(scope, &previous);
    scope.set_continuation_preserved_embedder_data(previous);
    result
}

// ---------------------------------------------------------------------------
// Reading the running code's context
// ---------------------------------------------------------------------------

/// The invocation frame V8 selected for the running continuation.
enum Frame<'s> {
    /// No frame: code entered without an invocation context.
    Absent,
    /// A frame whose value this module did not encode.
    Malformed,
    Present(v8::Local<'s, v8::Array>),
}

fn current_frame<'s>(scope: &mut v8::PinScope<'s, '_>) -> Frame<'s> {
    let Some(map) = read_context_map(scope) else {
        return Frame::Absent;
    };
    let key = invocation_key(scope);
    if !map.has(scope, key.into()).unwrap_or(false) {
        return Frame::Absent;
    }
    let Some(value) = map.get(scope, key.into()) else {
        return Frame::Malformed;
    };
    match v8::Local::<v8::Array>::try_from(value) {
        Ok(array) if array.length() == 2 => Frame::Present(array),
        _ => Frame::Malformed,
    }
}

/// The request the running code belongs to, as its invocation frame says.
pub enum RequestRead<'s> {
    /// Code entered without an invocation context.
    NoFrame,
    /// A frame that belongs to no request.
    Anonymous,
    Request(CurrentRequest<'s>),
    /// A frame this module did not encode.
    Malformed,
}

pub fn current_request<'s>(scope: &mut v8::PinScope<'s, '_>) -> RequestRead<'s> {
    match current_frame(scope) {
        Frame::Absent => RequestRead::NoFrame,
        Frame::Malformed => RequestRead::Malformed,
        Frame::Present(array) => request_in(scope, array),
    }
}

fn request_in<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    array: v8::Local<'s, v8::Array>,
) -> RequestRead<'s> {
    let Some(token) = array.get_index(scope, 1) else {
        return RequestRead::Malformed;
    };
    if token.is_null() {
        return RequestRead::Anonymous;
    }
    let Ok(object) = v8::Local::<v8::Object>::try_from(token) else {
        return RequestRead::Malformed;
    };
    crate::brand::state::<TokenState>(scope, object).map_or(RequestRead::Malformed, |state| {
        RequestRead::Request(CurrentRequest { object, state })
    })
}

/// What the running code's invocation context says about it.
pub struct ContextView<'s> {
    pub user_json: Option<String>,
    pub request: Option<CurrentRequest<'s>>,
}

/// Read the context V8 selected for the currently executing continuation.
///
/// Key presence is authoritative. A malformed platform value becomes an
/// anonymous context rather than falling through to isolate-global state.
pub fn current_context<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<ContextView<'s>> {
    let anonymous = || {
        Some(ContextView {
            user_json: None,
            request: None,
        })
    };
    let array = match current_frame(scope) {
        Frame::Absent => return None,
        Frame::Malformed => return anonymous(),
        Frame::Present(array) => array,
    };
    let Some(user_value) = array.get_index(scope, 0) else {
        return anonymous();
    };
    let user_json = if user_value.is_null() {
        None
    } else if user_value.is_string() {
        Some(user_value.to_rust_string_lossy(scope))
    } else {
        return anonymous();
    };
    let request = match request_in(scope, array) {
        RequestRead::Request(request) => Some(request),
        RequestRead::Anonymous => None,
        RequestRead::NoFrame | RequestRead::Malformed => return anonymous(),
    };
    Some(ContextView { user_json, request })
}

/// Resolve the owning request for a native callback. Invocation-local state
/// wins; the Rust field remains as a fallback for host turns that predate a
/// captured frame.
pub(crate) fn current_request_id(
    scope: &mut v8::PinScope,
    state: &SharedState,
) -> Option<u64> {
    match current_request(scope) {
        RequestRead::Request(request) => Some(request.id()),
        RequestRead::Anonymous | RequestRead::Malformed => None,
        RequestRead::NoFrame => state.borrow().executing_request_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused_as_ended(scope: &mut v8::PinScope) -> bool {
        matches!(
            crate::state::admit_op(scope),
            Err(crate::state::OpError {
                kind: crate::state::OpErrorKind::CodedError { code, .. },
                ..
            }) if code == crate::state::REQUEST_CANCELLED
        )
    }

    /// The op gate admits code whose frame names no request, and refuses code
    /// whose frame this module did not encode: a frame it cannot read fails
    /// closed rather than reading as code that belongs to no request.
    #[test]
    fn the_op_gate_refuses_code_whose_frame_it_cannot_read() {
        crate::init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handles, &mut isolate);
        let context = v8::Context::new(handles, v8::ContextOptions::default());
        let scope = &mut v8::ContextScope::new(handles, context);

        with_context(scope, &InvocationContext::connection(None), |scope| {
            assert!(matches!(current_request(scope), RequestRead::Anonymous));
            assert!(
                crate::state::admit_op(scope).is_ok(),
                "code that belongs to no request is admitted"
            );
        });

        let frame = v8::Map::new(scope);
        let key = invocation_key(scope);
        let unreadable = v8::Integer::new(scope, 7);
        frame.set(scope, key.into(), unreadable.into());
        scope.set_continuation_preserved_embedder_data(frame.into());
        assert!(matches!(current_request(scope), RequestRead::Malformed));
        assert!(refused_as_ended(scope), "a frame the gate cannot read is refused");
    }
}
