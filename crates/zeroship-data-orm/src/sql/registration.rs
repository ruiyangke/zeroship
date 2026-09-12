//! Immutable SQL compiler and storage-codec registration.

use super::{
    compile::SqlDialect,
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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegistrationIdentity([u8; 32]);

impl RegistrationIdentity {
    fn new(
        configuration: &str,
        compiler: &str,
        codecs: &str,
        dialect: SqlDialect,
        support: SqlSupport,
    ) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"zeroship.orm.sql-registration\0");
        hash.update(configuration.as_bytes());
        hash.update(compiler.as_bytes());
        hash.update(codecs.as_bytes());
        hash.update(format!("\0{dialect:?}\0{support:?}").as_bytes());
        Self(hash.finalize().into())
    }

    pub(crate) fn contribute_to(self, hash: &mut Sha256) {
        hash.update(self.0);
    }
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
    dialect: SqlDialect,
    compiler: Arc<dyn SqlCompiler>,
    codecs: Arc<dyn SqlStorageCodecs>,
    effective: SqlSupport,
}

impl fmt::Debug for SqlRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqlRegistration")
            .field("identity", &self.identity)
            .field("dialect", &self.dialect)
            .field("effective", &self.effective)
            .finish_non_exhaustive()
    }
}

impl SqlRegistration {
    pub fn new<C, K>(
        identity: &str,
        dialect: SqlDialect,
        compiler: C,
        codecs: K,
        effective: SqlSupport,
    ) -> Result<Self, CompileError>
    where
        C: SqlCompiler + 'static,
        K: SqlStorageCodecs + 'static,
    {
        compiler.check(&Requirements::default(), &effective)?;
        Ok(Self {
            identity: RegistrationIdentity::new(
                identity,
                std::any::type_name::<C>(),
                std::any::type_name::<K>(),
                dialect,
                effective,
            ),
            dialect,
            compiler: Arc::new(compiler),
            codecs: Arc::new(codecs),
            effective,
        })
    }

    pub fn builtin(dialect: SqlDialect) -> Self {
        match dialect {
            SqlDialect::Postgres => {
                let compiler = PostgresCompiler;
                Self::new(
                    "builtin-postgres",
                    dialect,
                    compiler,
                    PostgresCodecs,
                    compiler.support(),
                )
                .expect("built-in PostgreSQL SQL registration")
            }
            SqlDialect::Sqlite => {
                let compiler = SqliteCompiler;
                Self::new(
                    "builtin-sqlite",
                    dialect,
                    compiler,
                    SqliteCodecs,
                    compiler.support(),
                )
                .expect("built-in SQLite SQL registration")
            }
        }
    }

    pub fn identity(&self) -> RegistrationIdentity {
        self.identity
    }

    pub fn dialect(&self) -> SqlDialect {
        self.dialect
    }

    pub fn support(&self) -> SqlSupport {
        self.effective
    }

    pub fn check(&self, requirements: &Requirements) -> Result<(), CompileError> {
        self.compiler.check(requirements, &self.effective)
    }

    pub fn compile(&self, statement: Statement) -> Result<CompiledQuery, CompileError> {
        self.compiler.compile(statement, &self.effective)
    }

    pub fn compile_identity_allocation(
        &self,
        request: crate::sql::statement::IdentityRequest,
    ) -> Result<crate::sql::compiler::IdentityPlan, CompileError> {
        self.check(&Requirements {
            identity_allocation: true,
            ..Requirements::default()
        })?;
        self.compiler
            .compile_identity_allocation(request, &self.effective)
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
}
