use super::{CompileError, CompiledQuery};
use crate::value::Value;
use std::fmt::Write;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ParameterSlot(usize);

/// Statement syntax and native binding share a single parameter budget.
pub(crate) struct SqlWriter {
    pub(crate) sql: String,
    params: Vec<Value>,
    limit: usize,
}

impl SqlWriter {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            sql: String::new(),
            params: Vec::new(),
            limit,
        }
    }

    pub(crate) fn identifier(&mut self, name: &str) {
        self.sql.push('"');
        for ch in name.chars() {
            if ch == '"' {
                self.sql.push('"');
            }
            self.sql.push(ch);
        }
        self.sql.push('"');
    }

    pub(crate) fn bind(&mut self, value: Value) -> Result<ParameterSlot, CompileError> {
        if self.params.len() >= self.limit {
            return Err(CompileError::BindLimitExceeded { limit: self.limit });
        }
        self.params.push(value);
        Ok(ParameterSlot(self.params.len()))
    }

    pub(crate) fn write_bound(&mut self, slot: ParameterSlot) {
        assert!(
            slot.0 <= self.params.len(),
            "parameter must be bound before use"
        );
        write!(self.sql, "${}", slot.0).expect("writing to String");
    }

    pub(crate) fn write_param(&mut self, value: Value) -> Result<(), CompileError> {
        let slot = self.bind(value)?;
        self.write_bound(slot);
        Ok(())
    }

    pub(crate) fn finish(self) -> CompiledQuery {
        CompiledQuery::new(self.sql, self.params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_quoted_and_native_buffers_move_into_bindings() {
        let bytes = vec![0, 255];
        let pointer = bytes.as_ptr();
        let mut writer = SqlWriter::new(2);
        writer.sql.push_str("SELECT ");
        writer.identifier("a\"b");
        writer.sql.push_str(" WHERE ");
        writer.identifier("payload");
        writer.sql.push_str(" = ");
        writer.write_param(Value::Bytes(bytes)).unwrap();
        let query = writer.finish();
        assert_eq!(query.sql(), "SELECT \"a\"\"b\" WHERE \"payload\" = $1");
        assert_eq!(query.params()[0].as_bytes().unwrap().as_ptr(), pointer);
    }

    #[test]
    fn reused_slots_do_not_consume_the_budget() {
        let mut writer = SqlWriter::new(1);
        let slot = writer.bind(Value::from("secret")).unwrap();
        writer.sql.push_str("SELECT ");
        writer.write_bound(slot);
        writer.sql.push_str(" = ");
        writer.write_bound(slot);
        assert_eq!(
            writer.bind(Value::Null).unwrap_err(),
            CompileError::BindLimitExceeded { limit: 1 }
        );
        let query = writer.finish();
        assert_eq!(query.sql(), "SELECT $1 = $1");
        assert_eq!(query.params(), &[Value::from("secret")]);
    }
}
