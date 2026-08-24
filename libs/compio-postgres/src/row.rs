// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

//! Rows.

use crate::row::sealed::{AsName, Sealed};
use crate::simple_query::SimpleColumn;
use crate::statement::Column;
use crate::types::{FromSql, Type, WrongType};
use crate::{Error, Statement};
use fallible_iterator::FallibleIterator;
use postgres_protocol::message::backend::DataRowBody;
use std::fmt;
use std::io;
use std::ops::Range;
use std::str;
use std::sync::Arc;

mod sealed {
    pub trait Sealed {}

    pub trait AsName {
        fn as_name(&self) -> &str;
    }
}

impl AsName for Column {
    fn as_name(&self) -> &str {
        self.name()
    }
}

impl AsName for String {
    fn as_name(&self) -> &str {
        self
    }
}

/// A trait implemented by types that can index into columns of a row.
///
/// This cannot be implemented outside of this crate.
pub trait RowIndex: Sealed {
    #[doc(hidden)]
    fn __idx<T>(&self, columns: &[T]) -> Option<usize>
    where
        T: AsName;
}

impl Sealed for usize {}

impl RowIndex for usize {
    #[inline]
    fn __idx<T>(&self, columns: &[T]) -> Option<usize>
    where
        T: AsName,
    {
        if *self >= columns.len() {
            None
        } else {
            Some(*self)
        }
    }
}

impl Sealed for str {}

impl RowIndex for str {
    #[inline]
    fn __idx<T>(&self, columns: &[T]) -> Option<usize>
    where
        T: AsName,
    {
        if let Some(idx) = columns.iter().position(|d| d.as_name() == self) {
            return Some(idx);
        };

        // FIXME ASCII-only case insensitivity isn't really the right thing to
        // do. Postgres itself uses a dubious wrapper around tolower and JDBC
        // uses the US locale.
        columns
            .iter()
            .position(|d| d.as_name().eq_ignore_ascii_case(self))
    }
}

impl<T> Sealed for &T where T: ?Sized + Sealed {}

impl<T> RowIndex for &T
where
    T: ?Sized + RowIndex,
{
    #[inline]
    fn __idx<U>(&self, columns: &[U]) -> Option<usize>
    where
        U: AsName,
    {
        T::__idx(*self, columns)
    }
}

/// Render an accessor failure and every `source()` beneath it, joined by
/// `": "`.
///
/// `Error`'s own `Display` prints its KIND and nothing more, and for the two
/// failures these accessors can hit that is either a restatement of the caller
/// context (`Kind::FromSql` prints `error deserializing column 3`) or the whole
/// story already (`Kind::Column` prints `invalid column \`x\``). Everything
/// that says WHY a decode failed - `unexpected null`, or `cannot convert
/// between the Rust type ... and the Postgres type ...` - lives in the cause.
///
/// A panic message is read once, by a person, with no `Debug` formatting and
/// no chance to inspect `source()`. Printing the outer error alone made a NULL
/// read into a non-`Option` and a wrong-`FromSql`-type read produce the
/// identical sentence.
fn render_with_causes(error: &Error) -> String {
    let mut rendered = error.to_string();
    let mut link = std::error::Error::source(error);
    while let Some(cause) = link {
        rendered.push_str(": ");
        rendered.push_str(&cause.to_string());
        link = cause.source();
    }
    rendered
}

/// A row of data returned from the database by a query.
#[derive(Clone)]
pub struct Row {
    statement: Statement,
    body: DataRowBody,
    ranges: Vec<Option<Range<usize>>>,
}

impl fmt::Debug for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Row")
            .field("columns", &self.columns())
            .finish()
    }
}

