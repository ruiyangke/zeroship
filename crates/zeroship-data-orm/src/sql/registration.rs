//! Immutable SQL compiler and storage-codec registration.

use super::{
    compiler::{
        CompileError, CompiledQuery, PostgresCompiler, Requirements, SqlCompiler, SqlSupport,
        SqliteCompiler,
    },
    statement::{Statement, StorageType},
};
use crate::value::Value;
use sha2::{Digest, Sha256};
use std::{fmt, sync::Arc};

mod postgres;
mod sqlite;
use postgres::PostgresCodecs;
use sqlite::SqliteCodecs;

/// Open identifier for SQL implementations that share execution semantics.
/// Downstream backends choose their own globally unique static name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SqlFamily(&'static str);

impl SqlFamily {
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    const fn name(self) -> &'static str {
        self.0
    }
}

pub const POSTGRES_FAMILY: SqlFamily = SqlFamily::new("zeroship.postgresql");
pub const SQLITE_FAMILY: SqlFamily = SqlFamily::new("zeroship.sqlite");

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegistrationIdentity([u8; 32]);

impl RegistrationIdentity {
    fn new(
        configuration: &str,
        compiler: &str,
        codecs: &str,
        family: SqlFamily,
        implemented: SqlSupport,
        effective: SqlSupport,
    ) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"zeroship.orm.sql-registration\0");
        hash_component(&mut hash, configuration.as_bytes());
        hash_component(&mut hash, compiler.as_bytes());
        hash_component(&mut hash, codecs.as_bytes());
        hash_component(&mut hash, family.name().as_bytes());
        hash_component(
            &mut hash,
            format!("{implemented:?}\0{effective:?}").as_bytes(),
        );
        Self(hash.finalize().into())
    }

    pub(crate) fn contribute_to(self, hash: &mut Sha256) {
        hash.update(self.0);
    }
}

fn hash_component(hash: &mut Sha256, value: &[u8]) {
    let length = u64::try_from(value.len()).expect("registration identity component length");
    hash.update(length.to_le_bytes());
    hash.update(value);
}

impl fmt::Debug for RegistrationIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RegistrationIdentity(<opaque>)")
    }
}

pub trait SqlStorageCodecs: Send + Sync {
    fn storage_type(&self, definition: &Value) -> Result<StorageType, CompileError>;
    fn encode(&self, storage: StorageType, value: Value) -> Result<Value, CompileError>;
    fn decode(&self, storage: StorageType, value: Value) -> Result<Value, CompileError>;
}

#[derive(Clone)]
pub struct SqlRegistration {
    identity: RegistrationIdentity,
    family: SqlFamily,
    compiler: Arc<dyn SqlCompiler>,
    codecs: Arc<dyn SqlStorageCodecs>,
    implemented: SqlSupport,
    effective: SqlSupport,
}

impl fmt::Debug for SqlRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqlRegistration")
            .field("identity", &self.identity)
            .field("family", &self.family)
            .field("effective", &self.effective)
            .finish_non_exhaustive()
    }
}

impl SqlRegistration {
    pub fn new<C, K>(
        identity: &str,
        family: SqlFamily,
        compiler: C,
        codecs: K,
        effective: SqlSupport,
    ) -> Result<Self, CompileError>
    where
        C: SqlCompiler + 'static,
        K: SqlStorageCodecs + 'static,
    {
        let implemented = compiler.support();
        super::compiler::enforce_support(implemented, &Requirements::default(), &effective)?;
        compiler.check(&Requirements::default(), &effective)?;
        Ok(Self {
            identity: RegistrationIdentity::new(
                identity,
                std::any::type_name::<C>(),
                std::any::type_name::<K>(),
                family,
                implemented,
                effective,
            ),
            family,
            compiler: Arc::new(compiler),
            codecs: Arc::new(codecs),
            implemented,
            effective,
        })
    }

    pub fn postgres() -> Self {
        let compiler = PostgresCompiler;
        Self::new(
            "builtin-postgres",
            POSTGRES_FAMILY,
            compiler,
            PostgresCodecs,
            compiler.support(),
        )
        .expect("built-in PostgreSQL SQL registration")
    }

    pub fn sqlite() -> Self {
        let compiler = SqliteCompiler;
        Self::new(
            "builtin-sqlite",
            SQLITE_FAMILY,
            compiler,
            SqliteCodecs,
            compiler.support(),
        )
        .expect("built-in SQLite SQL registration")
    }

    pub fn identity(&self) -> RegistrationIdentity {
        self.identity
    }

    pub fn family(&self) -> SqlFamily {
        self.family
    }

    pub fn support(&self) -> SqlSupport {
        self.effective
    }

    pub fn check(&self, requirements: &Requirements) -> Result<(), CompileError> {
        super::compiler::enforce_support(self.implemented, requirements, &self.effective)?;
        self.compiler.check(requirements, &self.effective)
    }

    pub fn compile(&self, statement: Statement) -> Result<CompiledQuery, CompileError> {
        self.check(&Requirements::for_statement(&statement))?;
        let query = self.compiler.compile(statement, &self.effective)?;
        self.check_output(&query)?;
        Ok(query)
    }

    pub fn compile_identity_allocation(
        &self,
        request: crate::sql::statement::IdentityRequest,
    ) -> Result<crate::sql::compiler::IdentityPlan, CompileError> {
        self.check(&Requirements {
            identity_allocation: true,
            ..Requirements::default()
        })?;
        let plan = self
            .compiler
            .compile_identity_allocation(request, &self.effective)?;
        if let Some(reservation) = &plan.reservation {
            self.check_output(reservation)?;
        }
        match &plan.allocation {
            crate::sql::compiler::IdentityReadPlan::Rows(query) => self.check_output(query)?,
            crate::sql::compiler::IdentityReadPlan::MaximumAndCounter {
                maximum,
                counter_exists,
                counter,
            } => {
                self.check_output(maximum)?;
                self.check_output(counter_exists)?;
                self.check_output(counter)?;
            }
        }
        Ok(plan)
    }

    pub fn storage_type(&self, definition: &Value) -> Result<StorageType, CompileError> {
        self.codecs.storage_type(definition)
    }

    pub fn encode(&self, storage: StorageType, value: Value) -> Result<Value, CompileError> {
        self.codecs.encode(storage, value)
    }

    pub fn decode(&self, storage: StorageType, value: Value) -> Result<Value, CompileError> {
        self.codecs.decode(storage, value)
    }

    pub fn decode_rows(
        &self,
        schema: &Value,
        rows: &mut [Value],
    ) -> Result<(), crate::sql::codecs::CodecError> {
        crate::sql::codecs::decode_rows(self, schema, rows)
    }

    fn check_output(&self, query: &CompiledQuery) -> Result<(), CompileError> {
        if query.params().len() > self.effective.max_bind_parameters {
            return Err(CompileError::BindLimitExceeded {
                limit: self.effective.max_bind_parameters,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_identity_delimits_each_component() {
        let support = PostgresCompiler.support();
        let first =
            RegistrationIdentity::new("a", "bc", "d", SqlFamily::new("e"), support, support);
        let second =
            RegistrationIdentity::new("ab", "c", "d", SqlFamily::new("e"), support, support);
        assert_ne!(first, second);
    }
}
