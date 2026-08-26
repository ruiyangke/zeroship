use zero_migrate_backend::existence_probe::ExistenceProbePolicy;

/// This crate's DECLARED identifier cap, read off its own descriptor rather than
/// restated as a literal. A second `63` in the same crate as
/// [`crate::descriptor`]'s `IdentifierLimit::Bytes(63)` is two definitions of one
/// fact, and they drift silently - which is the defect the one-definition rule
/// closed for the three core sites.
const PG_MAX_IDENT_BYTES: usize = match crate::VENDOR.descriptor.limits.identifier {
    zero_migrate_ir::backend::IdentifierLimit::Bytes(n) => n,
    zero_migrate_ir::backend::IdentifierLimit::Unbounded
    | zero_migrate_ir::backend::IdentifierLimit::Characters(_) => {
        panic!("PostgreSQL declares a BYTE identifier cap")
    }
};

#[derive(Debug)]
pub(crate) struct PostgresExistenceProbePolicy;

pub(crate) static POLICY: PostgresExistenceProbePolicy = PostgresExistenceProbePolicy;

impl ExistenceProbePolicy for PostgresExistenceProbePolicy {
    fn unique_index_carries_constraint_identity(&self) -> bool {
        false
    }

    fn unresolved_constraint_drop_reason(&self) -> Option<&'static str> {
        // PostgreSQL snapshots every table constraint identity used by the probe.
        None
    }

    fn normalize_constraint_definition(&self, definition: &str) -> String {
        normalize_fk_definition(definition)
    }

    fn truncated_identifier(&self, authored: &str) -> Option<String> {
        (authored.len() > PG_MAX_IDENT_BYTES).then(|| pg_truncated_identifier(authored))
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

/// PostgreSQL's OWN spelling of an over-long identifier: the longest prefix of WHOLE
/// characters that fits in [`PG_MAX_IDENT_BYTES`].
///
/// The budget is bytes (NAMEDATALEN) but the clip is on a character boundary - verified
/// against a live PostgreSQL 18 server, where a 62-ASCII-byte prefix plus a two-byte
/// character (64 bytes) becomes the 62 ASCII bytes rather than a 63rd byte that would
/// split the codepoint.
fn pg_truncated_identifier(name: &str) -> String {
    let mut out = String::with_capacity(PG_MAX_IDENT_BYTES);
    for ch in name.chars() {
        if out.len() + ch.len_utf8() > PG_MAX_IDENT_BYTES {
            break;
        }
        out.push(ch);
    }
    out
}
