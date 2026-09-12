//! Convert storage errors into the JavaScript error contract.

use zeroship_kv::KvError;
use zeroship_runtime::state::OpError;

pub(crate) fn to_op_error(error: KvError) -> OpError {
    match error {
        KvError::InvalidKey { message }
        | KvError::InvalidValue { message }
        | KvError::InvalidArgument { message } => OpError::type_error(message),
        KvError::NonNumeric { message } => {
            OpError::coded("kv_non_numeric", message, None::<String>)
        }
        KvError::Overflow { message } => OpError::coded("kv_overflow", message, None::<String>),
        KvError::ListTooLarge { message } => {
            OpError::coded("kv_list_too_large", message, None::<String>)
        }
        KvError::Connection { message } => OpError::coded(
            "kv_connection",
            message,
            Some("transient backend failure; retry after a short backoff".to_string()),
        ),
        KvError::Backend { message } => OpError::coded("kv_backend", message, None::<String>),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_runtime::state::OpErrorKind;

    #[test]
    fn validation_variants_become_type_errors() {
        for e in [
            KvError::invalid_key("k"),
            KvError::invalid_value("v"),
            KvError::invalid_argument("a"),
        ] {
            assert!(e.is_validation());
            match to_op_error(e).kind {
                OpErrorKind::TypeError => {}
                other => panic!("expected TypeError, got {other:?}"),
            }
        }
    }

    #[test]
    fn coded_variants_stamp_canonical_codes() {
        let cases = [
            (KvError::non_numeric(""), "kv_non_numeric"),
            (KvError::overflow(""), "kv_overflow"),
            (
                KvError::ListTooLarge { message: "".into() },
                "kv_list_too_large",
            ),
            (KvError::connection(""), "kv_connection"),
            (KvError::backend(""), "kv_backend"),
        ];
        for (variant, expected) in cases {
            assert!(!variant.is_validation());
            match to_op_error(variant).kind {
                OpErrorKind::CodedError { code, .. } => assert_eq!(code, expected),
                other => panic!("expected CodedError, got {other:?}"),
            }
        }
    }

    #[test]
    fn connection_carries_retry_hint() {
        match to_op_error(KvError::connection("boom")).kind {
            OpErrorKind::CodedError { hint, .. } => assert!(hint.is_some()),
            other => panic!("expected CodedError, got {other:?}"),
        }
    }
}
