//! Pure data for large-table backfill plan steps.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::model::ir::PerRowGenerator;

const CROCKFORD_UPPER: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CROCKFORD_LOWER: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// One per-row generator assignment whose destination contract was validated by
/// the IR planner.
///
/// The type is public because [`BackfillSpec`] is part of the backend API, but
/// its payload and constructor are crate-private. External backend callers can
/// therefore keep constructing ordinary specs with an empty `per_row` map, but
/// cannot bypass schema-family validation by manufacturing a generator-bearing
/// spec directly.
///
/// ```compile_fail
/// use zero_migrate::model::backfill::PerRowAssignment;
/// use zero_migrate::PerRowGenerator;
///
/// let _ = PerRowAssignment {
///     generator: PerRowGenerator::UuidV4,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerRowAssignment {
    schema: String,
    table: String,
    column: String,
    generator: PerRowGenerator,
}

impl PerRowAssignment {
    /// Mint an assignment after planner validation of its destination contract.
    #[must_use]
    pub(crate) fn validated(
        schema: impl Into<String>,
        table: impl Into<String>,
        column: impl Into<String>,
        generator: PerRowGenerator,
    ) -> Self {
        Self {
            schema: schema.into(),
            table: table.into(),
            column: column.into(),
            generator,
        }
    }

    /// Read the validated generator at the executor boundary.
    #[must_use]
    pub(crate) fn generator(&self) -> &PerRowGenerator {
        &self.generator
    }

    /// Confirm that a cloned planner token has not been moved to another target.
    #[must_use]
    pub(crate) fn matches_target(&self, schema: &str, table: &str, column: &str) -> bool {
        self.schema == schema && self.table == table && self.column == column
    }
}

/// The structured definition of a large-table backfill.
///
/// The creator/AI supplies the **target** ([`table`](Self::table)), the ordered
/// key to page by ([`cursor_column`](Self::cursor_column)), the
/// [`batch_size`](Self::batch_size), the per-row transform
/// ([`set_clause`](Self::set_clause)), and an optional row
/// [`filter`](Self::filter). The engine owns everything else — the cursor-window
/// predicate, the parameter binding, the loop control — so the authored input
/// can never escape the target table or inject into the loop control.
#[derive(Debug, Clone)]
pub struct BackfillSpec {
    /// The effective schema the backfill targets.
    pub schema: String,
    /// The target table — a bare identifier in [`schema`](Self::schema).
    pub table: String,
    /// The ordered key to page by.
    pub cursor_column: String,
    /// Rows per batch.
    pub batch_size: u32,
    /// The authored per-row transform — the body of `UPDATE … SET <here>`.
    pub set_clause: String,
    /// Apply-engine assignments evaluated independently for every selected row.
    /// Kept separate from `set_clause` so no sampled literal can be rendered into
    /// the plan and reused across a batch.
    pub per_row: BTreeMap<String, PerRowAssignment>,
    /// An optional authored row filter.
    pub filter: Option<String>,
    /// A human-readable name for the backfill.
    pub name: String,
}

impl BackfillSpec {
    /// A stable identity for this backfill, used as the progress-row PK so a
    /// resumed run finds its own progress.
    #[must_use]
    pub fn backfill_id(&self) -> String {
        let mut h = Sha256::new();
        for field in [
            self.name.as_str(),
            self.schema.as_str(),
            self.table.as_str(),
            self.cursor_column.as_str(),
            self.set_clause.as_str(),
            self.filter.as_deref().unwrap_or("\u{0}<none>"),
        ] {
            h.update((field.len() as u64).to_be_bytes());
            h.update(field.as_bytes());
        }
        // Preserve the pre-feature identity byte-for-byte for ordinary
        // backfills. The domain marker makes new generator-bearing identities
        // self-delimiting without orphaning existing progress rows.
        if !self.per_row.is_empty() {
            h.update(b"\0perRow/v1");
            h.update((self.per_row.len() as u64).to_be_bytes());
            for (column, assignment) in &self.per_row {
                let generator = assignment.generator();
                h.update((column.len() as u64).to_be_bytes());
                h.update(column.as_bytes());
                match generator {
                    PerRowGenerator::UuidV4 => h.update(b"uuidV4"),
                    PerRowGenerator::UuidV7 => h.update(b"uuidV7"),
                    PerRowGenerator::TypeId { prefix } => {
                        h.update(b"typeId");
                        h.update((prefix.len() as u64).to_be_bytes());
                        h.update(prefix.as_bytes());
                    }
                    PerRowGenerator::Ulid => h.update(b"ulid"),
                }
            }
        }
        h.update(self.batch_size.to_be_bytes());
        format!("bf_{}", hex::encode(&h.finalize()[..16]))
    }
}

