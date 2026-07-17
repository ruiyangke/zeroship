//! Pure data for large-table backfill plan steps.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::model::ir::{CursorStability, IrScalar, PerRowGenerator};

const CROCKFORD_UPPER: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CROCKFORD_LOWER: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// The canonical tagged-scalar representation used by a cursor component.
///
/// Database integer families deliberately share one `int64` checkpoint shape:
/// using the untagged safe-integer JSON spelling for small values and the tagged
/// spelling for large values would make one column change wire type as paging
/// crosses the JavaScript-safe boundary. Date/time/UUID and character families
/// use the ordinary string scalar; their exact database type remains separately
/// pinned by [`CursorColumnContract::database_type`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CursorScalarType {
    /// Canonical `{ "int64": "..." }` scalar.
    Int64,
    /// Canonical `{ "decimal": "..." }` scalar.
    Decimal,
    /// Ordinary JSON string scalar.
    String,
}

/// Comparison semantics that must remain identical for the whole backfill.
///
/// This is intentionally catalog-facing rather than a portable collation hint.
/// Resume compares the exact value recorded at cohort initialization, so a
/// server-default collation change cannot silently change tuple ordering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase", deny_unknown_fields)]
pub enum CursorComparison {
    /// The target's default case-sensitive comparison for this exact type.
    Default,
    /// A portable/recovered case-insensitive comparison (`citext`/`NOCASE`).
    CaseInsensitive,
    /// An exact PostgreSQL or SQLite named collation.
    NamedCollation {
        /// PostgreSQL collation schema; absent for SQLite.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// Exact catalog collation name.
        name: String,
    },
    /// Exact MySQL character-set and collation comparison contract.
    MysqlText {
        /// Exact catalog character set.
        character_set: String,
        /// Exact catalog collation.
        collation: String,
    },
}

/// One position in the ordered cursor tuple.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CursorColumnContract {
    /// Cursor column name at this tuple position.
    pub name: String,
    /// Canonical tagged-scalar checkpoint family.
    pub scalar_type: CursorScalarType,
    /// Exact canonical target type used to cast/bind and detect resume drift.
    pub database_type: String,
    /// Exact comparison/collation semantics used by `=`, `>`, and `ORDER BY`.
    pub comparison: CursorComparison,
}

/// The planner-proven ordered cursor contract persisted with progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CursorContract {
    /// One contract entry per `cursorColumns` position, in declared order.
    pub columns: Vec<CursorColumnContract>,
}

impl CursorContract {
    /// Confirm this contract describes the exact authored tuple.
    pub fn validate_columns(&self, cursor_columns: &[String]) -> Result<(), CursorTupleError> {
        let recorded = self
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>();
        let expected = cursor_columns
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        if recorded != expected {
            return Err(CursorTupleError::Columns {
                expected: cursor_columns.to_vec(),
                actual: self
                    .columns
                    .iter()
                    .map(|column| column.name.clone())
                    .collect(),
            });
        }
        Ok(())
    }
}

/// A typed cursor checkpoint tuple encoded as one JSON scalar array.
///
/// The array uses [`IrScalar`]'s existing tagged wire codec verbatim. It is never
/// joined through a delimiter, so embedded NULs/delimiters and composite values
/// remain unambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorTuple(Vec<IrScalar>);

