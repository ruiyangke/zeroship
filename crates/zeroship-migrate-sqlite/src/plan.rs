use zeroship_migrate_backend::table_rebuild::SequenceHighWaterPolicy;

/// How a SQLite table rebuild handles the table's `sqlite_sequence` row.
///
/// Rebuilds preserve an existing `AUTOINCREMENT` high-water mark by default. The
/// only caller that should request [`SqliteSequencePolicy::Remove`] is a
/// structured operation which has explicitly validated and declared removal of
/// the table's `AUTOINCREMENT` identity facet. The removal happens inside the
/// rebuild transaction, so an aborted rebuild restores the original sequence row
/// together with the original table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SqliteSequencePolicy {
    /// Capture and monotonically restore the pre-rebuild high-water mark.
    #[default]
    Preserve,
    /// Do not restore the old high-water mark and delete any row for the rebuilt
    /// table. This is the explicit identity-removal transition.
    Remove,
}

/// The neutral rebuild spec says WHICH transition; this vendor says what that
/// means for `sqlite_sequence`.
///
/// `TableRebuildSpec::sequence_policy` used to be this very type, which pointed the
/// dependency the wrong way: the backend CONTRACT, which every vendor sits above,
/// would have had to name this crate. It carries
/// [`SequenceHighWaterPolicy`] now, and the translation happens here - at the
/// boundary of the backend that owns the behaviour - rather than in a plan
/// carrier every dialect shares.
impl From<SequenceHighWaterPolicy> for SqliteSequencePolicy {
    fn from(policy: SequenceHighWaterPolicy) -> Self {
        match policy {
            SequenceHighWaterPolicy::Preserve => Self::Preserve,
            SequenceHighWaterPolicy::Reset => Self::Remove,
        }
    }
}