impl Row {
    pub(crate) fn new(statement: Statement, body: DataRowBody) -> Result<Row, Error> {
        let ranges: Vec<Option<Range<usize>>> = body.ranges().collect().map_err(Error::parse)?;
        // The accessors index `ranges` with an index bounds-checked against
        // the COLUMNS, so the two lists have to agree. The protocol says they
        // do - a `DataRow` carries exactly as many fields as the
        // `RowDescription` that preceded it declared columns - and a peer that
        // breaks that is malformed, not merely surprising.
        //
        // Checked here rather than defended against in each accessor: a row
        // whose arity disagrees is not partially usable, and letting it exist
        // meant `try_get`, whose whole contract is returning a `Result` rather
        // than panicking, panicked with an out-of-bounds index.
        if ranges.len() != statement.columns().len() {
            return Err(Error::parse(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "DataRow carries {} fields but its RowDescription declared {} columns",
                    ranges.len(),
                    statement.columns().len(),
                ),
            )));
        }
        Ok(Row {
            statement,
            body,
            ranges,
        })
    }

    /// Returns information about the columns of data in the row.
    pub fn columns(&self) -> &[Column] {
        self.statement.columns()
    }

    /// Determines if the row contains no values.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the number of values in the row.
    pub fn len(&self) -> usize {
        self.columns().len()
    }

    /// Deserializes a value from the row.
    ///
    /// The value can be specified either by its numeric index in the row, or by its column name.
    ///
    /// # Panics
    ///
    /// Panics if the index is out of bounds or if the value cannot be converted to the specified type.
    ///
    /// The panic names the column AND the reason, down the whole cause chain -
    /// see [`render_with_causes`] for why the reason is not in the outer error.
    #[track_caller]
    pub fn get<'a, I, T>(&'a self, idx: I) -> T
    where
        I: RowIndex + fmt::Display,
        T: FromSql<'a>,
    {
        match self.get_inner(&idx) {
            Ok(ok) => ok,
            Err(err) => panic!(
                "error retrieving column {}: {}",
                idx,
                render_with_causes(&err)
            ),
        }
    }

    /// Like `Row::get`, but returns a `Result` rather than panicking.
    pub fn try_get<'a, I, T>(&'a self, idx: I) -> Result<T, Error>
    where
        I: RowIndex + fmt::Display,
        T: FromSql<'a>,
    {
        self.get_inner(&idx)
    }

    fn get_inner<'a, I, T>(&'a self, idx: &I) -> Result<T, Error>
    where
        I: RowIndex + fmt::Display,
        T: FromSql<'a>,
    {
        let idx = match idx.__idx(self.columns()) {
            Some(idx) => idx,
            None => return Err(Error::column(idx.to_string())),
        };

        let ty = self.columns()[idx].type_();
        // A domain decodes as its base, exactly as it BINDS as its base. The
        // two paths disagreed until now: `crate::query::encode_parameter`
        // unwrapped, this did not, so a `d[]` column could be written and then
        // not read back. `underlying_base_type` is the one definition both use.
        //
        // The reduction is only consulted when the declared type is refused, so
        // no ordinary column changes what it decodes as.
        let reduced = if T::accepts(ty) {
            None
        } else {
            crate::query::underlying_base_type(ty).filter(|base| T::accepts(base))
        };
        let ty = match &reduced {
            Some(base) => base,
            None if T::accepts(ty) => ty,
            None => {
                return Err(Error::from_sql(
                    Box::new(WrongType::new::<T>(ty.clone())),
                    idx,
                ));
            }
        };

        FromSql::from_sql_nullable(ty, self.col_buffer(idx)).map_err(|e| Error::from_sql(e, idx))
    }

    /// The row's length-prefixed FIELD DATA, in bytes: `sum(4 + field_len)`
    /// over the columns, counting 4 bytes for a SQL NULL (its length field,
    /// which carries -1, with no payload).
    ///
    /// IT IS NOT THE ROW'S SIZE ON THE WIRE, and the difference is a fixed
    /// 7 bytes per row that this does not count: the `DataRow` tag (1), the
    /// message length (4) and the field count (2). Measured -- `SELECT
    /// 'x'::text` reports 5 against a 12-byte frame, so a caller metering
    /// ingress bandwidth from this alone undercounts a small row by more than
    /// half. Add 7 per row for wire bytes.
    ///
    /// The old doc said "the raw size of the row in bytes", which invites
    /// exactly that reading. The number was always this one; only the
    /// description was ambiguous.
    pub fn raw_size_bytes(&self) -> usize {
        self.body.buffer_bytes().len()
    }

    /// Get the raw bytes for the column at the given index.
    ///
    /// `idx` is bounds-checked against the columns by `RowIndex::__idx`, and
    /// `Row::new` refuses any row whose field count differs from that column
    /// count, so indexing `ranges` here cannot go out of bounds.
    fn col_buffer(&self, idx: usize) -> Option<&[u8]> {
        let range = self.ranges[idx].to_owned()?;
        Some(&self.body.buffer()[range])
    }

    /// Raw wire bytes for the column identified by `idx` (name or index).
    ///
    /// `Ok(None)` is a column that IS there and holds SQL NULL. `Err` is a
    /// name or index this row does not carry - [`Error::column`], naming it,
    /// the same refusal [`Row::try_get`] gives.
    ///
    /// Values are in PostgreSQL's binary wire format — the shape depends on
    /// the column's OID. Intended for callers that want to bypass the
    /// `FromSql` trait and do custom decoding (for example, re-encoding
    /// `TIMESTAMPTZ` as JS Unix milliseconds).
    ///
    /// # Errors
    ///
    /// [`Error::column`] when `idx` names no column of this row, or is an
    /// index past its last one. A SQL NULL is never an error.
    ///
    /// This returned a bare `Option`, folding both answers into `None`. They
    /// are different facts and only one is the caller's mistake: a caller
    /// reading BY NAME saw a renamed or mistyped column as a legitimate SQL
    /// NULL and decoded a default from it. `plugin-db`'s audit reader did
    /// exactly that, reporting a backfill whose `details` column it had failed
    /// to find as one that had processed zero rows.
    pub fn raw_value<I>(&self, idx: I) -> Result<Option<&[u8]>, Error>
    where
        I: RowIndex + fmt::Display,
    {
        match idx.__idx(self.columns()) {
            Some(resolved) => Ok(self.col_buffer(resolved)),
            None => Err(Error::column(idx.to_string())),
        }
    }
}

