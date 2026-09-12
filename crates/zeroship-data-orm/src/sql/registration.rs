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
        support: SqlSupport,
    ) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"zeroship.orm.sql-registration\0");
        hash.update(configuration.as_bytes());
        hash.update(compiler.as_bytes());
        hash.update(codecs.as_bytes());
        hash.update(family.name().as_bytes());
        hash.update(format!("\0{support:?}").as_bytes());
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
    family: SqlFamily,
    compiler: Arc<dyn SqlCompiler>,
    codecs: Arc<dyn SqlStorageCodecs>,
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
        compiler.check(&Requirements::default(), &effective)?;
        Ok(Self {
            identity: RegistrationIdentity::new(
                identity,
                std::any::type_name::<C>(),
                std::any::type_name::<K>(),
                family,
                effective,
            ),
            family,
            compiler: Arc::new(compiler),
            codecs: Arc::new(codecs),
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

    pub fn decode_rows(
        &self,
        schema: &Value,
        rows: &mut [Value],
    ) -> Result<(), crate::sql::codecs::CodecError> {
        crate::sql::codecs::decode_rows(self, schema, rows)
    }
}
