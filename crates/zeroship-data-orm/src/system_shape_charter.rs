//! The operator-owned assignment authority, compiled into the worker.
//!
//! **Why the worker carries this at all.** The runtime descriptor is
//! creator-authored - `zeroship-migrate-server`'s `apply.rs` says so outright,
//! that "a creator who hand-edits both generated files can make them agree about
//! a lie". So the descriptor cannot be the authority for *who assigns a column's
//! value*. The charter can, because it ships with the binary.
//!
//! **Why a synthetic header.** The `.toml` on disk is deliberately a fragment: it
//! carries no `policy_version`, because migrate-server prefixes its own grants
//! and the TypeScript ceiling prefixes its own header. The worker needs only the
//! inject rule, so it prefixes the same header the TypeScript side does and
//! parses with an empty knob registry - the narrowest truthful configuration.
//!
//! **Why `include_str!` rather than a file read.** There is no runtime path, no
//! deployment step and no creator-controlled source: the bytes are in the binary.

use std::cell::RefCell;
use std::rc::Rc;

use zeroship_migrate_policy::{
    AssignmentEvent, AssignmentGenerator, PolicyRegistry, RootCharter, RuleKind,
};

use zeroship_data_orm::error::DbError;

/// The synthetic header, then the operator-shipped fragment.
const SYSTEM_SHAPE_CHARTER_TOML: &str = concat!(
    "policy_version = 1\n\n",
    include_str!("../../../policies/confined-system-shape.inject.toml"),
);

/// Parse the compiled charter once, during database-service construction.
///
/// The parsed value is retained by the shared plugin prototype; request paths
/// receive it and never re-parse this text.
pub fn load() -> Result<RootCharter, DbError> {
    RootCharter::parse_toml(SYSTEM_SHAPE_CHARTER_TOML, &PolicyRegistry::empty()).map_err(|source| {
        DbError::config(
            "system_shape_charter_invalid",
            format!("the embedded system-shape charter is invalid: {source:?}"),
        )
    })
}

/// One column the operator charter assigns: which generator computes its value,
/// and on which write event.
///
/// This is a PROJECTION of the charter, not a second declaration of it. Every
/// field is copied out of one [`zeroship_migrate_policy::InjectColumn`]; nothing
/// here is defaulted, inferred or renamed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignedColumn {
    /// The column name, exactly as the charter spells it.
    pub name: String,
    /// The generator that computes the value.
    pub by: AssignmentGenerator,
    /// The event that activates the generator.
    pub on: AssignmentEvent,
}

/// Every column the operator charter assigns, in charter order.
///
/// **This is what replaced the hardcoded name lists in the write pass.** The
/// pass iterates this and matches on [`AssignedColumn::by`] / [`AssignedColumn::on`];
/// it names no column. Adding an eighth platform column is a charter line.
///
/// Order is charter order, and that is load-bearing rather than incidental:
/// [`Self::columns`] is walked to build the INSERT document, and two columns
/// assigned by the same generator must be injected in the order the operator
/// declared them so the emitted SQL is stable across runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignmentPlan {
    columns: Vec<AssignedColumn>,
}

impl AssignmentPlan {
    /// Load the fixed operator policy and project runtime assignments.
    pub fn load() -> Result<Self, DbError> {
        load().map(|charter| Self::from_charter(&charter))
    }

    /// Project every assigned column out of a parsed charter's inject rules.
    ///
    /// Infallible on purpose. The only failure this could report is two inject
    /// rules assigning one column differently, and the loader already refuses
    /// that: `compose::inject_specs_collide` compares `assign` alongside
    /// type/nullable/default/collation, so two divergent injects collide at
    /// load rather than reaching here. The `debug_assert` states the assumption
    /// where it is relied on instead of inventing an error path no caller can
    /// take.
    pub fn from_charter(charter: &RootCharter) -> Self {
        let mut columns: Vec<AssignedColumn> = Vec::new();
        for rule in &charter.doc().rules {
            let RuleKind::Inject { spec } = &rule.kind else {
                continue;
            };
            for column in &spec.columns {
                let Some(assign) = &column.assign else {
                    continue;
                };
                let projected = AssignedColumn {
                    name: column.name.clone(),
                    by: assign.by,
                    on: assign.on,
                };
                if let Some(existing) = columns.iter().find(|c| c.name == projected.name) {
                    debug_assert_eq!(
                        *existing, projected,
                        "two inject rules assign `{}` differently; the loader's collision \
                         comparator should have refused this charter",
                        projected.name,
                    );
                    continue;
                }
                columns.push(projected);
            }
        }
        Self { columns }
    }

    /// Every assigned column, in charter order.
    pub fn columns(&self) -> &[AssignedColumn] {
        &self.columns
    }

    /// Columns whose value is fixed when the row is created and may never be
    /// re-assigned: `on = "insert"`.
    ///
    /// This is where `IMMUTABLE_SYSTEM_FIELDS` went. Immutability is DERIVED
    /// from the event rather than declared as its own property - the design
    /// decision that `assign = { by, on }` is one property, not three.
    pub fn immutable_after_insert(&self) -> impl Iterator<Item = &str> {
        self.columns
            .iter()
            .filter(|column| column.on == AssignmentEvent::Insert)
            .map(|column| column.name.as_str())
    }

