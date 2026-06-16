//! The enumerated dangerous-construct rules — **data, not logic** (design §1.4).
//!
//! Every constant here names a vector from the threat model (§1.1). The guard
//! (`crate::guard`) consults these; keeping them as flat data makes the
//! security surface auditable at a glance and easy to extend.
//!
//! Comparisons are **case-insensitive** (callers lowercase first): SQL
//! identifiers/keywords are case-insensitive, and an attacker would otherwise
//! bypass with `PLPYTHONU` vs `plpythonu`.

/// Procedural languages that are *untrusted*.
///
/// They can escape the SQL sandbox into the host (filesystem, network, shell).
/// Only trusted PLs (`plpgsql`, `sql`) are permitted; everything else — incl.
/// anything ending in `u` (the Postgres convention for an untrusted variant) —
/// is denied by the guard's deny-by-default language rule.
pub const UNTRUSTED_LANGUAGES: &[&str] = &[
    "plpythonu",
    "plpython2u",
    "plpython3u",
    "plperlu",
    "pltclu",
    "plr",
    "plsh",
    "plv8", // executes arbitrary JS in-process
    "c",
    "internal",
];

/// Languages explicitly trusted for creator/AI-authored migrations.
pub const TRUSTED_LANGUAGES: &[&str] = &["plpgsql", "sql"];

/// Extensions that are *never* allowed regardless of the per-project allowlist.
///
/// They grant filesystem, network (SSRF), or RCE reach even if a creator's
/// allowlist is misconfigured — a belt over the deny-by-default allowlist.
pub const FORBIDDEN_EXTENSIONS: &[&str] = &[
    "dblink",
    "postgres_fdw",
    "file_fdw",
    "mysql_fdw",
    "oracle_fdw",
    "tds_fdw",
    "plpythonu",
    "plpython2u",
    "plpython3u",
    "plperlu",
    "pltclu",
    "plsh",
    "plr",
    "plv8",
    "adminpack", // exposes pg_file_write / server-file editing
    "amcheck",
    "pg_background",
    "lo",
];

/// Functions that read/write the server filesystem or large objects mapped to
/// files. Calling any of these is denied (the migrator role also lacks the
/// privilege; this is the parse-layer belt).
pub const FILE_ACCESS_FUNCTIONS: &[&str] = &[
    "pg_read_file",
    "pg_read_binary_file",
    "pg_ls_dir",
    "pg_ls_logdir",
    "pg_ls_waldir",
    "pg_stat_file",
    "lo_import",
    "lo_export",
    "pg_file_read",
    "pg_file_write",
    "pg_file_unlink",
    "pg_file_rename",
    "pg_logdir_ls",
];

/// Functions that reach other databases / the network (SSRF + cross-DB).
pub const NETWORK_FUNCTIONS: &[&str] = &[
    "dblink",
    "dblink_connect",
    "dblink_connect_u",
    "dblink_exec",
    "dblink_open",
    "dblink_fetch",
    "dblink_send_query",
    "dblink_get_result",
];

/// The `reg*` pseudo-type family (a `text → reg*` cast resolves a named object).
///
/// A `text → reg*` cast resolves the named object
/// (relation/schema/function/type/role/…) by name at runtime. A
/// schema-qualified name inside the *string literal* being cast
/// (`'control.users'::regclass`) is an `A_Const`, invisible to the structural
/// schema walker — so its leading `schema.` qualifier must be re-checked for
/// cross-schema confinement. Matched on the trailing (bare) type name.
pub const REG_TYPES: &[&str] = &[
    "regclass",
    "regnamespace",
    "regproc",
    "regprocedure",
    "regtype",
    "regoper",
    "regoperator",
    "regrole",
    "regcollation",
    "regconfig",
    "regdictionary",
];

/// Name-resolving builtins that take a text object name (the same leak in `FuncCall` form).
///
/// These take a text object name and resolve it (by
/// name) to a catalog object — the same string-literal-carried-schema leak as
/// the [`REG_TYPES`] casts, in `FuncCall` form. `setval`/`nextval`/`currval`
/// are cross-tenant *mutations* of a foreign sequence; the `to_reg*` family and
/// `pg_get_serial_sequence` are foreign-object resolvers/reads. The object name
/// is the FIRST argument; its leading `schema.` qualifier is re-checked for
/// cross-schema confinement. (`lastval` takes no argument and is omitted.)
///
/// Matched on the trailing (bare) function name — `pg_catalog.nextval(...)`
/// resolves the same builtin.
pub const NAME_RESOLVER_FUNCTIONS: &[&str] = &[
    "nextval",
    "currval",
    "setval",
    "to_regclass",
    "to_regnamespace",
    "to_regproc",
    "to_regprocedure",
    "to_regtype",
    "to_regoper",
    "to_regoperator",
    "to_regrole",
    "to_regcollation",
    "pg_get_serial_sequence",
];