impl AsName for SimpleColumn {
    fn as_name(&self) -> &str {
        self.name()
    }
}

/// A row of data returned from the database by a simple query.
#[derive(Debug)]
pub struct SimpleQueryRow {
    columns: Arc<[SimpleColumn]>,
    body: DataRowBody,
    ranges: Vec<Option<Range<usize>>>,
}

impl SimpleQueryRow {
    #[allow(clippy::new_ret_no_self)]
    pub(crate) fn new(
        columns: Arc<[SimpleColumn]>,
        body: DataRowBody,
    ) -> Result<SimpleQueryRow, Error> {
        let ranges: Vec<Option<Range<usize>>> = body.ranges().collect().map_err(Error::parse)?;
        // Same reconciliation as `Row::new`, for the same reason: `get_inner`
        // indexes `ranges` with an index bounds-checked against the columns.
        if ranges.len() != columns.len() {
            return Err(Error::parse(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "DataRow carries {} fields but its RowDescription declared {} columns",
                    ranges.len(),
                    columns.len(),
                ),
            )));
        }
        Ok(SimpleQueryRow {
            columns,
            body,
            ranges,
        })
    }

    /// Returns information about the columns of data in the row.
    pub fn columns(&self) -> &[SimpleColumn] {
        &self.columns
    }

    /// Determines if the row contains no values.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the number of values in the row.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Returns a value from the row.
    ///
    /// The value can be specified either by its numeric index in the row, or by its column name.
    ///
    /// # Panics
    ///
    /// Panics if the index is out of bounds or if the value cannot be converted to the specified type.
    ///
    /// The panic names the column AND the reason, down the whole cause chain -
    /// see [`render_with_causes`] for why the reason is not in the outer error.
    #[track_caller]
    pub fn get<I>(&self, idx: I) -> Option<&str>
    where
        I: RowIndex + fmt::Display,
    {
        match self.get_inner(&idx) {
            Ok(ok) => ok,
            Err(err) => panic!(
                "error retrieving column {}: {}",
                idx,
                render_with_causes(&err)
            ),
        }
    }

    /// Like `SimpleQueryRow::get`, but returns a `Result` rather than panicking.
    pub fn try_get<I>(&self, idx: I) -> Result<Option<&str>, Error>
    where
        I: RowIndex + fmt::Display,
    {
        self.get_inner(&idx)
    }

    fn get_inner<I>(&self, idx: &I) -> Result<Option<&str>, Error>
    where
        I: RowIndex + fmt::Display,
    {
        let idx = match idx.__idx(&self.columns) {
            Some(idx) => idx,
            None => return Err(Error::column(idx.to_string())),
        };

        let buf = self.ranges[idx].clone().map(|r| &self.body.buffer()[r]);
        FromSql::from_sql_nullable(&Type::TEXT, buf).map_err(|e| Error::from_sql(e, idx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simple_query::SimpleColumn;
    use bytes::BytesMut;
    use postgres_protocol::message::backend::Message;

    /// Build a `DataRowBody` carrying exactly `values` fields.
    ///
    /// Unlike `test_utils::row_for_test` this does NOT insist the field count
    /// match any column list - producing a row whose arity disagrees with its
    /// `RowDescription` is the entire point.
    fn data_row(values: &[Option<&[u8]>]) -> DataRowBody {
        let mut payload = BytesMut::new();
        payload.extend_from_slice(&u16::try_from(values.len()).unwrap().to_be_bytes());
        for v in values {
            match v {
                None => payload.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(bytes) => {
                    payload.extend_from_slice(&i32::try_from(bytes.len()).unwrap().to_be_bytes());
                    payload.extend_from_slice(bytes);
                }
            }
        }

        let mut wire = BytesMut::new();
        wire.extend_from_slice(b"D");
        wire.extend_from_slice(&u32::try_from(payload.len() + 4).unwrap().to_be_bytes());
        wire.extend_from_slice(&payload);

        match Message::parse(&mut wire).expect("synthetic DataRow parses") {
            Some(Message::DataRow(body)) => body,
            _ => panic!("synthetic DataRow did not parse as a DataRow"),
        }
    }

    fn int4_columns(names: &[&str]) -> Vec<Column> {
        names
            .iter()
            .map(|name| Column {
                name: (*name).to_string(),
                table_oid: None,
                column_id: None,
                type_modifier: -1,
                r#type: Type::INT4,
            })
            .collect()
    }

    /// A `DataRow` carrying fewer fields than its `RowDescription` declared
    /// columns must never panic a caller who asked for a `Result`.
    ///
    /// The row's field ranges come off the DataRow; the index bound comes off
    /// the statement's column list. Nothing reconciled the two, so `try_get`
    /// on a column the row does not carry indexed `self.ranges` out of bounds
    /// and panicked - from the method whose entire contract is "like `get`,
    /// but returns a `Result` rather than panicking".
    ///
    /// Either answer is acceptable and the assertion allows both: refuse the
    /// row when it is built, or return an error from the accessor. What is not
    /// acceptable is a panic, so this fails on an unfixed driver whichever
    /// shape the fix takes.
    #[test]
    fn a_short_data_row_never_panics_a_try_get() {
        let columns = int4_columns(&["a", "b", "c"]);
        let statement = Statement::unnamed(Vec::new(), columns);
        let body = data_row(&[Some(&1i32.to_be_bytes())]);

        match Row::new(statement, body) {
            Err(_) => {}
            Ok(row) => {
                assert!(
                    row.try_get::<_, i32>(2).is_err(),
                    "try_get on a column the row does not carry must be an error"
                );
                assert!(
                    row.raw_value(2).is_err(),
                    "raw_value on a column the row does not carry must be an error"
                );
            }
        }
    }

    /// The same for the simple-query row, which builds its ranges the same way
    /// off a `RowDescription` it does not check against.
    #[test]
    fn a_short_simple_query_row_never_panics_a_try_get() {
        let columns: Arc<[SimpleColumn]> = vec![
            SimpleColumn::new("a".to_string()),
            SimpleColumn::new("b".to_string()),
        ]
        .into();
        let body = data_row(&[Some(b"1")]);

        match SimpleQueryRow::new(columns, body) {
            Err(_) => {}
            Ok(row) => {
                assert!(
                    row.try_get(1).is_err(),
                    "try_get on a column the row does not carry must be an error"
                );
            }
        }
    }

    /// The control: a row whose arity matches its columns still reads every
    /// value, including SQL NULL, and still reports a missing NAME as a column
    /// error rather than refusing the row.
    ///
    /// A fix that rejects rows too eagerly - anything keyed on "are there
    /// NULLs", or an off-by-one in the arity comparison - turns this red.
    #[test]
    fn a_row_matching_its_columns_reads_every_value() {
        let columns = int4_columns(&["a", "b", "c"]);
        let statement = Statement::unnamed(Vec::new(), columns);
        let body = data_row(&[Some(&7i32.to_be_bytes()), None, Some(&9i32.to_be_bytes())]);

        let row = Row::new(statement, body).expect("a row matching its columns is well formed");
        assert_eq!(row.try_get::<_, i32>(0).unwrap(), 7);
        assert_eq!(row.try_get::<_, Option<i32>>(1).unwrap(), None);
        assert_eq!(row.try_get::<_, i32>("c").unwrap(), 9);
        assert_eq!(row.raw_value(0).unwrap(), Some(&7i32.to_be_bytes()[..]));
        assert!(row.try_get::<_, i32>("nope").is_err());
        assert!(row.try_get::<_, i32>(3).is_err());
    }

    /// `raw_value` must answer a column that is NOT THERE differently from one
    /// that is there and holds SQL NULL.
    ///
    /// It answered `None` to both, so a caller reading by NAME - `plugin-db`'s
    /// audit reader, among others - decoded a renamed or mistyped column into
    /// whatever default it had chosen for NULL, and no layer above could see
    /// that it had asked for a column the query never returned.
    ///
    /// The DB-free twin of `tests/raw_value_column_identity.rs`: this pins the
    /// accessor, that one pins it against what a real `RowDescription` and
    /// `DataRow` produce.
    #[test]
    fn raw_value_separates_a_missing_column_from_a_sql_null() {
        let statement = Statement::unnamed(Vec::new(), int4_columns(&["a", "b"]));
        let body = data_row(&[Some(&7i32.to_be_bytes()), None]);
        let row = Row::new(statement, body).expect("a row matching its columns is well formed");

        // The column is there and holds SQL NULL: not an error.
        assert_eq!(row.raw_value("b").unwrap(), None);
        assert_eq!(row.raw_value(1).unwrap(), None);

        // The column is not there: an error that names it.
        let by_name = row.raw_value("nope").expect_err("`nope` is not a column");
        assert_eq!(by_name.to_string(), "invalid column `nope`");
        let by_index = row.raw_value(2).expect_err("index 2 is out of range");
        assert_eq!(by_index.to_string(), "invalid column `2`");

        // The control: a present, non-null column still yields its bytes, so
        // "missing columns error" cannot be met by erroring on everything.
        assert_eq!(row.raw_value("a").unwrap(), Some(&7i32.to_be_bytes()[..]));
    }

    /// Run `f`, and return the message of the panic it raised.
    fn panic_message(f: impl FnOnce()) -> String {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
            .err()
            .expect("the operation was expected to panic");
        std::panic::set_hook(previous);
        payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&'static str>().copied())
            .unwrap_or("<non-string panic payload>")
            .to_string()
    }

    /// The panic `Row::get` raises has to say WHY, and the two ways a decode
    /// fails are not the same mistake.
    ///
    /// `Error`'s `Display` renders its KIND and nothing else - `Kind::FromSql`
    /// prints `error deserializing column N` - and everything that identifies
    /// the failure hangs off `source()`. Formatting it with `{}` therefore
    /// turned both of these into the same contentless sentence, `error
    /// retrieving column 0: error deserializing column 0`, which restates the
    /// prefix and drops the cause.
    ///
    /// Both readings below name column 0 of an int4 column, so the index cannot
    /// be what tells them apart. What must is the cause: `unexpected null` for
    /// one and the two type names for the other.
    #[test]
    fn a_get_panic_distinguishes_a_null_from_a_type_mismatch() {
        let statement = Statement::unnamed(Vec::new(), int4_columns(&["a"]));
        let null_row = Row::new(statement.clone(), data_row(&[None])).expect("well formed");
        let value_row =
            Row::new(statement, data_row(&[Some(&1i32.to_be_bytes())])).expect("well formed");

        let on_null = panic_message(|| {
            null_row.get::<_, i32>(0);
        });
        let on_wrong_type = panic_message(|| {
            value_row.get::<_, String>(0);
        });

        assert_ne!(
            on_null, on_wrong_type,
            "reading a NULL into a non-Option and reading an int4 into a String \
             are different mistakes and must not produce the same message"
        );
        assert!(
            on_null.to_ascii_lowercase().contains("null"),
            "the NULL panic does not say the value was null: {on_null}"
        );
        assert!(
            on_wrong_type.contains("int4") && on_wrong_type.contains("String"),
            "the type-mismatch panic names neither type it could not convert \
             between: {on_wrong_type}"
        );
    }

    /// The control, differing in ONE variable: this failure HAS NO CAUSE.
    ///
    /// `Kind::Column` carries the whole story in its own `Display`, so walking
    /// the chain must add nothing - no trailing separator, no repetition. A fix
    /// that appends unconditionally turns this red while the claim above stays
    /// green.
    #[test]
    fn a_get_panic_for_a_missing_column_gains_nothing() {
        let statement = Statement::unnamed(Vec::new(), int4_columns(&["a"]));
        let row = Row::new(statement, data_row(&[Some(&1i32.to_be_bytes())])).expect("well formed");

        assert_eq!(
            panic_message(|| {
                row.get::<_, i32>("nope");
            }),
            "error retrieving column nope: invalid column `nope`"
        );
        assert_eq!(
            panic_message(|| {
                row.get::<_, i32>(4);
            }),
            "error retrieving column 4: invalid column `4`"
        );
    }

    /// The simple-query accessor shares the shape and so shares the defect: its
    /// only decode failure is a value that is not UTF-8, and `Kind::FromSql`
    /// renders that as `error deserializing column 0` too.
    #[test]
    fn a_simple_query_get_panic_names_the_decode_failure() {
        let columns: Arc<[SimpleColumn]> = vec![SimpleColumn::new("a".to_string())].into();
        let row = SimpleQueryRow::new(columns, data_row(&[Some(b"\xff")])).expect("well formed");

        let message = panic_message(|| {
            row.get(0);
        });
        assert!(
            message.contains("utf-8") || message.contains("utf8"),
            "the panic does not say the value was not valid UTF-8: {message}"
        );
    }

    /// The control for the simple-query row.
    #[test]
    fn a_simple_query_row_matching_its_columns_reads_every_value() {
        let columns: Arc<[SimpleColumn]> = vec![
            SimpleColumn::new("a".to_string()),
            SimpleColumn::new("b".to_string()),
        ]
        .into();
        let body = data_row(&[Some(b"hello"), None]);

        let row = SimpleQueryRow::new(columns, body)
            .expect("a row matching its columns is well formed");
        assert_eq!(row.try_get(0).unwrap(), Some("hello"));
        assert_eq!(row.try_get("b").unwrap(), None);
        assert!(row.try_get("nope").is_err());
    }
}