    /// Columns the platform re-assigns on every write: `on = "write"`.
    pub fn reassigned_on_write(&self) -> impl Iterator<Item = &str> {
        self.columns
            .iter()
            .filter(|column| column.on == AssignmentEvent::Write)
            .map(|column| column.name.as_str())
    }
}

/// This worker thread's projection of the operator charter.
///
/// The plugin stamps it during the adapter tier's `DbPlugin::register`, the same way it
/// stamps the meter and the resource key. A vector that never registered a
/// plugin - the unit tests, and the `test-helpers` integration targets that
/// drive the pass directly - derives it here on first use.
///
/// **That fallback is not a second authority.** Both paths parse
/// [`SYSTEM_SHAPE_CHARTER_TOML`], which is `include_str!`-ed from the operator's
/// one file; there is no configuration, no descriptor and no environment in
/// either path, so the two cannot disagree. What the stamp buys is that a
/// production worker fails at composition rather than inside its first write.
pub fn plan() -> Result<Rc<AssignmentPlan>, DbError> {
    if let Some(plan) = PLAN.with_borrow(Clone::clone) {
        return Ok(plan);
    }
    let plan = Rc::new(AssignmentPlan::from_charter(&load()?));
    PLAN.with_borrow_mut(|slot| *slot = Some(Rc::clone(&plan)));
    Ok(plan)
}

thread_local! {
    /// This thread's projection, held HERE rather than as a field on the
    /// adapter's `ThreadDbContext`.
    ///
    /// `AssignmentPlan` is this module's own type and `plan()` is its only
    /// reader, so parking the slot on the adapter meant the engine reached UP
    /// into the adapter twice - once to read, once to memoise - to consult a
    /// value it owns outright. The adapter's remaining involvement is [`stamp`],
    /// which is a downward call and therefore fine.
    static PLAN: RefCell<Option<Rc<AssignmentPlan>>> = const { RefCell::new(None) };
}

/// Stamp this thread's projection at composition time.
///
/// Called from `DbPlugin::register`, so a production worker fails at
/// composition rather than inside its first write; see [`plan`] for why the
/// lazy fallback there is not a competing authority.
pub fn stamp(plan: Rc<AssignmentPlan>) {
    PLAN.with_borrow_mut(|slot| *slot = Some(plan));
}

/// Drop this thread's projection, so the next [`plan`] re-derives.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_tests() {
    PLAN.with_borrow_mut(|slot| *slot = None);
}

#[cfg(test)]
mod tests {
    use zeroship_migrate_policy::RuleKind;

    use super::*;

    /// The fragment must receive exactly the header the TypeScript ceiling
    /// prepends. If the two ever diverge, the worker and the build would be
    /// reading the same file as two different documents.
    #[test]
    fn the_fragment_takes_the_same_synthetic_header_as_typescript() {
        assert!(
            SYSTEM_SHAPE_CHARTER_TOML.starts_with("policy_version = 1\n\n"),
            "the compiled charter must open with the synthetic header",
        );
        // The fragment itself opens with its own prose banner, so this asserts
        // the rule survives the concatenation rather than that it comes first.
        assert!(
            SYSTEM_SHAPE_CHARTER_TOML.contains("\n[[inject]]"),
            "the concatenated charter must still carry the inject rule",
        );
        // The fragment must supply no version KEY of its own, or the synthetic
        // header would be a duplicate. Assert this structurally: a substring
        // count reads the fragment's own prose, which says "no policy_version"
        // and would make a textual check pass or fail for the wrong reason.
        let charter = load().expect("the compiled worker charter must parse");
        assert_eq!(
            charter.doc().policy_version,
            zeroship_migrate_policy::SUPPORTED_POLICY_VERSION,
            "the parsed version must be the one the synthetic header supplied",
        );
    }

    /// This is the test that binds the design: every column the operator injects
    /// carries a parsed assignment. A charter that parses but assigns nothing
    /// would let the runtime fall back to trusting the descriptor, which is the
    /// exact hole this module exists to close.
    #[test]
    fn every_injected_column_carries_a_parsed_assignment() {
        let charter = load().expect("the compiled worker charter must parse");

        let mut inject_rules = charter.doc().rules.iter().filter_map(|rule| {
            let RuleKind::Inject { spec } = &rule.kind else {
                return None;
            };
            Some(spec.columns.as_slice())
        });

        let columns = inject_rules
            .next()
            .expect("the worker charter must carry an inject rule");
        assert!(
            inject_rules.next().is_none(),
            "the system-shape fragment declares exactly one inject authority",
        );
        assert!(
            !columns.is_empty(),
            "the inject authority must own at least one column",
        );

        let unassigned: Vec<&str> = columns
            .iter()
            .filter(|column| column.assign.is_none())
            .map(|column| column.name.as_str())
            .collect();
        assert!(
            unassigned.is_empty(),
            "every operator-injected column must carry an assignment; these do not: {unassigned:?}",
        );
    }
}
