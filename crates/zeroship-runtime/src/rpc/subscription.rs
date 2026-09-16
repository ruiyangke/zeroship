//! Native RPC subscription handshake, invocation and WebSocket framing.

use std::collections::HashMap;
use std::time::Duration;

use crate::core::invocation::with_captured_context;
use crate::http::ResponseInfo;
use crate::rpc::dispatch::{CallProgress, ProcedureRegistry, RpcCall, response};
use crate::rpc::lifetime::RequestLifetime;
use crate::state::{DispatchResult, OpResult, SharedState};
use crate::websocket_native::network::{self, WsEvent};
use crate::websocket_native::{WsFrame, pair};

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const PONG_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
pub enum Timer {
    Hello { ws_id: u32, token: u64 },
    Ping { ws_id: u32, token: u64 },
    Pong { ws_id: u32, token: u64 },
}

impl Timer {
    pub(crate) fn ws_id(self) -> u32 {
        match self {
            Self::Hello { ws_id, .. } | Self::Ping { ws_id, .. } | Self::Pong { ws_id, .. } => {
                ws_id
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct Subscriptions {
    sessions: HashMap<u32, Session>,
}

struct Session {
    client_ws_id: u32,
    server_ws_id: u32,
    name: String,
    registry: ProcedureRegistry,
    rpc_ctx: v8::Global<v8::Object>,
    lifetime: Option<RequestLifetime>,
    phase: Phase,
    timer_token: u64,
    pong_token: u64,
    awaiting_pong: Option<u64>,
}

enum Phase {
    AwaitHello,
    Calling(RpcCall),
    Iterating(IteratorState),
}

struct IteratorState {
    iterator: v8::Global<v8::Object>,
    next: v8::Global<v8::Function>,
    frame: v8::Global<v8::Value>,
    pending: Option<v8::Global<v8::Promise>>,
    returned: bool,
}

enum ClientFrame<'s> {
    InvalidJson,
    MissingTag,
    Ping,
    Pong,
    Hello(v8::Local<'s, v8::Value>),
    Ignore,
}

impl Subscriptions {
    pub(crate) fn open(
        &mut self,
        scope: &mut v8::PinScope,
        state: &SharedState,
        name: String,
        registry: ProcedureRegistry,
        rpc_ctx: v8::Local<v8::Object>,
        lifetime: RequestLifetime,
    ) -> ResponseInfo {
        let (client_ws_id, server_ws_id) = pair::mint_pair(scope, state);
        let token = 1;
        self.sessions.insert(
            server_ws_id,
            Session {
                client_ws_id,
                server_ws_id,
                name,
                registry,
                rpc_ctx: v8::Global::new(scope, rpc_ctx),
                lifetime: Some(lifetime),
                phase: Phase::AwaitHello,
                timer_token: token,
                pong_token: 0,
                awaiting_pong: None,
            },
        );
        schedule_timer(
            state,
            HELLO_TIMEOUT,
            Timer::Hello {
                ws_id: server_ws_id,
                token,
            },
        );
        ResponseInfo::WebSocket {
            ws_id: client_ws_id,
            headers: vec![("sec-websocket-protocol".into(), "zs.v1".into())],
        }
    }

    pub(crate) fn owns(&self, ws_id: u32) -> bool {
        self.sessions.contains_key(&ws_id)
    }

    pub(crate) fn on_websocket_event(
        &mut self,
        scope: &mut v8::PinScope,
        state: &SharedState,
        ws_id: u32,
    ) {
        let Some(mut session) = self.sessions.remove(&ws_id) else {
            return;
        };
        let events = network::drain_events(state, ws_id);
        let mut keep = true;
        for event in events {
            if !keep {
                break;
            }
            keep = match event {
                WsEvent::MessageText(text) => self.on_text(scope, state, &mut session, &text),
                WsEvent::MessageBinary(_) => {
                    close_session(scope, state, &mut session, 4400, "invalid JSON", true);
                    false
                }
                WsEvent::Close { .. } | WsEvent::Error { .. } => {
                    retire_session(scope, state, &mut session, true);
                    false
                }
                WsEvent::Open { .. } => true,
            };
        }
        if keep {
            self.sessions.insert(ws_id, session);
        }
    }

    pub(crate) fn advance(&mut self, scope: &mut v8::PinScope, state: &SharedState, ws_id: u32) {
        let Some(mut session) = self.sessions.remove(&ws_id) else {
            return;
        };
        if advance_session(scope, state, &mut session) {
            self.sessions.insert(ws_id, session);
        }
    }

    pub(crate) fn on_timer(&mut self, scope: &mut v8::PinScope, state: &SharedState, timer: Timer) {
        let (ws_id, token) = match timer {
            Timer::Hello { ws_id, token }
            | Timer::Ping { ws_id, token }
            | Timer::Pong { ws_id, token } => (ws_id, token),
        };
        let Some(mut session) = self.sessions.remove(&ws_id) else {
            return;
        };
        let keep = match timer {
            Timer::Hello { .. }
                if matches!(session.phase, Phase::AwaitHello) && session.timer_token == token =>
            {
                close_session(scope, state, &mut session, 4400, "missing hello", true);
                false
            }
            Timer::Ping { .. }
                if !matches!(session.phase, Phase::AwaitHello) && session.timer_token == token =>
            {
                send_text(state, &session, r#"{"t":"ping"}"#.into());
                if session.awaiting_pong.is_none() {
                    session.pong_token = session.pong_token.wrapping_add(1);
                    let pong_token = session.pong_token;
                    session.awaiting_pong = Some(pong_token);
                    schedule_timer(
                        state,
                        PONG_TIMEOUT,
                        Timer::Pong {
                            ws_id,
                            token: pong_token,
                        },
                    );
                }
                session.timer_token = session.timer_token.wrapping_add(1);
                let ping_token = session.timer_token;
                schedule_timer(
                    state,
                    PING_INTERVAL,
                    Timer::Ping {
                        ws_id,
                        token: ping_token,
                    },
                );
                true
            }
            Timer::Pong { .. } if session.awaiting_pong == Some(token) => {
                close_session(scope, state, &mut session, 4408, "pong timeout", true);
                false
            }
            _ => true,
        };
        if keep {
            self.sessions.insert(ws_id, session);
        }
    }

    fn on_text(
        &mut self,
        scope: &mut v8::PinScope,
        state: &SharedState,
        session: &mut Session,
        text: &str,
    ) -> bool {
        match parse_client_frame(scope, text) {
            ClientFrame::InvalidJson => {
                close_session(scope, state, session, 4400, "invalid JSON", true);
                false
            }
            ClientFrame::MissingTag => {
                close_session(scope, state, session, 4400, "missing frame tag", true);
                false
            }
            ClientFrame::Ping => {
                send_text(state, session, r#"{"t":"pong"}"#.into());
                true
            }
            ClientFrame::Pong => {
                session.awaiting_pong = None;
                true
            }
            ClientFrame::Hello(input) if matches!(session.phase, Phase::AwaitHello) => {
                session.timer_token = session.timer_token.wrapping_add(1);
                let ping_token = session.timer_token;
                schedule_timer(
                    state,
                    PING_INTERVAL,
                    Timer::Ping {
                        ws_id: session.server_ws_id,
                        token: ping_token,
                    },
                );
                let ctx = v8::Local::new(scope, &session.rpc_ctx);
                let call = crate::rpc::with_rpc_context(scope, ctx, |scope| {
                    RpcCall::new(
                        scope,
                        session.registry.clone(),
                        session.name.clone(),
                        input,
                        ctx.into(),
                    )
                });
                session.phase = Phase::Calling(call);
                advance_session(scope, state, session)
            }
            ClientFrame::Hello(_) | ClientFrame::Ignore => true,
        }
    }
}

fn parse_client_frame<'s>(scope: &mut v8::PinScope<'s, '_>, text: &str) -> ClientFrame<'s> {
    v8::tc_scope!(let tc, scope);
    let Some(source) = v8::String::new(tc, text) else {
        return ClientFrame::InvalidJson;
    };
    let Some(value) = v8::json::parse(tc, source) else {
        return ClientFrame::InvalidJson;
    };
    if tc.has_caught() || !value.is_object() || value.is_array() {
        return ClientFrame::MissingTag;
    }
    let object = v8::Local::<v8::Object>::try_from(value).unwrap();
    let Some(tag_key) = v8::String::new(tc, "t") else {
        return ClientFrame::MissingTag;
    };
    let Some(tag) = object.get(tc, tag_key.into()) else {
        return ClientFrame::MissingTag;
    };
    if !tag.is_string() {
        return ClientFrame::MissingTag;
    }
    match tag.to_rust_string_lossy(tc).as_str() {
        "ping" => ClientFrame::Ping,
        "pong" => ClientFrame::Pong,
        "hello" => {
            let Some(input_key) = v8::String::new(tc, "input") else {
                return ClientFrame::InvalidJson;
            };
            ClientFrame::Hello(
                object
                    .get(tc, input_key.into())
                    .unwrap_or_else(|| v8::undefined(tc).into()),
            )
        }
        _ => ClientFrame::Ignore,
    }
}

fn advance_session(scope: &mut v8::PinScope, state: &SharedState, session: &mut Session) -> bool {
    loop {
        match &mut session.phase {
            Phase::AwaitHello => return true,
            Phase::Calling(call) => match call.poll(scope) {
                Ok(CallProgress::Pending(promise)) => {
                    return retain_until_settled(scope, promise, session.server_ws_id).map_or_else(
                        |error| {
                            send_error_and_close(scope, state, session, error);
                            false
                        },
                        |_| true,
                    );
                }
                Ok(CallProgress::Missing(name)) => {
                    let error = response::error_value(
                        format!("Method not found: {name}"),
                        404,
                        "NOT_FOUND",
                    );
                    send_error_and_close(scope, state, session, error);
                    return false;
                }
                Ok(CallProgress::MethodNotAllowed { method, path }) => {
                    let error = response::error_value(
                        format!("method {method} not allowed on {path}"),
                        405,
                        "FAILED_PRECONDITION",
                    );
                    send_error_and_close(scope, state, session, error);
                    return false;
                }
                Err(failure) => {
                    let error = response::failure(scope, failure);
                    send_error_and_close(scope, state, session, error);
                    return false;
                }
                Ok(CallProgress::Complete { invocation, value }) => {
                    let value = v8::Local::new(scope, value);
                    match capture_iterator(scope, value, invocation.frame) {
                        Ok(iterator) => session.phase = Phase::Iterating(iterator),
                        Err(error) => {
                            send_error_and_close(scope, state, session, error);
                            return false;
                        }
                    }
                }
            },
            Phase::Iterating(iterator) => {
                if let Some(promise) = iterator.pending.take() {
                    let promise = v8::Local::new(scope, promise);
                    match promise.state() {
                        v8::PromiseState::Pending => {
                            iterator.pending = Some(v8::Global::new(scope, promise));
                            return true;
                        }
                        v8::PromiseState::Rejected => {
                            let error = crate::dispatch::v8_exception_to_error_value(
                                scope,
                                promise.result(scope),
                            );
                            send_error_and_close(scope, state, session, error);
                            return false;
                        }
                        v8::PromiseState::Fulfilled => {
                            let step = promise.result(scope);
                            let done = match iterator_step(scope, step, state, session) {
                                Ok(done) => done,
                                Err(error) => {
                                    send_error_and_close(scope, state, session, error);
                                    return false;
                                }
                            };
                            if done {
                                send_text(state, session, r#"{"t":"end"}"#.into());
                                close_session(scope, state, session, 1000, "", false);
                                return false;
                            }
                            return true;
                        }
                    }
                }
                let promise = match call_next(scope, iterator) {
                    Ok(promise) => promise,
                    Err(error) => {
                        send_error_and_close(scope, state, session, error);
                        return false;
                    }
                };
                if let Err(error) =
                    retain_until_settled(scope, promise.clone(), session.server_ws_id)
                {
                    send_error_and_close(scope, state, session, error);
                    return false;
                }
                iterator.pending = Some(promise);
                return true;
            }
        }
    }
}

fn capture_iterator(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
    frame: v8::Global<v8::Value>,
) -> Result<IteratorState, DispatchResult> {
    let Ok(iterator) = v8::Local::<v8::Object>::try_from(value) else {
        return Err(not_async_iterator());
    };
    v8::tc_scope!(let tc, scope);
    let async_iterator = v8::Symbol::get_async_iterator(tc);
    let Some(async_iterator) = iterator.get(tc, async_iterator.into()) else {
        return Err(tc.exception().map_or_else(not_async_iterator, |error| {
            crate::dispatch::v8_exception_to_error_value(tc, error)
        }));
    };
    let Some(next_key) = v8::String::new(tc, "next") else {
        return Err(DispatchResult::Error(
            "could not allocate subscription iterator property".into(),
        ));
    };
    let Some(next) = iterator.get(tc, next_key.into()) else {
        return Err(tc.exception().map_or_else(not_async_iterator, |error| {
            crate::dispatch::v8_exception_to_error_value(tc, error)
        }));
    };
    let Ok(next) = v8::Local::<v8::Function>::try_from(next) else {
        return Err(not_async_iterator());
    };
    if !async_iterator.is_function() {
        return Err(not_async_iterator());
    }
    Ok(IteratorState {
        iterator: v8::Global::new(tc, iterator),
        next: v8::Global::new(tc, next),
        frame,
        pending: None,
        returned: false,
    })
}

fn not_async_iterator() -> DispatchResult {
    response::error_value(
        "Subscription handler must return an async iterator".into(),
        500,
        "INTERNAL",
    )
}

fn call_next(
    scope: &mut v8::PinScope,
    iterator: &IteratorState,
) -> Result<v8::Global<v8::Promise>, DispatchResult> {
    with_captured_context(scope, &iterator.frame, |scope| {
        v8::tc_scope!(let tc, scope);
        let local = v8::Local::new(tc, &iterator.iterator);
        let next = v8::Local::new(tc, &iterator.next);
        let Some(result) = next.call(tc, local.into(), &[]) else {
            return Err(tc.exception().map_or_else(
                || DispatchResult::Error("could not call subscription iterator".into()),
                |error| crate::dispatch::v8_exception_to_error_value(tc, error),
            ));
        };
        let Some(resolver) = v8::PromiseResolver::new(tc) else {
            return Err(DispatchResult::Error(
                "could not retain subscription iterator result".into(),
            ));
        };
        resolver.resolve(tc, result);
        let promise = resolver.get_promise(tc);
        promise.mark_as_handled();
        Ok(v8::Global::new(tc, promise))
    })
}

fn iterator_step(
    scope: &mut v8::PinScope,
    step: v8::Local<v8::Value>,
    state: &SharedState,
    session: &Session,
) -> Result<bool, DispatchResult> {
    let Ok(step) = v8::Local::<v8::Object>::try_from(step) else {
        return Err(response::error_value(
            "Subscription iterator result must be an object".into(),
            500,
            "INTERNAL",
        ));
    };
    v8::tc_scope!(let tc, scope);
    let done_key = v8::String::new(tc, "done").ok_or_else(|| {
        DispatchResult::Error("could not allocate iterator result property".into())
    })?;
    let done = step.get(tc, done_key.into()).ok_or_else(|| {
        tc.exception().map_or_else(
            || DispatchResult::Error("could not read subscription iterator result".into()),
            |error| crate::dispatch::v8_exception_to_error_value(tc, error),
        )
    })?;
    if done.boolean_value(tc) {
        return Ok(true);
    }
    let value_key = v8::String::new(tc, "value").ok_or_else(|| {
        DispatchResult::Error("could not allocate iterator result property".into())
    })?;
    let value = step.get(tc, value_key.into()).ok_or_else(|| {
        tc.exception().map_or_else(
            || DispatchResult::Error("could not read subscription iterator value".into()),
            |error| crate::dispatch::v8_exception_to_error_value(tc, error),
        )
    })?;
    let frame = encode_data_frame(tc, value)?;
    send_data_text(state, session, frame);
    Ok(false)
}

fn encode_data_frame(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<String, DispatchResult> {
    v8::tc_scope!(let tc, scope);
    let frame = v8::Object::new(tc);
    let tag_key = v8::String::new(tc, "t")
        .ok_or_else(|| DispatchResult::Error("could not allocate subscription frame".into()))?;
    let value_key = v8::String::new(tc, "value")
        .ok_or_else(|| DispatchResult::Error("could not allocate subscription frame".into()))?;
    let tag = v8::String::new(tc, "data")
        .ok_or_else(|| DispatchResult::Error("could not allocate subscription frame".into()))?;
    if frame.create_data_property(tc, tag_key.into(), tag.into()) != Some(true)
        || frame.create_data_property(tc, value_key.into(), value) != Some(true)
    {
        return Err(DispatchResult::Error(
            "could not build subscription frame".into(),
        ));
    }
    v8::json::stringify(tc, frame.into())
        .map(|json| json.to_rust_string_lossy(tc))
        .ok_or_else(|| {
            tc.exception().map_or_else(
                || DispatchResult::Error("could not serialize subscription value".into()),
                |error| crate::dispatch::v8_exception_to_error_value(tc, error),
            )
        })
}

fn retain_until_settled(
    scope: &mut v8::PinScope,
    promise: v8::Global<v8::Promise>,
    ws_id: u32,
) -> Result<(), DispatchResult> {
    let id = v8::Integer::new_from_unsigned(scope, ws_id);
    let callback = v8::FunctionTemplate::builder(wake_subscription_callback)
        .data(id.into())
        .build(scope)
        .get_function(scope)
        .ok_or_else(|| DispatchResult::Error("could not watch subscription promise".into()))?;
    let promise = v8::Local::new(scope, promise);
    let chained = promise
        .then2(scope, callback, callback)
        .ok_or_else(|| DispatchResult::Error("could not watch subscription promise".into()))?;
    chained.mark_as_handled();
    Ok(())
}

fn wake_subscription_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if let Some(state) = scope.get_slot::<SharedState>().cloned() {
        let ws_id = args.data().uint32_value(scope).unwrap_or_default();
        schedule_advance(&state, ws_id);
    }
    rv.set(args.get(0));
}

fn schedule_advance(state: &SharedState, ws_id: u32) {
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        OpResult::SubscriptionAdvance { ws_id }
    }));
    state.borrow().notify_pump();
}

fn schedule_timer(state: &SharedState, delay: Duration, timer: Timer) {
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        compio::time::sleep(delay).await;
        OpResult::SubscriptionTimer(timer)
    }));
    state.borrow().notify_pump();
}

fn send_text(state: &SharedState, session: &Session, text: String) {
    pair::deliver_to_peer(
        state,
        session.server_ws_id,
        session.client_ws_id,
        vec![WsFrame::Text(text)],
    );
}

fn send_data_text(state: &SharedState, session: &Session, text: String) {
    pair::deliver_to_peer_with_completion(
        state,
        session.server_ws_id,
        session.client_ws_id,
        vec![WsFrame::Text(text)],
        OpResult::SubscriptionAdvance {
            ws_id: session.server_ws_id,
        },
    );
}

fn close_session(
    scope: &mut v8::PinScope,
    state: &SharedState,
    session: &mut Session,
    code: u16,
    reason: &str,
    abort: bool,
) {
    pair::deliver_to_peer(
        state,
        session.server_ws_id,
        session.client_ws_id,
        vec![WsFrame::Close {
            code: Some(code),
            reason: reason.into(),
        }],
    );
    retire_session(scope, state, session, abort);
}

fn retire_session(
    scope: &mut v8::PinScope,
    state: &SharedState,
    session: &mut Session,
    abort: bool,
) {
    if let Some(lifetime) = session.lifetime.take() {
        if abort {
            lifetime.cancel.cancel();
            let reason = crate::dom::abort_signal::build_abort_error(scope);
            lifetime.signal.abort(scope, reason.into());
        }
        lifetime.release(state);
    }
    if let Phase::Iterating(iterator) = &mut session.phase {
        close_iterator(scope, iterator);
    }
    state.borrow_mut().ws_user.remove(&session.server_ws_id);
    state.borrow_mut().ws_user.remove(&session.client_ws_id);
}

fn close_iterator(scope: &mut v8::PinScope, iterator: &mut IteratorState) {
    if iterator.returned {
        return;
    }
    iterator.returned = true;
    with_captured_context(scope, &iterator.frame, |scope| {
        v8::tc_scope!(let tc, scope);
        let local = v8::Local::new(tc, &iterator.iterator);
        let Some(key) = v8::String::new(tc, "return") else {
            return;
        };
        let Some(value) = local.get(tc, key.into()) else {
            return;
        };
        let Ok(method) = v8::Local::<v8::Function>::try_from(value) else {
            return;
        };
        if let Some(promise) = method
            .call(tc, local.into(), &[])
            .and_then(|result| v8::Local::<v8::Promise>::try_from(result).ok())
        {
            promise.mark_as_handled();
        }
    });
}

fn send_error_and_close(
    scope: &mut v8::PinScope,
    state: &SharedState,
    session: &mut Session,
    error: DispatchResult,
) {
    if let Some(frame) = encode_error_frame(scope, error) {
        send_text(state, session, frame);
    }
    close_session(scope, state, session, 1011, "", false);
}

fn encode_error_frame(scope: &mut v8::PinScope, error: DispatchResult) -> Option<String> {
    let envelope = v8::Object::new(scope);
    let (message, name, code, details_json, retryable) = match error {
        DispatchResult::ErrorValue {
            message,
            name,
            code,
            details_json,
            retryable,
            ..
        } => (message, name, code, details_json, retryable),
        DispatchResult::Error(message) => {
            (message, "Error".into(), Some("INTERNAL".into()), None, None)
        }
        DispatchResult::HttpResponse(_) => (
            "Subscription failed".into(),
            "Error".into(),
            Some("INTERNAL".into()),
            None,
            None,
        ),
    };
    set_string(scope, envelope, "message", &message)?;
    set_string(scope, envelope, "name", &name)?;
    if let Some(code) = code {
        set_string(scope, envelope, "code", &code)?;
    }
    if let Some(details_json) = details_json {
        let source = v8::String::new(scope, &details_json)?;
        if let Some(details) = v8::json::parse(scope, source) {
            set_value(scope, envelope, "details", details)?;
        }
    }
    if let Some(retryable) = retryable {
        let retryable = v8::Boolean::new(scope, retryable);
        set_value(scope, envelope, "retryable", retryable.into())?;
    }
    let frame = v8::Object::new(scope);
    set_string(scope, frame, "t", "error")?;
    set_value(scope, frame, "error", envelope.into())?;
    v8::json::stringify(scope, frame.into()).map(|json| json.to_rust_string_lossy(scope))
}

fn set_string(
    scope: &mut v8::PinScope,
    object: v8::Local<v8::Object>,
    name: &str,
    value: &str,
) -> Option<()> {
    let value = v8::String::new(scope, value)?;
    set_value(scope, object, name, value.into())
}

fn set_value(
    scope: &mut v8::PinScope,
    object: v8::Local<v8::Object>,
    name: &str,
    value: v8::Local<v8::Value>,
) -> Option<()> {
    let key = v8::String::new(scope, name)?;
    object
        .create_data_property(scope, key.into(), value)?
        .then_some(())
}
