//! Check logical update operations before row lookup or storage transforms.
use crate::error::DbError;
use crate::protection::protection_floor::{
    descriptor_declares_encryption, descriptor_declares_mask,
};
use crate::value::Value;
use crate::sql::update;

pub(super) fn validate(schema: &Value, patch: &Value) -> Result<(), DbError> {
    for assignment in update::assignments(patch)? {
        use update::Operator;
        let definition = &schema[assignment.field];
        if assignment.operator != Operator::Set
            && (descriptor_declares_encryption(definition) || descriptor_declares_mask(definition))
        {
            return Err(DbError::validation(
                "protected_update_operation",
                format!(
                    "column '{}' is protected; use a literal assignment instead of '{}'",
                    assignment.field,
                    assignment.operator.name()
                ),
            ));
        }
        assignment
            .operator
            .validate_type(assignment.field, definition)?;
    }
    Ok(())
}
