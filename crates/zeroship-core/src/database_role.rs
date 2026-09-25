//! `PostgreSQL` role names shared by the migration and data planes.
//!
//! `PostgreSQL` limits identifier length through `NAMEDATALEN`. It truncates a
//! longer name with only a notice, so a role name
//! used as an authorization fence must be refused rather than shortened. A
//! truncation could otherwise make a newly derived name resolve to a role that
//! was meant to be reaped.
//!
//! The database-keyed names are where that refusal is load-bearing rather than
//! defensive. [`binding_role_name`] ends in the binding id, so a truncation
//! eats exactly the component that tells one binding's role from another's and
//! two bindings land on one role - the role each of them is the only way to
//! revoke. The collision and the refusal that prevents it are exhibited by
//! `tests::truncating_an_over_long_binding_role_would_collapse_two_bindings`.
//!
//! Every composer here takes text and returns text, because a role name is a
//! physical identifier and not an identity: the app-keyed composer takes a
//! schema name that can differ from the platform id outright, and this is the
//! layer at which a name too long for `PostgreSQL` can be exhibited.
//! `zeroship_core::database_derivation` is the typed seam over the
//! database-keyed composers.

/// `PostgreSQL`'s default identifier limit (`NAMEDATALEN - 1`), in bytes.
pub const POSTGRES_IDENTIFIER_MAX_BYTES: usize = 63;

/// Why a per-app `PostgreSQL` role name could not be composed safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PerAppRoleNameError {
    /// The complete, unmodified role name does not fit in a `PostgreSQL`
    /// identifier. The composer never truncates or hashes authorization roles.
    #[error("per-app PostgreSQL role name is {actual_bytes} bytes; maximum is {max_bytes} bytes")]
    TooLong {
        actual_bytes: usize,
        max_bytes: usize,
    },
}

/// A composed role name `PostgreSQL` would have truncated, refused instead.
///
/// One failure mode, so one type: a composer here either returns the complete
/// name or this. There is no shortening arm to select between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("derived PostgreSQL role name is {actual_bytes} bytes; maximum is {max_bytes} bytes")]
pub struct RoleNameTooLong {
    pub actual_bytes: usize,
    pub max_bytes: usize,
}

/// The privilege set one binding holds on one database.
///
/// The migrator is deliberately absent: it owns the schema and is named by no
/// binding, so a capability value can never compose the owner's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DatabaseCapability {
    /// `USAGE` on the schema plus the column-listed DML grants.
    ReadWrite,
    /// `USAGE` on the schema plus the column-listed `SELECT` grants.
    ReadOnly,
}

impl DatabaseCapability {
    /// The trailing component that distinguishes this capability's role from
    /// the other's.
    #[must_use]
    pub const fn role_suffix(self) -> &'static str {
        match self {
            Self::ReadWrite => "rw",
            Self::ReadOnly => "ro",
        }
    }

    /// The spelling `zeroship.database_bindings.capability` stores.
    ///
    /// The column's CHECK admits exactly these two values
    /// (`db/migrations-ts/20260919000200_database_entities.ts`) and the
    /// control-plane surface refuses anything else before it reaches a
    /// statement. Writing them once here is what stops a producer and a
    /// consumer disagreeing about which text names which capability: the
    /// cluster reconciler reads the stored text back and composes a role name
    /// from it, and a second spelling would compose a role nothing created.
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::ReadWrite => "readwrite",
            Self::ReadOnly => "readonly",
        }
    }

    /// Parse the stored spelling, or refuse it.
    ///
    /// `None` is not a shrug. A row whose capability this cannot read is a row
    /// whose binding role cannot be composed, so the caller has to stop rather
    /// than pick a capability for it.
    #[must_use]
    pub fn from_wire(text: &str) -> Option<Self> {
        match text {
            "readwrite" => Some(Self::ReadWrite),
            "readonly" => Some(Self::ReadOnly),
            _ => None,
        }
    }

    /// Whether a binding holding this capability may modify rows.
    ///
    /// The answer is the definition of the two variants and lives here, once,
    /// so a caller cannot ask with a `matches!` whose wildcard arm would give a
    /// third capability the read-only answer by omission. The match below is
    /// exhaustive, so a third variant does not compile until it says which it
    /// is.
    ///
    /// **`PostgreSQL` is what enforces this, not the callers of this method.**
    /// The reconciler grants a binding role membership in exactly one
    /// capability role
    /// (`zeroship_migrate_server::datastore::cluster::grant_binding_statements`,
    /// `GRANT {capability} TO {binding} WITH SET FALSE`), so a session narrowed
    /// to a read-only binding is refused by the server whatever any process
    /// believes. A data-plane caller reading this is choosing what to SAY about
    /// an operation the server would refuse anyway.
    #[must_use]
    pub const fn permits_writes(self) -> bool {
        match self {
            Self::ReadWrite => true,
            Self::ReadOnly => false,
        }
    }
}

