use crate::value::Value;
use std::fmt;

/// Native bind shape, without the parameter's contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParameterType {
    Null,
    Bool,
    Number,
    Text,
    Bytes,
    Timestamp,
    Decimal,
    Json,
    Array,
    Object,
}

impl ParameterType {
    pub(crate) fn of(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(_) => Self::Bool,
            Value::Number(_) => Self::Number,
            Value::String(_) => Self::Text,
            Value::Bytes(_) => Self::Bytes,
            Value::TimestampMicros(_) => Self::Timestamp,
            Value::Decimal(_) => Self::Decimal,
            Value::Json(_) => Self::Json,
            Value::Array(_) => Self::Array,
            Value::Object(_) => Self::Object,
        }
    }
}

/// Compiler output for execution. Debug formatting omits parameter contents.
#[derive(Clone, PartialEq)]
pub struct CompiledQuery {
    pub(crate) sql: String,
    pub(crate) params: Vec<Value>,
}

impl CompiledQuery {
    /// Assemble output from a dialect compiler. ORM callers prepare operations.
    pub fn new(sql: String, params: Vec<Value>) -> Self {
        Self { sql, params }
    }

    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// Native bindings for the execution adapter, in placeholder order.
    pub fn params(&self) -> &[Value] {
        &self.params
    }

    pub fn parameter_types(&self) -> impl ExactSizeIterator<Item = ParameterType> + '_ {
        self.params.iter().map(ParameterType::of)
    }

    /// Transfer the statement and its buffers to the execution adapter.
    pub fn into_parts(self) -> (String, Vec<Value>) {
        (self.sql, self.params)
    }
}

impl fmt::Debug for CompiledQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompiledQuery")
            .field("sql", &self.sql)
            .field(
                "parameter_types",
                &self.parameter_types().collect::<Vec<_>>(),
            )
            .finish()
    }
}