/// Generate one canonical value for one selected backfill row.
///
/// Calling this function is the evaluation boundary: lowering records only the
/// generator enum, and executors call it anew inside the batch transaction for
/// each destination row.
#[must_use]
pub(crate) fn generate_per_row_value(generator: &PerRowGenerator) -> String {
    match generator {
        PerRowGenerator::UuidV4 => uuid::Uuid::new_v4().to_string(),
        PerRowGenerator::UuidV7 => uuid::Uuid::now_v7().to_string(),
        PerRowGenerator::TypeId { prefix } => {
            let suffix = crockford_u128(uuid::Uuid::now_v7().as_u128(), CROCKFORD_LOWER);
            if prefix.is_empty() {
                suffix
            } else {
                format!("{prefix}_{suffix}")
            }
        }
        PerRowGenerator::Ulid => crockford_u128(uuid::Uuid::now_v7().as_u128(), CROCKFORD_UPPER),
    }
}

fn crockford_u128(mut value: u128, alphabet: &[u8; 32]) -> String {
    let mut out = [b'0'; 26];
    for byte in out.iter_mut().rev() {
        *byte = alphabet[(value & 0x1f) as usize];
        value >>= 5;
    }
    // Both alphabets are ASCII, so this conversion cannot fail.
    String::from_utf8(out.to_vec()).expect("Crockford alphabets are valid ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_crockford(value: &str, alphabet: &[u8; 32]) -> u128 {
        value.bytes().fold(0_u128, |decoded, byte| {
            let digit = alphabet
                .iter()
                .position(|candidate| *candidate == byte)
                .expect("generated Crockford digit") as u128;
            (decoded << 5) | digit
        })
    }

    #[test]
    fn generated_values_have_exact_canonical_families() {
        let v4 = generate_per_row_value(&PerRowGenerator::UuidV4);
        let parsed_v4 = uuid::Uuid::parse_str(&v4).expect("UUIDv4 parses");
        assert_eq!(parsed_v4.get_version_num(), 4);
        assert_eq!(parsed_v4.get_variant(), uuid::Variant::RFC4122);

        let v7 = generate_per_row_value(&PerRowGenerator::UuidV7);
        let parsed_v7 = uuid::Uuid::parse_str(&v7).expect("UUIDv7 parses");
        assert_eq!(parsed_v7.get_version_num(), 7);
        assert_eq!(parsed_v7.get_variant(), uuid::Variant::RFC4122);

        let type_id = generate_per_row_value(&PerRowGenerator::TypeId {
            prefix: "order".to_string(),
        });
        assert!(type_id.starts_with("order_"));
        assert_eq!(type_id.len(), 32);
        assert!(type_id[6..]
            .bytes()
            .all(|byte| CROCKFORD_LOWER.contains(&byte)));
        assert!(type_id.as_bytes()[6] <= b'7');
        let type_id_uuid = uuid::Uuid::from_u128(decode_crockford(&type_id[6..], CROCKFORD_LOWER));
        assert_eq!(
            type_id_uuid.get_version_num(),
            7,
            "TypeID suffix must encode UUIDv7 bytes"
        );
        assert_eq!(type_id_uuid.get_variant(), uuid::Variant::RFC4122);

        let ulid = generate_per_row_value(&PerRowGenerator::Ulid);
        assert_eq!(ulid.len(), 26);
        assert!(ulid.bytes().all(|byte| CROCKFORD_UPPER.contains(&byte)));
        assert!(ulid.as_bytes()[0] <= b'7');
    }

    #[test]
    fn generation_is_evaluated_for_each_call() {
        for generator in [
            PerRowGenerator::UuidV4,
            PerRowGenerator::UuidV7,
            PerRowGenerator::TypeId {
                prefix: "event".to_string(),
            },
            PerRowGenerator::Ulid,
        ] {
            let values = (0..32)
                .map(|_| generate_per_row_value(&generator))
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(values.len(), 32, "{generator:?} must run once per call");
        }
    }

    #[test]
    fn planner_assignment_is_bound_to_its_exact_destination() {
        let assignment =
            PerRowAssignment::validated("app", "events", "public_id", PerRowGenerator::UuidV7);
        assert!(assignment.matches_target("app", "events", "public_id"));
        assert!(!assignment.matches_target("other", "events", "public_id"));
        assert!(!assignment.matches_target("app", "other", "public_id"));
        assert!(!assignment.matches_target("app", "events", "other"));
    }

    #[test]
    fn generator_contract_is_part_of_the_progress_identity() {
        let mut spec = BackfillSpec {
            schema: "app".into(),
            table: "events".into(),
            cursor_column: "id".into(),
            batch_size: 100,
            set_clause: String::new(),
            per_row: BTreeMap::from([(
                "public_id".into(),
                PerRowAssignment::validated("app", "events", "public_id", PerRowGenerator::UuidV4),
            )]),
            filter: None,
            name: "fill_public_ids".into(),
        };
        let v4 = spec.backfill_id();
        spec.per_row.insert(
            "public_id".into(),
            PerRowAssignment::validated("app", "events", "public_id", PerRowGenerator::UuidV7),
        );
        let v7 = spec.backfill_id();
        spec.per_row.insert(
            "public_id".into(),
            PerRowAssignment::validated(
                "app",
                "events",
                "public_id",
                PerRowGenerator::TypeId {
                    prefix: "event".into(),
                },
            ),
        );
        let type_id = spec.backfill_id();
        assert_ne!(v4, v7);
        assert_ne!(v7, type_id);
        assert_ne!(v4, type_id);
    }
}