/// The wire form is [`DatabaseCapability::as_wire`], never a second spelling a
/// derive produced.
///
/// A `#[derive(Serialize)]` with a rename rule would stand a SECOND codec beside
/// the one `zeroship.database_bindings.capability` and the cluster reconciler
/// already share, and the two would be free to drift: a rename rule edited here
/// would move the worker version feed off the text control stores while every
/// `as_wire` caller stayed put. Delegating leaves one spelling and one parse, so
/// a version-feed entry and a binding row cannot disagree about which text names
/// which capability.
impl serde::Serialize for DatabaseCapability {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_wire())
    }
}

/// The parse is [`DatabaseCapability::from_wire`], and a text it refuses is
/// refused here.
///
/// No fallback arm and no default: both capabilities are values this field
/// carries, so whichever a default picked would be the correct reading for some
/// live binding and a silent misreading for the other - and the direction that
/// goes wrong quietly is the read-write one, where nothing says anything until
/// `PostgreSQL` produces a bare `42501` at the first write.
impl<'de> serde::Deserialize<'de> for DatabaseCapability {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'_, str> as serde::Deserialize>::deserialize(deserializer)?;
        // The offending text is NOT quoted back: this decodes a control
        // response, and the length of a field in one is not bounded here.
        Self::from_wire(&text).ok_or_else(|| serde::de::Error::custom("not a database capability"))
    }
}

/// Compose the per-app `PostgreSQL` role name.
///
/// The input is the physical schema name. It can differ from the platform
/// `AppId`; preserving its spelling keeps distinct schemas in distinct roles.
///
/// # Errors
///
/// Returns [`PerAppRoleNameError::TooLong`] rather than allowing `PostgreSQL` to
/// silently truncate a name beyond [`POSTGRES_IDENTIFIER_MAX_BYTES`].
pub fn per_app_role_name(schema_name: &str) -> Result<String, PerAppRoleNameError> {
    refuse_truncation(format!("app_{schema_name}_role")).map_err(|too_long| {
        PerAppRoleNameError::TooLong {
            actual_bytes: too_long.actual_bytes,
            max_bytes: too_long.max_bytes,
        }
    })
}

/// Compose the role that OWNS a database's schema and applies its DDL.
///
/// It is held by the migration service alone. No binding names it, so no app
/// can reach schema change through role membership.
///
/// # Errors
///
/// [`RoleNameTooLong`] rather than a name `PostgreSQL` would truncate onto
/// another database's.
pub fn database_migrator_role_name(database_id: &str) -> Result<String, RoleNameTooLong> {
    refuse_truncation(format!("zs_db_{database_id}_mig"))
}

/// Compose one of a database's two capability roles.
///
/// A binding role inherits exactly one of these, which is what keeps
/// per-statement confinement to one database while revoking one app's edge
/// still bites.
///
/// # Errors
///
/// [`RoleNameTooLong`] rather than a truncated name. Truncation here would
/// drop the capability suffix and hand a readonly binding the readwrite role.
pub fn database_capability_role_name(
    database_id: &str,
    capability: DatabaseCapability,
) -> Result<String, RoleNameTooLong> {
    refuse_truncation(format!("zs_db_{database_id}_{}", capability.role_suffix()))
}

/// Compose the role that reaches a database's masked real-value columns.
///
/// It is the only role an apply grants `SELECT` on a `__zs_raw__` column to.
/// Both capability roles are granted the mask-bearing column and withheld the
/// sibling that holds the real value, so the plaintext of a classified field is
/// reachable only by assuming this role for the statement that reads it - which
/// is what keeps the audited unmask dispatcher the one path to that value
/// rather than one path among several.
///
/// A binding is granted membership in it `WITH INHERIT FALSE`, so the privilege
/// is never ambient on a narrowed session: a `SELECT *` under the binding role
/// still fails on the withheld column.
///
/// # Errors
///
/// [`RoleNameTooLong`] rather than a truncated name. Truncation here would eat
/// the suffix that tells this role from the database's other three.
pub fn database_unmask_role_name(database_id: &str) -> Result<String, RoleNameTooLong> {
    refuse_truncation(format!("zs_db_{database_id}_unmask"))
}

