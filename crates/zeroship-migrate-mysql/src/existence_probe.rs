use zeroship_migrate_backend::existence_probe::ExistenceProbePolicy;

#[derive(Debug)]
pub(crate) struct MysqlExistenceProbePolicy;

pub(crate) static POLICY: MysqlExistenceProbePolicy = MysqlExistenceProbePolicy;

impl ExistenceProbePolicy for MysqlExistenceProbePolicy {
    fn unique_index_carries_constraint_identity(&self) -> bool {
        // MySQL exposes a named table UNIQUE and its backing index as one key object.
        true
    }

    fn unresolved_constraint_drop_reason(&self) -> Option<&'static str> {
        // The catalog has CHECK name, clause, and ENFORCED metadata. The engine's
        // snapshot deliberately consumes only its own ID-format CHECKs as column
        // evidence, so arbitrary CHECK identity remains outside this probe's scope.
        Some(
            "<unknown: the MySQL snapshot's constraint scope excludes arbitrary CHECK identities, so not found does not prove absent>",
        )
    }

    fn normalize_constraint_definition(&self, definition: &str) -> String {
        normalize_fk_definition(definition)
    }

    fn truncated_identifier(&self, _authored: &str) -> Option<String> {
        // MySQL rejects names beyond its 64-character limit; it does not silently
        // create a different catalog identity by truncating them.
        None
    }
}

fn normalize_fk_definition(def: &str) -> String {
    const NEEDLE: &str = "REFERENCES ";
    let Some(pos) = def.find(NEEDLE) else {
        return def.to_string();
    };
    let after = pos + NEEDLE.len();
    let rest = &def[after..];
    // The referenced object reference runs up to the opening `(` of its column list.
    let Some(paren_rel) = rest.find('(') else {
        return def.to_string();
    };
    let obj = &rest[..paren_rel]; // e.g. `"schema".people` / `schema.people` / `people`
                                  // Keep only the FINAL dotted segment (the table), dropping any `<schema>.` prefix.
                                  // Handles quoted identifiers by splitting on the last `.`.
                                  //
                                  // **SAFE post-validation** - `rsplit('.')` would mis-split a referenced table
                                  // whose own (quoted) identifier contained a literal dot (e.g. `"a.b"`). That
                                  // case is UNREACHABLE here: the declared side is built by
                                  // [`crate::render::declarative::fk_definition_pg`] from a `target` that has already
                                  // passed `validate_ident` (rejects `.` in identifiers) and `reject_cross_app_ref`
                                  // (rejects dotted FK targets, `declarative.rs`), so the referenced table is
                                  // ALWAYS a single dot-free segment. The live side comes from
                                  // `pg_get_constraintdef`, which double-quotes such a name - but the catalog only
                                  // ever holds names this same author path created, so it is dot-free too. The
                                  // debug_assert pins that invariant; if identifier rules ever loosen this must
                                  // become a quote-aware split.
    let table_seg = obj.rsplit('.').next().unwrap_or(obj).trim();
    debug_assert!(
        !table_seg.trim_matches('"').contains('.'),
        "FK referenced table segment must be dot-free post-validation (validate_ident / \
         reject_cross_app_ref); got {table_seg:?} from {def:?}"
    );
    let mut out = String::with_capacity(def.len());
    out.push_str(&def[..after]);
    out.push_str(table_seg);
    out.push_str(&rest[paren_rel..]);
    out
}