/// Builtins whose first `text` argument is a **relation name** (NOT a
/// `regclass` cast).
///
/// A schema-qualified relation passed as a bare string
/// literal here reaches a foreign-tenant object the structural schema walker
/// never sees — the same leak class as [`NAME_RESOLVER_FUNCTIONS`], so the
/// literal's leading `schema.` qualifier is re-checked for confinement.
///
/// `pg_relation_size`/`pg_total_relation_size`/`pg_table_size`/`pg_indexes_size`
/// disclose a foreign table's on-disk size; the `has_*_privilege` family
/// discloses access bits on a foreign table. (`pg_relation_size('s.t'::regclass)`
/// goes through the [`REG_TYPES`] cast path instead — a non-literal/runtime arg
/// is out of parse-time scope, the line-2 role's job.)
///
/// Matched on the trailing (bare) function name — `pg_catalog.pg_relation_size`
/// resolves the same builtin.
pub const TEXT_RELATION_NAME_FUNCTIONS: &[&str] = &[
    "pg_relation_size",
    "pg_total_relation_size",
    "pg_table_size",
    "pg_indexes_size",
    "has_table_privilege",
    "has_any_column_privilege",
    "has_column_privilege",
    // The XML table-export family takes a `regclass`-coercible relation NAME
    // and dumps the relation's rows as XML — a foreign-table exfil vector.
    "table_to_xml",
    "table_to_xmlschema",
    "table_to_xml_and_xmlschema",
];

/// Builtins whose first `text` argument is a bare **schema name**.
///
/// The `schema_to_xml` family dumps an ENTIRE schema's contents as XML; pointed
/// at a foreign schema (`schema_to_xml('control', …)`) it exfiltrates the whole
/// platform/other-tenant schema. The literal IS the bare schema name (no
/// `schema.object` split — like `regnamespace`/`to_regnamespace`), so it is
/// re-checked as a namespace name for cross-schema confinement. Matched on the
/// trailing (bare) function name.
///
/// (`database_to_xml`/`database_to_xmlschema` carry NO schema literal — they
/// dump everything the role can see — so they are out of parse-time scope and
/// rely on the line-2 least-priv `migrator` role.)
pub const NAMESPACE_NAME_FUNCTIONS: &[&str] = &[
    "schema_to_xml",
    "schema_to_xmlschema",
    "schema_to_xml_and_xmlschema",
];

/// XML-emitting table functions whose first `text` argument is a free-form
/// **SQL string** that the server executes.
///
/// The embedded SQL is never re-parsed
/// by the structural walks, so a cross-schema read, a file-access function, or
/// a DDL hidden in that literal slips past — it must be re-parsed and run
/// through the SAME guard recursively.
///
/// `query_to_xml(text query, …)` / `query_to_xmlschema` /
/// `query_to_xml_and_xmlschema` take the SQL as arg 0;
/// `cursor_to_xml(refcursor, …)` / `cursor_to_xmlschema` take a cursor, but the
/// string-literal call form `cursor_to_xml('SELECT …', …)` still carries SQL.
/// Only the literal (`A_Const`) form is in parse-scope; a runtime-constructed
/// query argument is the line-2 role's job. Matched on the trailing (bare) name.
pub const SQL_STRING_ARG_FUNCTIONS: &[&str] = &[
    "query_to_xml",
    "query_to_xmlschema",
    "query_to_xml_and_xmlschema",
    "cursor_to_xml",
    "cursor_to_xmlschema",
];

/// The `set_config()` builtin — the function-call form of `SET <param>`.
///
/// `set_config(<param>, <value>, <is_local>)` is the function-call form of
/// `SET <param> = <value>`. The structural `VariableSetStmt` gate denies a
/// `SET search_path`/`role`/`session_authorization`, but `set_config()` is a
/// `FuncCall` that slips past it — so its FIRST string-literal argument is
/// checked against [`FORBIDDEN_SET_PARAMS`] and denied identically.
pub const SET_CONFIG_FUNCTION: &str = "set_config";

/// Object-name resolvers whose object name is a Postgres ARRAY literal.
///
/// The object name is a Postgres array literal whose FIRST element is the
/// schema (`pg_get_object_address('table','{schema,obj}', …)`). The schema is
/// value-carried inside the array text, invisible to the structural walker; its
/// first element is re-checked for cross-schema confinement. Matched on the
/// trailing (bare) function name.
pub const OBJECT_ADDRESS_FUNCTIONS: &[&str] = &["pg_get_object_address"];

