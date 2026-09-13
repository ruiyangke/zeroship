//! Check logical update operations before row lookup or storage transforms.
use crate::error::DbError;
use crate::protection::protection_floor::{
    descriptor_declares_encryption, descriptor_declares_mask,
};
use crate::schema::FieldMap;
use crate::sql::update;
use crate::value::Value;

pub(super) fn validate(schema: &FieldMap, patch: &Value) -> Result<(), DbError> {
    for assignment in update::assignments(patch)? {
        use update::Operator;
        let definition = schema.get(assignment.field).ok_or_else(|| {
            DbError::validation(
                "unknown_field",
                format!("field '{}' is not declared", assignment.field),
            )
        })?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ColumnSchema, LogicalType};

    #[test]
    fn update_validation_refuses_undeclared_fields_without_indexing_them() {
        let fields: FieldMap = [("title".into(), ColumnSchema::new(LogicalType::Text))].into();
        assert!(validate(&fields, &crate::value!({"title":"allowed"})).is_ok());
        for patch in [
            crate::value!({"unknown":"private"}),
            crate::value!({"$set":{"unknown":"private"}}),
        ] {
            let error = validate(&fields, &patch).unwrap_err();
            assert!(matches!(
                error,
                DbError::ValidationFailed {
                    code: "unknown_field",
                    ..
                }
            ));
            assert!(!error.to_string().contains("private"));
        }
    }
}
