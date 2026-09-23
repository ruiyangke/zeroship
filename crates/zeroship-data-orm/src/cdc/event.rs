//! Database change-event vocabulary shared by producers, sinks, and consumers.

use std::collections::HashMap;

use crate::binding::DbRoute;

/// One committed row change flowing through the data-plane broker.
///
/// Mutation callbacks and database change streams produce the same shape so
/// downstream delivery does not depend on the originating backend.
#[derive(Debug, Clone)]
pub struct ChangeEvent {
    /// Tenant AND database the change was committed on. The routing table keys
    /// on this together with [`Self::collection`].
    ///
    /// **The database half is load-bearing and its omission is silent.** One
    /// app may reach two databases and both may declare a collection of the
    /// same name, so an event carrying only the tenant would match a
    /// subscription on the other database and nothing would raise.
    pub route: DbRoute,
    /// Collection (table inside the app database).
    pub collection: String,
    /// Operation kind: `"insert"`, `"update"`, or `"delete"`.
    pub op: ChangeOp,
    /// Logical row id of the affected row, if known. Serialized as text so
    /// string and numeric identities share one wire shape.
    pub pk: Option<String>,
    /// Columns the mutation touched. Inserts report every declared column,
    /// updates report the SET-side, and deletes report an empty list.
    pub changed_columns: Vec<String>,
    /// Text-encoded post-image values, or the pre-image for deletes. This may
    /// be empty when the producer cannot obtain a tuple snapshot; consumers
    /// treat a missing predicate column as non-matching.
    pub new_tuple: HashMap<String, String>,
    /// Optional pre-image for updates. A subscriber may match either image so
    /// it observes a row entering or leaving its view. Inserts have no old
    /// image; deletes carry their pre-image in [`Self::new_tuple`].
    pub old_tuple: Option<HashMap<String, String>>,
}

/// Kind of row change carried by [`ChangeEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOp {
    Insert,
    Update,
    Delete,
}

impl ChangeOp {
    /// Stable lowercase operation name used by delivery envelopes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}
