//! The app-facts answer a test dictates, used by suites that put a STALE answer
//! in front of the policy publication fence.
//!
//! A lagging replica or a cache in front of the route is what would do that in
//! production; neither can be arranged in a fixture, so this seam lets a case
//! control the answer and its watermark directly.

use std::{cell::RefCell, rc::Rc};
use zeroship_core::{
    AppId,
    workflow_app_facts::AppFactsResponse,
};
use zeroship_workflow_manager::{
    Error,
    app_facts::{AppFactsFuture, AppFactsSource},
};

/// An answer a test dictates, including its watermark.
#[derive(Debug, Default)]
pub struct ScriptedAppFacts {
    answer: RefCell<Option<AppFactsResponse>>,
}

impl ScriptedAppFacts {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    /// The answer the next observation returns. An unset answer is unavailable
    /// rather than empty, so a case that forgets to script one fails loudly.
    pub fn set(&self, response: AppFactsResponse) {
        *self.answer.borrow_mut() = Some(response);
    }
}

impl AppFactsSource for ScriptedAppFacts {
    fn observe<'a>(&'a self, apps: &'a [AppId]) -> AppFactsFuture<'a> {
        let scripted = self.answer.borrow().clone();
        Box::pin(async move {
            if apps.is_empty() {
                return Err(Error::Invalid);
            }
            scripted.ok_or(Error::Unavailable)
        })
    }
}
