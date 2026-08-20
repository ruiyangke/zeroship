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
    #[track_caller]
    pub fn get<'a, I, T>(&'a self, idx: I) -> T
    where
        I: RowIndex + fmt::Display,
        T: FromSql<'a>,
    {
        match self.get_inner(&idx) {
            Ok(ok) => ok,
            Err(err) => panic!("error retrieving column {}: {}", idx, err),
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
        if !T::accepts(ty) {
            return Err(Error::from_sql(
                Box::new(WrongType::new::<T>(ty.clone())),
                idx,
            ));
        }

        FromSql::from_sql_nullable(ty, self.col_buffer(idx)).map_err(|e| Error::from_sql(e, idx))
    }

    /// Returns the raw size of the row in bytes.
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
    /// Returns `None` if the value is SQL NULL or the column doesn't exist.
    /// Values are in PostgreSQL's binary wire format — the shape depends on
    /// the column's OID. Intended for callers that want to bypass the
    /// `FromSql` trait and do custom decoding (for example, re-encoding
    /// `TIMESTAMPTZ` as JS Unix milliseconds).
    pub fn raw_value<I>(&self, idx: I) -> Option<&[u8]>
    where
        I: RowIndex + fmt::Display,
    {
        let idx = idx.__idx(self.columns())?;
        self.col_buffer(idx)
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
    #[track_caller]
    pub fn get<I>(&self, idx: I) -> Option<&str>
    where
        I: RowIndex + fmt::Display,
    {
        match self.get_inner(&idx) {
            Ok(ok) => ok,
            Err(err) => panic!("error retrieving column {}: {}", idx, err),
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
                    row.raw_value(2).is_none(),
                    "raw_value on a column the row does not carry must be None"
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
        assert_eq!(row.raw_value(0), Some(&7i32.to_be_bytes()[..]));
        assert!(row.try_get::<_, i32>("nope").is_err());
        assert!(row.try_get::<_, i32>(3).is_err());
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