/// Compose the role one binding narrows to.
///
/// One role per binding, and the binding id is last, which is exactly why this
/// composer may not truncate: `PostgreSQL` drops the tail, so a shortened name
/// is the same name for two bindings whose ids share a prefix, and revoking
/// either would withdraw the other's access as well.
///
/// # Errors
///
/// [`RoleNameTooLong`] rather than a name `PostgreSQL` would truncate.
pub fn binding_role_name(binding_id: &str) -> Result<String, RoleNameTooLong> {
    refuse_truncation(format!("zs_bind_{binding_id}"))
}

/// Return `role` unchanged, or refuse it if `PostgreSQL` would have shortened it.
///
/// The one place the limit is compared, so a new composer cannot acquire a
/// different opinion about where it sits.
fn refuse_truncation(role: String) -> Result<String, RoleNameTooLong> {
    if role.len() > POSTGRES_IDENTIFIER_MAX_BYTES {
        return Err(RoleNameTooLong {
            actual_bytes: role.len(),
            max_bytes: POSTGRES_IDENTIFIER_MAX_BYTES,
        });
    }
    Ok(role)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_app_role_name_uses_the_physical_schema() {
        assert_eq!(per_app_role_name("app_demo").unwrap(), "app_app_demo_role");
        assert_eq!(
            per_app_role_name("creator_data").unwrap(),
            "app_creator_data_role"
        );
    }

    #[test]
    fn per_app_role_name_does_not_collapse_distinct_schemas() {
        assert_ne!(
            per_app_role_name("app-demo").unwrap(),
            per_app_role_name("app_demo").unwrap(),
            "hyphen and underscore schema names must map to distinct quoted roles"
        );
    }

    #[test]
    fn per_app_role_name_accepts_exactly_63_bytes() {
        let app_id = "a".repeat(54);
        let role = per_app_role_name(&app_id).expect("63-byte role name");
        assert_eq!(role.len(), POSTGRES_IDENTIFIER_MAX_BYTES);
    }

    #[test]
    fn per_app_role_name_refuses_64_bytes_without_shortening() {
        let app_id = "a".repeat(55);
        assert_eq!(
            per_app_role_name(&app_id),
            Err(PerAppRoleNameError::TooLong {
                actual_bytes: 64,
                max_bytes: POSTGRES_IDENTIFIER_MAX_BYTES,
            })
        );
    }

    #[test]
    fn per_app_role_name_limit_counts_bytes() {
        let app_id = "\u{e9}".repeat(28);
        assert_eq!(
            per_app_role_name(&app_id),
            Err(PerAppRoleNameError::TooLong {
                actual_bytes: 65,
                max_bytes: POSTGRES_IDENTIFIER_MAX_BYTES,
            })
        );
    }

    /// The spellings the cluster reconciler provisions, pinned to their bytes.
    #[test]
    fn database_keyed_role_names_are_pinned_spellings() {
        assert_eq!(
            database_migrator_role_name("dbs_demo").unwrap(),
            "zs_db_dbs_demo_mig"
        );
        assert_eq!(
            database_capability_role_name("dbs_demo", DatabaseCapability::ReadWrite).unwrap(),
            "zs_db_dbs_demo_rw"
        );
        assert_eq!(
            database_capability_role_name("dbs_demo", DatabaseCapability::ReadOnly).unwrap(),
            "zs_db_dbs_demo_ro"
        );
        assert_eq!(
            database_unmask_role_name("dbs_demo").unwrap(),
            "zs_db_dbs_demo_unmask"
        );
        assert_eq!(binding_role_name("bnd_demo").unwrap(), "zs_bind_bnd_demo");
    }

    /// The five names one database and one binding produce are five roles.
    ///
    /// The migrator owns the schema, the two capability roles carry different
    /// grants and the unmask role carries the real-value `SELECT` neither of
    /// them holds, so any pair collapsing onto one name would hand an app
    /// authority the design withheld.
    #[test]
    fn one_database_produces_five_distinct_roles() {
        let names = [
            database_migrator_role_name("dbs_demo").unwrap(),
            database_capability_role_name("dbs_demo", DatabaseCapability::ReadWrite).unwrap(),
            database_capability_role_name("dbs_demo", DatabaseCapability::ReadOnly).unwrap(),
            database_unmask_role_name("dbs_demo").unwrap(),
            binding_role_name("bnd_demo").unwrap(),
        ];
        let mut distinct = names.to_vec();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), names.len(), "composed {names:?}");
    }

    #[test]
    fn database_keyed_role_names_do_not_collapse_distinct_databases() {
        assert_ne!(
            database_migrator_role_name("dbs-demo").unwrap(),
            database_migrator_role_name("dbs_demo").unwrap()
        );
        assert_ne!(
            database_capability_role_name("dbs-demo", DatabaseCapability::ReadWrite).unwrap(),
            database_capability_role_name("dbs_demo", DatabaseCapability::ReadWrite).unwrap()
        );
        assert_ne!(
            database_unmask_role_name("dbs-demo").unwrap(),
            database_unmask_role_name("dbs_demo").unwrap()
        );
        assert_ne!(
            binding_role_name("bnd-demo").unwrap(),
            binding_role_name("bnd_demo").unwrap()
        );
    }

    /// The composer accepts a name that exactly fills the identifier.
    ///
    /// The control for the refusal below: without it, a composer that refused
    /// everything would pass that arm.
    #[test]
    fn binding_role_name_accepts_exactly_63_bytes() {
        let binding = "b".repeat(POSTGRES_IDENTIFIER_MAX_BYTES - "zs_bind_".len());
        let role = binding_role_name(&binding).expect("63-byte role name");
        assert_eq!(role.len(), POSTGRES_IDENTIFIER_MAX_BYTES);
        assert_eq!(role, format!("zs_bind_{binding}"));
    }

    #[test]
    fn binding_role_name_refuses_64_bytes_without_shortening() {
        let binding = "b".repeat(POSTGRES_IDENTIFIER_MAX_BYTES - "zs_bind_".len() + 1);
        assert_eq!(
            binding_role_name(&binding),
            Err(RoleNameTooLong {
                actual_bytes: POSTGRES_IDENTIFIER_MAX_BYTES + 1,
                max_bytes: POSTGRES_IDENTIFIER_MAX_BYTES,
            })
        );
    }

    /// What truncation would cost, exhibited rather than described.
    ///
    /// The binding id is the last component, so `PostgreSQL` shortening the
    /// name eats the bytes that tell two bindings apart. This arm builds the
    /// two names the composer is handed - independently of the composer, so
    /// the composed shape is checked too - shows that their first
    /// [`POSTGRES_IDENTIFIER_MAX_BYTES`] bytes are ONE name, and then shows
    /// the composer refusing both rather than returning it.
    #[test]
    fn truncating_an_over_long_binding_role_would_collapse_two_bindings() {
        let shared = "b".repeat(POSTGRES_IDENTIFIER_MAX_BYTES - "zs_bind_".len());
        let first = format!("zs_bind_{shared}1");
        let second = format!("zs_bind_{shared}2");

        assert_ne!(first, second, "the two bindings are two names in full");
        assert_eq!(first.len(), POSTGRES_IDENTIFIER_MAX_BYTES + 1);
        assert_eq!(second.len(), POSTGRES_IDENTIFIER_MAX_BYTES + 1);
        assert_eq!(
            first[..POSTGRES_IDENTIFIER_MAX_BYTES],
            second[..POSTGRES_IDENTIFIER_MAX_BYTES],
            "the collision being prevented: shortened to the identifier limit, \
             two bindings are one role"
        );

        for suffix in ['1', '2'] {
            let binding = format!("{shared}{suffix}");
            assert_eq!(
                binding_role_name(&binding),
                Err(RoleNameTooLong {
                    actual_bytes: POSTGRES_IDENTIFIER_MAX_BYTES + 1,
                    max_bytes: POSTGRES_IDENTIFIER_MAX_BYTES,
                }),
                "binding {binding} must be refused, not shortened onto its neighbour"
            );
        }
    }

    #[test]
    fn database_keyed_role_names_refuse_rather_than_shorten() {
        let database = "d".repeat(POSTGRES_IDENTIFIER_MAX_BYTES);
        assert_eq!(
            database_migrator_role_name(&database),
            Err(RoleNameTooLong {
                actual_bytes: "zs_db__mig".len() + POSTGRES_IDENTIFIER_MAX_BYTES,
                max_bytes: POSTGRES_IDENTIFIER_MAX_BYTES,
            })
        );
        for capability in [DatabaseCapability::ReadWrite, DatabaseCapability::ReadOnly] {
            assert_eq!(
                database_capability_role_name(&database, capability),
                Err(RoleNameTooLong {
                    actual_bytes: "zs_db__rw".len() + POSTGRES_IDENTIFIER_MAX_BYTES,
                    max_bytes: POSTGRES_IDENTIFIER_MAX_BYTES,
                }),
                "{capability:?} must be refused"
            );
        }
        assert_eq!(
            database_unmask_role_name(&database),
            Err(RoleNameTooLong {
                actual_bytes: "zs_db__unmask".len() + POSTGRES_IDENTIFIER_MAX_BYTES,
                max_bytes: POSTGRES_IDENTIFIER_MAX_BYTES,
            })
        );
    }

    #[test]
    fn database_keyed_role_name_limits_count_bytes() {
        let database = "\u{e9}".repeat(29);
        assert_eq!(
            database_migrator_role_name(&database),
            Err(RoleNameTooLong {
                actual_bytes: "zs_db__mig".len() + 58,
                max_bytes: POSTGRES_IDENTIFIER_MAX_BYTES,
            }),
            "a two-byte character counts twice against NAMEDATALEN"
        );
        assert_eq!(
            database.chars().count(),
            29,
            "the control: the same input is well under the limit counted in chars"
        );
    }

    /// The stored spelling round-trips, and nothing else parses.
    ///
    /// The refusal arm is the one that matters: a capability the reconciler
    /// cannot read must not silently become the other one, because the two
    /// compose different role names and one of them carries write grants.
    #[test]
    fn the_stored_capability_spelling_round_trips_and_refuses_everything_else() {
        let mut round_tripped = 0;
        for capability in [DatabaseCapability::ReadWrite, DatabaseCapability::ReadOnly] {
            assert_eq!(
                DatabaseCapability::from_wire(capability.as_wire()),
                Some(capability),
                "{capability:?} must survive its own spelling"
            );
            round_tripped += 1;
        }
        assert_eq!(round_tripped, 2, "the arm must not pass over an empty set");

        assert_eq!(DatabaseCapability::ReadWrite.as_wire(), "readwrite");
        assert_eq!(DatabaseCapability::ReadOnly.as_wire(), "readonly");
        assert_ne!(
            DatabaseCapability::ReadWrite.as_wire(),
            DatabaseCapability::ReadOnly.as_wire(),
            "two capabilities must not share one stored spelling"
        );

        for refused in ["", "READWRITE", "read_write", "rw", "owner", "readwrite "] {
            assert_eq!(
                DatabaseCapability::from_wire(refused),
                None,
                "`{refused}` is not a capability the CHECK admits"
            );
        }
    }

    /// The two capabilities disagree about writes, and the read-only one is the
    /// one that says no.
    ///
    /// Both arms, because a method that answered `true` for everything would
    /// satisfy the `ReadWrite` assertion alone, and one that answered `false`
    /// for everything would satisfy the `ReadOnly` one alone.
    #[test]
    fn only_the_readwrite_capability_permits_writes() {
        assert!(DatabaseCapability::ReadWrite.permits_writes());
        assert!(!DatabaseCapability::ReadOnly.permits_writes());

        // The capability that permits writes is the one whose role name carries
        // the write grants, so the two answers cannot be wired to each other's
        // role by a suffix that drifted.
        assert_eq!(DatabaseCapability::ReadWrite.role_suffix(), "rw");
        assert_eq!(DatabaseCapability::ReadOnly.role_suffix(), "ro");
    }

    /// `serde` writes and reads the SAME text the stored-column codec does.
    ///
    /// The assertion is against `as_wire` rather than against a literal: a
    /// literal here would be a third spelling, and the property is that there
    /// are not two. Both capabilities, because one arm alone would pass over an
    /// impl that emitted a constant.
    #[test]
    fn serde_writes_and_reads_the_stored_capability_spelling() {
        let mut checked = 0;
        for capability in [DatabaseCapability::ReadWrite, DatabaseCapability::ReadOnly] {
            let json = serde_json::to_string(&capability).expect("a capability serializes");
            assert_eq!(json, format!("\"{}\"", capability.as_wire()));
            assert_eq!(
                serde_json::from_str::<DatabaseCapability>(&json).expect("it reads back"),
                capability
            );
            checked += 1;
        }
        assert_eq!(checked, 2, "the arm must not pass over an empty list");
    }

    /// A text `from_wire` refuses is refused by the decoder too, rather than
    /// defaulted to a capability.
    ///
    /// Its control is the accepted spelling beside it: without one, a decoder
    /// that had begun refusing everything would pass this.
    #[test]
    fn a_capability_spelling_the_codec_refuses_does_not_decode() {
        assert!(serde_json::from_str::<DatabaseCapability>("\"readwrite\"").is_ok());
        for text in ["\"read_write\"", "\"rw\"", "\"\"", "\"owner\"", "7"] {
            assert!(
                serde_json::from_str::<DatabaseCapability>(text).is_err(),
                "{text} is not a capability and must not decode as one"
            );
        }
    }
}