/// Built-in roles whose membership grants the host-escape capabilities above.
/// Granting any of these (or to them) is privilege escalation.
pub const PRIVILEGED_ROLES: &[&str] = &[
    "pg_read_server_files",
    "pg_write_server_files",
    "pg_execute_server_program",
    "pg_read_all_data",
    "pg_write_all_data",
    "pg_monitor",
    "superuser",
    "postgres",
];

/// The platform's own schemas.
///
/// A creator migration has **zero** business touching these (design §1.7: total
/// isolation from the platform db). Used by the body lexical scan to catch a
/// cross-tenant reference hidden in a PL/pgSQL body that the structural
/// `RangeVar` check cannot see. The structural check still denies *every*
/// non-project schema; this list is the precise (no record-var false-positive)
/// body backstop for the schemas an injection would actually target.
pub const PLATFORM_SCHEMAS: &[&str] = &["control", "auth", "billing"];

/// `SET`-able parameters whose modification breaks confinement.
///
/// `search_path` escapes the pinned project schema; `role` / `session
/// authorization` switches identity (privilege escalation).
pub const FORBIDDEN_SET_PARAMS: &[&str] = &["search_path", "role", "session_authorization"];

/// Rule names (stable string ids surfaced in [`crate::guard::GuardError::Denied`]).
pub mod rule {
    pub const COPY_PROGRAM: &str = "copy_program_rce";
    pub const COPY_FILE: &str = "copy_file_access";
    pub const UNTRUSTED_LANGUAGE: &str = "untrusted_language";
    pub const FORBIDDEN_EXTENSION: &str = "forbidden_extension";
    pub const EXTENSION_NOT_ALLOWLISTED: &str = "extension_not_allowlisted";
    pub const ALTER_SYSTEM: &str = "alter_system";
    pub const ROLE_MANAGEMENT: &str = "role_management";
    pub const PRIVILEGE_MANAGEMENT: &str = "privilege_management";
    pub const FILE_ACCESS_FUNCTION: &str = "file_access_function";
    pub const NETWORK_FUNCTION: &str = "network_function";
    pub const FORBIDDEN_SET: &str = "forbidden_set_param";
    pub const SET_ROLE: &str = "set_role";
    pub const DATABASE_MANAGEMENT: &str = "database_management";
    pub const FDW_MANAGEMENT: &str = "fdw_management";
    pub const LOAD_LIBRARY: &str = "load_library";
    pub const UNRECOGNIZED_DANGEROUS: &str = "unrecognized_dangerous_construct";
    pub const BODY_INSPECTION: &str = "dangerous_construct_in_body";
    /// `CREATE/ALTER FUNCTION … SECURITY DEFINER` — runs with the definer's
    /// (migrator) privileges, an escalation primitive once installed.
    pub const SECURITY_DEFINER: &str = "security_definer_function";
    /// `ALTER FUNCTION … SET search_path = …` — pins a confinement-escaping
    /// `search_path` into a persisted function.
    pub const FUNCTION_SET_SEARCH_PATH: &str = "function_set_search_path";
    /// `ALTER … OWNER TO <privileged role>` — reparent an object to a
    /// platform/superuser role (privilege transfer).
    pub const OWNER_CHANGE: &str = "owner_change_to_privileged_role";
    /// An `ALTER TABLE` subcommand outside the safe migration set (e.g. OWNER
    /// TO, INHERIT, REPLICA IDENTITY, generic options).
    pub const UNSAFE_ALTER_TABLE_CMD: &str = "unsafe_alter_table_subcommand";
    /// A relation read/write targeting a system catalog
    /// (`pg_catalog.*` / unqualified `pg_*` / `information_schema.*`) — leaks
    /// roles/passwords/function source/cross-tenant metadata.
    pub const SYSTEM_CATALOG_ACCESS: &str = "system_catalog_access";
}

/// Case-insensitive membership against a static list.
#[must_use]
pub fn list_contains_ci(list: &[&str], needle: &str) -> bool {
    let n = needle.to_ascii_lowercase();
    list.iter().any(|e| e.eq_ignore_ascii_case(&n))
}

/// Is `lang` a trusted procedural language? (case-insensitive)
#[must_use]
pub fn is_trusted_language(lang: &str) -> bool {
    list_contains_ci(TRUSTED_LANGUAGES, lang)
}

/// Is `lang` untrusted?
///
/// Deny-by-default: only the explicitly [`TRUSTED_LANGUAGES`] (`plpgsql`,
/// `sql`) are trusted; every other language — listed or not — is untrusted.
#[must_use]
pub fn is_untrusted_language(lang: &str) -> bool {
    let l = lang.to_ascii_lowercase();
    if is_trusted_language(&l) {
        return false;
    }
    // Anything not explicitly trusted is untrusted: only plpgsql/sql pass.
    true
}