impl CursorTuple {
    /// Build and canonicalize a tuple against its planner-proven contract.
    pub fn new(values: Vec<IrScalar>, contract: &CursorContract) -> Result<Self, CursorTupleError> {
        if values.len() != contract.columns.len() {
            return Err(CursorTupleError::Arity {
                expected: contract.columns.len(),
                actual: values.len(),
            });
        }
        let values = values
            .into_iter()
            .zip(&contract.columns)
            .enumerate()
            .map(|(index, (value, column))| {
                canonical_cursor_scalar(value, column.scalar_type).map_err(|actual| {
                    CursorTupleError::ScalarType {
                        index,
                        expected: column.scalar_type,
                        actual,
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self(values))
    }

    /// Decode one JSON scalar array and validate its arity/types.
    pub fn from_json(value: &str, contract: &CursorContract) -> Result<Self, CursorTupleError> {
        let values = serde_json::from_str::<Vec<IrScalar>>(value)
            .map_err(|error| CursorTupleError::Json(error.to_string()))?;
        if values.len() != contract.columns.len() {
            return Err(CursorTupleError::Arity {
                expected: contract.columns.len(),
                actual: values.len(),
            });
        }
        for (index, (value, column)) in values.iter().zip(&contract.columns).enumerate() {
            let exact_wire_type = matches!(
                (column.scalar_type, value),
                (CursorScalarType::Int64, IrScalar::Int64(_))
                    | (CursorScalarType::Decimal, IrScalar::Decimal(_))
                    | (CursorScalarType::String, IrScalar::Str(_))
            );
            if !exact_wire_type {
                return Err(CursorTupleError::ScalarType {
                    index,
                    expected: column.scalar_type,
                    actual: cursor_scalar_kind(value),
                });
            }
        }
        Self::new(values, contract)
    }

    /// Encode this tuple through the existing tagged scalar wire codec.
    pub fn to_json(&self) -> Result<String, CursorTupleError> {
        serde_json::to_string(&self.0).map_err(|error| CursorTupleError::Json(error.to_string()))
    }

    /// Borrow the ordered canonical scalar values.
    #[must_use]
    pub fn values(&self) -> &[IrScalar] {
        &self.0
    }

    /// Consume the tuple into its ordered canonical scalar values.
    #[must_use]
    pub fn into_values(self) -> Vec<IrScalar> {
        self.0
    }
}

fn canonical_cursor_scalar(
    value: IrScalar,
    expected: CursorScalarType,
) -> Result<IrScalar, &'static str> {
    match (expected, value) {
        (CursorScalarType::Int64, IrScalar::Int(value) | IrScalar::Int64(value)) => {
            Ok(IrScalar::Int64(value))
        }
        (CursorScalarType::Decimal, IrScalar::Decimal(value)) => Ok(IrScalar::Decimal(value)),
        (CursorScalarType::String, IrScalar::Str(value)) => Ok(IrScalar::Str(value)),
        (_, value) => Err(cursor_scalar_kind(&value)),
    }
}

fn cursor_scalar_kind(value: &IrScalar) -> &'static str {
    match value {
        IrScalar::Null => "null",
        IrScalar::Bool(_) => "bool",
        IrScalar::Int(_) => "int",
        IrScalar::Int64(_) => "int64",
        IrScalar::Decimal(_) => "decimal",
        IrScalar::Str(_) => "string",
        IrScalar::Bytes(_) => "bytes",
    }
}

/// A malformed typed cursor checkpoint or mismatched resume contract.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CursorTupleError {
    /// The stored cursor-column tuple changed.
    #[error("cursor columns changed: expected {expected:?}, found {actual:?}")]
    Columns {
        /// Authored tuple.
        expected: Vec<String>,
        /// Stored/proven tuple.
        actual: Vec<String>,
    },
    /// The scalar array arity differs from the cursor tuple.
    #[error("cursor tuple has arity {actual}; expected {expected}")]
    Arity {
        /// Contract arity.
        expected: usize,
        /// Checkpoint arity.
        actual: usize,
    },
    /// A scalar's tagged wire family differs from its column contract.
    #[error("cursor tuple position {index} has scalar type {actual}; expected {expected:?}")]
    ScalarType {
        /// Zero-based tuple position.
        index: usize,
        /// Planner-proven scalar family.
        expected: CursorScalarType,
        /// Actual tagged family.
        actual: &'static str,
    },
    /// Malformed tagged-scalar JSON.
    #[error("invalid cursor tuple JSON: {0}")]
    Json(String),
}

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
/// key to page by ([`cursor_columns`](Self::cursor_columns)), the
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
    /// The ordered unique key tuple to page by.
    pub cursor_columns: Vec<String>,
    /// How cursor immutability is enforced for the operation's whole lifetime.
    pub cursor_stability: CursorStability,
    /// Target comparison/type facts proven by live-schema planning. Offline
    /// structural previews may leave this absent; executable live plans populate
    /// it and apply revalidates it before every cohort/batch transition.
    pub cursor_contract: Option<CursorContract>,
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
            self.set_clause.as_str(),
            self.filter.as_deref().unwrap_or("\u{0}<none>"),
        ] {
            h.update((field.len() as u64).to_be_bytes());
            h.update(field.as_bytes());
        }
        h.update(b"\0cursorColumns/v1");
        h.update((self.cursor_columns.len() as u64).to_be_bytes());
        for column in &self.cursor_columns {
            h.update((column.len() as u64).to_be_bytes());
            h.update(column.as_bytes());
        }
        h.update(b"\0cursorStability/v1");
        match &self.cursor_stability {
            CursorStability::GuardUpdates => h.update(b"guardUpdates"),
            CursorStability::ExternalInvariant { name } => {
                h.update(b"externalInvariant");
                h.update((name.len() as u64).to_be_bytes());
                h.update(name.as_bytes());
            }
        }
        // Keep generator-bearing identities self-delimiting from every ordinary
        // transform and from the new cursor/stability domains above.
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
            cursor_columns: vec!["id".into()],
            cursor_stability: CursorStability::GuardUpdates,
            cursor_contract: None,
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

    #[test]
    fn cursor_tuple_round_trips_tagged_scalars_without_delimiters() {
        let contract = CursorContract {
            columns: vec![
                CursorColumnContract {
                    name: "tenant_id".into(),
                    scalar_type: CursorScalarType::Int64,
                    database_type: "bigint".into(),
                    comparison: CursorComparison::Default,
                },
                CursorColumnContract {
                    name: "amount".into(),
                    scalar_type: CursorScalarType::Decimal,
                    database_type: "numeric(38, 9)".into(),
                    comparison: CursorComparison::Default,
                },
                CursorColumnContract {
                    name: "external_id".into(),
                    scalar_type: CursorScalarType::String,
                    database_type: "text".into(),
                    comparison: CursorComparison::NamedCollation {
                        schema: Some("pg_catalog".into()),
                        name: "C".into(),
                    },
                },
            ],
        };
        let tuple = CursorTuple::new(
            vec![
                IrScalar::Int(42),
                IrScalar::Decimal("-12345678901234567890.000000001".into()),
                IrScalar::Str("embedded\0|,delimiter".into()),
            ],
            &contract,
        )
        .expect("valid typed tuple");

        let encoded = tuple.to_json().expect("encode tuple");
        assert_eq!(
            encoded,
            r#"[{"int64":"42"},{"decimal":"-12345678901234567890.000000001"},"embedded\u0000|,delimiter"]"#
        );
        assert_eq!(
            CursorTuple::from_json(&encoded, &contract).expect("decode tuple"),
            tuple
        );
    }

    #[test]
    fn cursor_tuple_rejects_arity_and_scalar_type_drift() {
        let contract = CursorContract {
            columns: vec![CursorColumnContract {
                name: "id".into(),
                scalar_type: CursorScalarType::Int64,
                database_type: "bigint".into(),
                comparison: CursorComparison::Default,
            }],
        };

        assert_eq!(
            CursorTuple::new(Vec::new(), &contract),
            Err(CursorTupleError::Arity {
                expected: 1,
                actual: 0,
            })
        );
        assert_eq!(
            CursorTuple::new(vec![IrScalar::Str("1".into())], &contract),
            Err(CursorTupleError::ScalarType {
                index: 0,
                expected: CursorScalarType::Int64,
                actual: "string",
            })
        );

        assert_eq!(
            CursorTuple::from_json("[1]", &contract),
            Err(CursorTupleError::ScalarType {
                index: 0,
                expected: CursorScalarType::Int64,
                actual: "int",
            }),
            "resume must reject an untagged integer instead of normalizing a legacy checkpoint"
        );
    }
}
