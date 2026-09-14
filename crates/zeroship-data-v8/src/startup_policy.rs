//! Creator policy declarations stay isolate-local until native finalization.

use zeroship_data_orm::protection::mask_policy::{MaskPolicy, install_mask_policy};
use zeroship_data_orm::{binding::DbBinding, error::DbError, value::Value};
use zeroship_runtime::state::OpError;

struct StartupPolicy {
    binding: DbBinding,
    declaration: Value,
    finalized: bool,
}

pub(crate) fn initialize(scope: &mut v8::PinScope, binding: DbBinding) {
    scope.set_slot(StartupPolicy {
        binding,
        declaration: Value::Object(Default::default()),
        finalized: false,
    });
}

pub(crate) fn declare(
    scope: &mut v8::PinScope,
    binding: &DbBinding,
    value: v8::Local<v8::Value>,
) -> Result<(), OpError> {
    if !zeroship_runtime::plugin::startup_declarations_open(scope) {
        return Err(OpError::coded(
            "MASK_POLICY_IMMUTABLE",
            "defineMaskPolicy: policy is fixed after startup; edit the app and redeploy",
            None::<String>,
        ));
    }
    let value = {
        v8::tc_scope!(let tc, scope);
        match crate::v8_bridge::decode_native(tc, value) {
            Ok(value) => value,
            Err(crate::v8_bridge::DecodeError::PendingException) => {
                let error = tc
                    .exception()
                    .expect("decoder retained the original exception");
                return Err(OpError::js_value(tc, error, "mask policy getter threw"));
            }
            Err(
                crate::v8_bridge::DecodeError::Budget(reason)
                | crate::v8_bridge::DecodeError::Unsupported(reason),
            ) => {
                return Err(OpError::coded(
                    "INVALID_MASK_POLICY_SHAPE",
                    reason,
                    None::<String>,
                ));
            }
        }
    };
    MaskPolicy::from_json(&value).map_err(|error| {
        let code = match &error {
            DbError::ValidationFailed {
                code: "invalid_mask_classification",
                ..
            } => "INVALID_MASK_CLASSIFICATION",
            _ => "INVALID_MASK_POLICY_SHAPE",
        };
        OpError::coded(code, error.to_string(), None::<String>)
    })?;
    let policy = scope.get_slot_mut::<StartupPolicy>().ok_or_else(|| {
        OpError::coded(
            "DB_STARTUP_BINDING_REQUIRED",
            "database startup policy binding is missing",
            None::<String>,
        )
    })?;
    if policy.finalized || &policy.binding != binding {
        return Err(OpError::coded(
            "MASK_POLICY_IMMUTABLE",
            "database policy binding is sealed",
            None::<String>,
        ));
    }
    policy.declaration = value;
    Ok(())
}

pub(crate) fn finalize(scope: &mut v8::PinScope) -> Result<(), String> {
    let policy = scope
        .get_slot_mut::<StartupPolicy>()
        .ok_or("database startup policy binding is missing")?;
    install_mask_policy(&policy.binding, policy.declaration.clone())
        .map_err(|error| error.to_string())?;
    policy.finalized = true;
    Ok(())
}

/// Capture refusal before asynchronous database work can outlive startup.
pub(crate) fn require_finalized(scope: &mut v8::PinScope) -> Result<(), DbError> {
    if scope
        .get_slot::<StartupPolicy>()
        .is_some_and(|policy| policy.finalized)
    {
        Ok(())
    } else {
        Err(DbError::validation(
            "database_startup_pending",
            "unmasking requires the finalized startup mask policy",
        ))
    }
}
