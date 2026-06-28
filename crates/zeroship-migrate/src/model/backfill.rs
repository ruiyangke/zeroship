//! Pure data for large-table backfill plan steps.

use sha2::{Digest, Sha256};

/// The structured definition of a large-table backfill (design §5).
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
        h.update(self.batch_size.to_be_bytes());
        format!("bf_{}", hex::encode(&h.finalize()[..16]))
    }
}
