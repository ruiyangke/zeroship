//! The managed DUAL-WRITE TRIGGER for an expand-contract online rename.
//!
//! This is the whole of it: the PL/pgSQL body, the `CREATE OR REPLACE FUNCTION` /
//! `CREATE TRIGGER` pair that installs it, and the `DROP TRIGGER` / `DROP FUNCTION`
//! pair that removes it.
//!
//! # Where it used to live
//!
//! Split across the two crates that are supposed to hold no vendor. The engine's
//! `render::expand_contract` spelled `CREATE OR REPLACE FUNCTION ... LANGUAGE plpgsql`,
//! `CREATE TRIGGER ... BEFORE INSERT OR UPDATE ... EXECUTE FUNCTION`, and - twice -
//! `DROP TRIGGER <t> ON <table>; DROP FUNCTION <f>()`, which is not portable syntax in
//! either direction. The BODY sat one crate lower still, as
//! `zeroship_migrate_backend::capability::dual_write_function_body`.
//!
//! Only ONE token of that was catchable by a vendor-name census: `plpgsql`. The other
//! four statements name no vendor and are no less this backend's SQL, and the body
//! passed the contract crate's neutrality census for the whole of its life there.
//! That is worth stating plainly rather than filing as an incidental move: a census
//! over product names bounds how much vendor NAMING escapes, not how much vendor
//! GRAMMAR does.
//!
//! # SECURITY INVOKER, deliberately
//!
//! `LANGUAGE plpgsql` with no `SECURITY DEFINER` clause, which is the PL/pgSQL
//! default. The engine's own `createRole` gate refuses to hide SUPERUSER inside an
//! opaque body for the same reason; a dual-write trigger that ran as its definer
//! would be a privilege escalation attached to an ordinary column rename.

use zeroship_migrate_backend::schema::{DualWriteTriggerSpec, DualWriteTriggerSql};

/// The PL/pgSQL body of the managed dual-write trigger function.
///
/// Public within this crate so ONE speller is reached by both the author that writes
/// the trigger (this crate's `SchemaRenderer::dual_write_trigger` impl) and the
/// backfill guard that proves the LIVE trigger's source still matches it. It was
/// `zeroship_migrate_backend::capability::dual_write_function_body` - this grammar in the
/// neutral contract crate - until the engine stopped spelling the wrapper around it.
///
/// `CREATE OR REPLACE FUNCTION ... LANGUAGE plpgsql` (SECURITY INVOKER - the
/// plpgsql default; we deliberately emit NO `SECURITY DEFINER`). The body is
/// **total**: after it runs, `from` and `to` are ALWAYS equal, for every INSERT
/// and UPDATE - no input row is left divergent (a divergent pair would be
/// silently destroyed by the contract's `DROP COLUMN <from>`). Precedence is
/// **`to` wins** (consistent with the contract keeping `to`):
///
/// - on INSERT: if only `from` is set, mirror `from -> to`; otherwise (`to` set,
///   both set, or both NULL) copy `to -> from`;
/// - on UPDATE: if only `from` changed, mirror `from -> to`; otherwise (`to`
///   changed, both changed -> to wins, or neither changed -> no-op) copy
///   `to -> from`.
///
/// The only-`from` arm is `IS DISTINCT FROM`-guarded (NULL-safe). The else arm
/// is the total catch-all; when nothing changed it is a no-op self-copy, so an
/// UPDATE that touches neither column is not amplified.
pub(crate) fn dual_write_function_body(from_q: &str, to_q: &str) -> String {
    format!(
        "\nBEGIN\n\
         \x20   IF TG_OP = 'INSERT' THEN\n\
         \x20       IF NEW.{to_q} IS NULL AND NEW.{from_q} IS NOT NULL THEN\n\
         \x20           NEW.{to_q} := NEW.{from_q};   -- only from set\n\
         \x20       ELSE\n\
         \x20           NEW.{from_q} := NEW.{to_q};   -- to set / both set (to wins) / both null (no-op)\n\
         \x20       END IF;\n\
         \x20   ELSE\n\
         \x20       -- UPDATE: TOTAL, to wins. Only-from-changed mirrors from→to;\n\
         \x20       -- to-changed / both-changed / neither-changed all resolve to→from.\n\
         \x20       IF NEW.{from_q} IS DISTINCT FROM OLD.{from_q}\n\
         \x20          AND NEW.{to_q} IS NOT DISTINCT FROM OLD.{to_q} THEN\n\
         \x20           NEW.{to_q} := NEW.{from_q};   -- only from changed\n\
         \x20       ELSE\n\
         \x20           NEW.{from_q} := NEW.{to_q};   -- to changed / both changed (to wins) / neither (no-op)\n\
         \x20       END IF;\n\
         \x20   END IF;\n\
         \x20   RETURN NEW;\n\
         END;\n"
    )
}

/// Install + remove for one dual-write trigger.
///
/// `install` is `CREATE OR REPLACE` + `CREATE TRIGGER`, so re-running it is safe;
/// `remove` is `IF EXISTS` on both, so tearing down a partly-applied install is safe.
/// The engine uses `remove` in three places and never composes it itself.
pub(crate) fn dual_write_trigger(spec: &DualWriteTriggerSpec<'_>) -> DualWriteTriggerSql {
    let DualWriteTriggerSpec {
        function,
        trigger,
        table,
        from,
        to,
    } = *spec;

    // The function body. `$zsdw$` dollar-quote so embedded SQL needs no escaping.
    // BEGIN ... RETURN NEW: a BEFORE trigger mutates NEW in place (never re-issues a
    // write -> no recursion).
    let body = dual_write_function_body(from, to);
    let func = format!(
        "CREATE OR REPLACE FUNCTION {function}() RETURNS trigger AS \
         $zsdw${body}$zsdw$ LANGUAGE plpgsql"
    );

    // BEFORE INSERT OR UPDATE, FOR EACH ROW. One trigger for both events and no WHEN
    // clause: PostgreSQL forbids OLD in a WHEN for the INSERT event, and the body is
    // already total + self-no-op (a no-op UPDATE falls into the to->from else arm,
    // which writes the same value back - no amplification). The body is the
    // authoritative guard.
    let create_trigger = format!(
        "CREATE TRIGGER {trigger} BEFORE INSERT OR UPDATE ON {table}\n\
         FOR EACH ROW EXECUTE FUNCTION {function}()"
    );

    DualWriteTriggerSql {
        install: format!("{func};\n{create_trigger}"),
        // Trigger before function: the function cannot be dropped while a trigger
        // still references it. `IF EXISTS` on both so this is idempotent.
        remove: format!(
            "DROP TRIGGER IF EXISTS {trigger} ON {table}; DROP FUNCTION IF EXISTS {function}()"
        ),
    }
}
