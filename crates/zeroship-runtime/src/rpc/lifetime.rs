//! Host cancellation ownership follows an RPC through its response body.

use std::time::Instant;

use crate::channel::CancelFlag;
use crate::state::{DispatchResult, SharedState};

use super::abort::{AbortGuard, RequestSignal};
use super::error::ZsErrorCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cancellation {
    Cancelled,
    Timeout,
    Overflow,
}

impl Cancellation {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Cancelled => "Request cancelled",
            Self::Timeout => "Request timed out",
            Self::Overflow => "Stream buffer exceeded its limit",
        }
    }

    pub(crate) fn exception<'s>(
        self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> v8::Local<'s, v8::Value> {
        match self {
            Self::Timeout => crate::dom::abort_signal::build_timeout_error(scope).into(),
            Self::Cancelled | Self::Overflow => {
                crate::dom::abort_signal::build_abort_error(scope).into()
            }
        }
    }

    pub(crate) fn response(self) -> DispatchResult {
        let code = match self {
            Self::Cancelled => ZsErrorCode::Cancelled,
            Self::Timeout => ZsErrorCode::Timeout,
            Self::Overflow => ZsErrorCode::ResourceExhausted,
        };
        DispatchResult::HttpResponse(crate::http::ResponseInfo::Complete {
            status: code.http_status(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: crate::dispatch::build_host_error_body(self.message(), code).into_bytes(),
        })
    }
}

pub(crate) struct RequestLifetime {
    pub request_id: u64,
    pub cancel: CancelFlag,
    pub deadline: Option<Instant>,
    pub signal: RequestSignal,
    pub _abort_guard: Option<AbortGuard>,
}

impl RequestLifetime {
    pub(crate) fn cancellation(&self, now: Instant) -> Option<Cancellation> {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            Some(Cancellation::Timeout)
        } else if self.cancel.is_cancelled() {
            Some(Cancellation::Cancelled)
        } else {
            None
        }
    }

    pub(crate) fn release(self, state: &SharedState) {
        let mut state = state.borrow_mut();
        state.per_request_user.remove(&self.request_id);
        state.request_ctx_by_id.remove(&self.request_id);
        state.request_by_id.remove(&self.request_id);
        state.per_request_logs.remove(&self.request_id);
    }
}
