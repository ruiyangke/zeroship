//! Convert engine refusals into the JavaScript error contract.
//!
//! Every refusal the engine returns reaches creator code as a plain `Error`
//! carrying `WorkflowServiceError::code()`. The binding's own option readers
//! own `TypeError`: they raise it for an argument whose shape is wrong before
//! any engine call, so a `TypeError` escaping `env.workflows` always means the
//! creator passed the wrong kind of value, never that the engine refused a
//! well-formed one.

use zeroship_runtime::state::OpError;
use zeroship_workflow::WorkflowServiceError;

pub(crate) fn to_op_error(error: WorkflowServiceError) -> OpError {
    // The two opaque arms below replace their message deliberately: a
    // creator cannot act on a host condition, and
    // `opaque_refusals_replace_their_message` asserts that contract. It is
    // not a reason for the OPERATOR to lose the wording too - dozens of
    // sites construct `Unavailable` with distinct text and every one of
    // them arrives here as a single sentence, so this is the last place
    // the distinction exists.
    if matches!(
        error,
        WorkflowServiceError::Internal(_) | WorkflowServiceError::Unavailable(_)
    ) {
        tracing::warn!(
            code = error.code(),
            detail = %error,
            "workflow refusal reported opaquely to creator code"
        );
    }
    let message = match &error {
        WorkflowServiceError::Internal(_) => "workflow operation failed".to_owned(),
        WorkflowServiceError::Unavailable(_) => "workflow service is unavailable".to_owned(),
        _ => error.to_string(),
    };
    OpError::coded(error.code(), message, None::<String>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_runtime::state::OpErrorKind;
    use zeroship_workflow::WorkflowServiceError as E;

    /// A wildcard-free match, so adding a variant stops compiling here until
    /// `refusals` carries an instance of it.
    const fn covered(error: &E) {
        match error {
            E::InvalidRequest(_)
            | E::Unauthenticated
            | E::PermissionDenied
            | E::NotFound(_)
            | E::Conflict(_)
            | E::ResourceExhausted(_)
            | E::PayloadTooLarge
            | E::Unavailable(_)
            | E::Timeout
            | E::IngressFenced(_)
            | E::Internal(_) => {}
        }
    }

    fn refusals() -> Vec<E> {
        let all = vec![
            E::InvalidRequest("bad key".into()),
            E::Unauthenticated,
            E::PermissionDenied,
            E::NotFound("workflow run".into()),
            E::Conflict("lease is live".into()),
            E::ResourceExhausted("limit reached".into()),
            E::PayloadTooLarge,
            E::Unavailable("engine down".into()),
            E::Timeout,
            E::IngressFenced(None),
            E::Internal("invariant broken".into()),
        ];
        let mut codes: Vec<&str> = all.iter().map(E::code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(
            codes.len(),
            all.len(),
            "each refusal needs its own instance here"
        );
        all
    }

    /// A creator branches on `error.code`, so no refusal may reach them
    /// without one. `InvalidRequest` is the case this guards: it used to
    /// return early as a code-less `TypeError`.
    #[test]
    fn every_refusal_carries_its_code() {
        let all = refusals();
        assert!(!all.is_empty());
        for variant in all {
            covered(&variant);
            let expected = variant.code();
            match to_op_error(variant.clone()).kind {
                OpErrorKind::CodedError { code, .. } => {
                    assert_eq!(code, expected, "{variant:?}");
                }
                other => panic!("{variant:?} reached creator code as {other:?}, not a code"),
            }
        }
    }

    /// The control below differs in one variable, the variant. Both keep the
    /// engine's own wording, so a creator reads why the call was refused.
    #[test]
    fn invalid_request_keeps_its_message() {
        assert_eq!(
            to_op_error(E::InvalidRequest("restart target is missing or ambiguous".into())).message,
            "restart target is missing or ambiguous"
        );
    }

    #[test]
    fn conflict_keeps_its_message() {
        assert_eq!(
            to_op_error(E::Conflict(
                "restart prefix contains unresolved operations".into()
            ))
            .message,
            "restart prefix contains unresolved operations"
        );
    }

    /// Two refusals name a host condition the creator cannot act on, so they
    /// are reported by their code alone rather than by the engine's wording.
    #[test]
    fn opaque_refusals_replace_their_message() {
        assert_eq!(
            to_op_error(E::Internal("journal row is corrupt".into())).message,
            "workflow operation failed"
        );
        assert_eq!(
            to_op_error(E::Unavailable("connection refused".into())).message,
            "workflow service is unavailable"
        );
    }

    /// The opaque arms discard their wording for the CREATOR by contract. The
    /// operator must still get it, so the discard is paired with a `warn!`
    /// carrying the original. Without that pairing a host condition reaches the
    /// log as one sentence no matter which of the dozens of construction sites
    /// produced it.
    #[test]
    fn an_opaque_refusal_records_its_discarded_wording() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buffer {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("capture buffer").extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for Buffer {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buffer = Buffer(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            to_op_error(E::Unavailable("workflow host policy not bound".into()));
            // The control: a variant whose message CROSSES intact needs no
            // warn, so its wording must not appear in the log either.
            to_op_error(E::Conflict("restart target already settled".into()));
        });

        let logged = String::from_utf8(buffer.0.lock().expect("capture buffer").clone())
            .expect("captured log is utf8");

        assert!(
            logged.contains("workflow host policy not bound"),
            "the discarded wording must reach the operator: {logged}"
        );
        assert!(
            !logged.contains("restart target already settled"),
            "a refusal that keeps its message must not also be logged: {logged}"
        );
    }
}
