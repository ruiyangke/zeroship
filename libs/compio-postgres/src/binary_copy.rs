// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

//! Utilities for working with the PostgreSQL binary copy format.

use crate::types::{FromSql, IsNull, ToSql, Type, WrongType};
use crate::{CopyInSink, CopyOutStream, Error, slice_iter};
use byteorder::{BigEndian, ByteOrder};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures_util::{SinkExt, Stream};
use pin_project_lite::pin_project;
use postgres_types::BorrowToSql;
use std::io;
use std::io::Cursor;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

const MAGIC: &[u8] = b"PGCOPY\n\xff\r\n\0";
const HEADER_LEN: usize = MAGIC.len() + 4 + 4;

pin_project! {
    /// A type which serializes rows into the PostgreSQL binary copy format.
    ///
    /// The copy *must* be explicitly completed via the `finish` method. If it is not, the copy will be aborted.
    pub struct BinaryCopyInWriter {
        #[pin]
        sink: CopyInSink<Bytes>,
        types: Vec<Type>,
        buf: BytesMut,
    }
}

impl BinaryCopyInWriter {
    /// Creates a new writer which will write rows of the provided types to the provided sink.
    pub fn new(sink: CopyInSink<Bytes>, types: &[Type]) -> BinaryCopyInWriter {
        let mut buf = BytesMut::new();
        buf.put_slice(MAGIC);
        buf.put_i32(0); // flags
        buf.put_i32(0); // header extension

        BinaryCopyInWriter {
            sink,
            types: types.to_vec(),
            buf,
        }
    }

    /// Writes a single row.
    ///
    /// # Panics
    ///
    /// Panics if the number of values provided does not match the number expected.
    pub async fn write(self: Pin<&mut Self>, values: &[&(dyn ToSql + Sync)]) -> Result<(), Error> {
        self.write_raw(slice_iter(values)).await
    }

    /// A maximally-flexible version of `write`.
    ///
    /// # Panics
    ///
    /// Panics if the number of values provided does not match the number expected.
    pub async fn write_raw<P, I>(self: Pin<&mut Self>, values: I) -> Result<(), Error>
    where
        P: BorrowToSql,
        I: IntoIterator<Item = P>,
        I::IntoIter: ExactSizeIterator,
    {
        let mut this = self.project();

        let values = values.into_iter();
        assert!(
            values.len() == this.types.len(),
            "expected {} values but got {}",
            this.types.len(),
            values.len(),
        );

        // A row is ATOMIC in this buffer. `encode_row` writes the tuple header
        // and then hands the same buffer to each value's `to_sql`, so a value
        // that is refused -- on `accepts`, or partway through its own encoding
        // -- leaves the field count, a length placeholder still reading zero,
        // and whatever it managed to append. Nothing downstream can tell that
        // stub from a row: `[count][len=0]` is a legal empty value PostgreSQL
        // INSERTS, and stray value bytes are read as the next tuple's field
        // count. Rewinding to the row boundary is what makes a `write_raw`
        // that returned `Err` mean "this row did not happen", which is the
        // only thing a caller holding a per-row error can act on.
        let checkpoint = this.buf.len();
        if let Err(error) = encode_row(this.buf, this.types.as_slice(), values) {
            this.buf.truncate(checkpoint);
            return Err(error);
        }

        if this.buf.len() > 4096 {
            this.sink.send(this.buf.split().freeze()).await?;
        }

        Ok(())
    }

    /// Completes the copy, returning the number of rows added.
    ///
    /// This method *must* be used to complete the copy process. If it is not, the copy will be aborted.
    pub async fn finish(self: Pin<&mut Self>) -> Result<u64, Error> {
        let mut this = self.project();

        this.buf.put_i16(-1);
        this.sink.send(this.buf.split().freeze()).await?;
        this.sink.finish().await
    }
}

/// Append one tuple -- field count, then a four-byte length and its payload per
/// value -- to `buf`.
///
/// Separated from `write_raw` so every early return is a single `?` inside one
/// function whose failures the caller undoes wholesale. Inline, each `?` was a
/// separate exit leaving a different amount of the row behind.
fn encode_row<P, I>(buf: &mut BytesMut, types: &[Type], values: I) -> Result<(), Error>
where
    P: BorrowToSql,
    I: Iterator<Item = P>,
{
    // REFUSED, not cast. The column types come from the CALLER
    // (`BinaryCopyInWriter::new` takes them as a slice), so this length is
    // caller-controlled, and `as i16` wraps SILENTLY - a cast does not panic on
    // overflow the way arithmetic does, so 32768 columns became -32768 on the
    // wire in every build. A negative field count desynchronises the COPY
    // stream, which costs the connection rather than just the row.
    //
    // `tuple_field_count` already refuses the same shape on the READ side,
    // widening to `i32` so a hostile `i16::MAX` cannot overflow. This is that
    // rule applied to what we write.
    let field_count = i16::try_from(types.len()).map_err(|_| {
        Error::encode(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "binary COPY supports at most {} columns, but {} were given",
                i16::MAX,
                types.len()
            ),
        ))
    })?;
    buf.put_i16(field_count);

    for (i, (value, type_)) in values.zip(types).enumerate() {
        let idx = buf.len();
        buf.put_i32(0);
        // Shares the bind path's domain fallback: `BinaryCopyInWriter` takes its
        // column types from the CALLER, who naturally obtains them from the
        // catalog or a prepared statement, both of which yield the DOMAIN.
        let len = match crate::query::encode_parameter(value.borrow_to_sql(), type_, buf)
            .map_err(|e| Error::to_sql(e, i))?
        {
            IsNull::Yes => -1,
            IsNull::No => i32::try_from(buf.len() - idx - 4)
                .map_err(|e| Error::encode(io::Error::new(io::ErrorKind::InvalidInput, e)))?,
        };
        BigEndian::write_i32(&mut buf[idx..], len);
    }

    Ok(())
}

struct Header {
    has_oids: bool,
}

pin_project! {
    /// A stream of rows deserialized from the PostgreSQL binary copy format.
    pub struct BinaryCopyOutStream {
        #[pin]
        stream: CopyOutStream,
        types: Arc<Vec<Type>>,
        header: Option<Header>,
    }
}

impl BinaryCopyOutStream {
    /// Creates a stream from a raw copy out stream and the types of the columns being returned.
    pub fn new(stream: CopyOutStream, types: &[Type]) -> BinaryCopyOutStream {
        BinaryCopyOutStream {
            stream,
            types: Arc::new(types.to_vec()),
            header: None,
        }
    }
}

impl Stream for BinaryCopyOutStream {
    type Item = Result<BinaryCopyOutRow, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        let chunk = match ready!(this.stream.poll_next(cx)) {
            Some(Ok(chunk)) => chunk,
            Some(Err(e)) => return Poll::Ready(Some(Err(e))),
            None => return Poll::Ready(Some(Err(Error::closed()))),
        };
        let mut chunk = Cursor::new(chunk);

        let has_oids = match &this.header {
            Some(header) => header.has_oids,
            None => {
                let header = match parse_binary_copy_header(&mut chunk) {
                    Ok(header) => header,
                    Err(e) => return Poll::Ready(Some(Err(e))),
                };
                let has_oids = header.has_oids;
                *this.header = Some(header);
                has_oids
            }
        };

        check_remaining(&chunk, 2)?;
        let raw = chunk.get_i16();
        if raw == -1 {
            return Poll::Ready(None);
        }

        let len = match tuple_field_count(raw, has_oids, this.types.len()) {
            Ok(len) => len,
            Err(e) => return Poll::Ready(Some(Err(e))),
        };

        let mut ranges = vec![];
        for _ in 0..len {
            check_remaining(&chunk, 4)?;
            let len = chunk.get_i32();
            if len == -1 {
                ranges.push(None);
            } else {
                let len = len as usize;
                check_remaining(&chunk, len)?;
                let start = chunk.position() as usize;
                ranges.push(Some(start..start + len));
                chunk.advance(len);
            }
        }

        // Every byte of a binary tuple is accounted for above, so a conforming
        // peer leaves NOTHING here: the PostgreSQL protocol's COPY Operations
        // section binds the backend to "zero or more CopyData messages (always
        // one per row)" in copy-out mode -- the frontend direction is
        // explicitly free to frame arbitrarily, this one is not. Whatever is
        // still in the chunk therefore belongs to tuples that will never be
        // returned, because the next poll reads the NEXT message. Dropping
        // them silently hands the caller a SHORT result with no error, which
        // is the one failure a caller cannot detect. Parsing on instead would
        // not close it: a peer free to pack two tuples into a message is
        // equally free to split one across two.
        if chunk.has_remaining() {
            return Poll::Ready(Some(Err(Error::parse(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} trailing bytes after a binary COPY tuple",
                    chunk.remaining()
                ),
            )))));
        }

        Poll::Ready(Some(Ok(BinaryCopyOutRow {
            buf: chunk.into_inner(),
            ranges,
            types: this.types.clone(),
        })))
    }
}

/// Parse the 19-byte binary-COPY file header from the front of `chunk`,
/// advancing the cursor past it (magic + flags + header-extension area).
///
/// Validates the magic value, rejects unknown critical flag bits, and reports
/// the recognized has-OIDs flag (bit 16).
fn parse_binary_copy_header(chunk: &mut Cursor<Bytes>) -> Result<Header, Error> {
    check_remaining(chunk, HEADER_LEN)?;
    if !chunk.chunk().starts_with(MAGIC) {
        return Err(Error::parse(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid magic value",
        )));
    }
    chunk.advance(MAGIC.len());

    let flags = chunk.get_i32();
    // Per PG COPY binary format spec: bits 16-31 are critical (abort on an
    // UNRECOGNIZED bit) and bits 0-15 are backward-compatible (ignore). Bit 16
    // is the recognized has-OIDs flag, so the unknown-critical range is bits
    // 17-31 (mask 0xFFFE_0000) — bit 16 must NOT be swallowed here, else the
    // has_oids report below becomes unreachable.
    if (flags as u32) & 0xFFFE_0000 != 0 {
        return Err(Error::parse(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported COPY header: critical flags set 0x{:08X}",
                flags as u32
            ),
        )));
    }
    let has_oids = (flags & (1 << 16)) != 0;

    let header_extension = chunk.get_u32() as usize;
    check_remaining(chunk, header_extension)?;
    chunk.advance(header_extension);

    Ok(Header { has_oids })
}

/// How many values a tuple header declares, checked against the column count
/// the caller asked for.
///
/// `raw` is the tuple's field count straight off the wire and `has_oids` comes
/// from a flag bit in the file header, so BOTH are chosen by the peer. The
/// `-1` end-of-data sentinel is handled by the caller; anything else reaching
/// here is a count.
fn tuple_field_count(raw: i16, has_oids: bool, expected: usize) -> Result<usize, Error> {
    // Widened before the adjustment: `raw` reaches `i16::MAX` and the OID adds
    // one, which overflows an i16 and aborts wherever overflow checks are on.
    // A negative count still lands far from any real column count once cast,
    // so it is refused below rather than needing its own arm.
    let len = i32::from(raw) + i32::from(has_oids);
    if len as usize != expected {
        return Err(Error::parse(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("expected {} values but got {}", expected, len),
        )));
    }
    Ok(len as usize)
}

fn check_remaining(buf: &Cursor<Bytes>, len: usize) -> Result<(), Error> {
    if buf.remaining() < len {
        Err(Error::parse(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected EOF",
        )))
    } else {
        Ok(())
    }
}

/// A row of data parsed from a binary copy out stream.
pub struct BinaryCopyOutRow {
    buf: Bytes,
    ranges: Vec<Option<Range<usize>>>,
    types: Arc<Vec<Type>>,
}

impl BinaryCopyOutRow {
    /// Like `get`, but returns a `Result` rather than panicking.
    pub fn try_get<'a, T>(&'a self, idx: usize) -> Result<T, Error>
    where
        T: FromSql<'a>,
    {
        let type_ = match self.types.get(idx) {
            Some(type_) => type_,
            None => return Err(Error::column(idx.to_string())),
        };

        if !T::accepts(type_) {
            return Err(Error::from_sql(
                Box::new(WrongType::new::<T>(type_.clone())),
                idx,
            ));
        }

        let r = match &self.ranges[idx] {
            Some(range) => T::from_sql(type_, &self.buf[range.clone()]),
            None => T::from_sql_null(type_),
        };

        r.map_err(|e| Error::from_sql(e, idx))
    }

    /// Deserializes a value from the row.
    ///
    /// # Panics
    ///
    /// Panics if the index is out of bounds or if the value cannot be converted to the specified type.
    pub fn get<'a, T>(&'a self, idx: usize) -> T
    where
        T: FromSql<'a>,
    {
        match self.try_get(idx) {
            Ok(value) => value,
            Err(e) => panic!("error retrieving column {}: {}", idx, e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 19-byte binary-COPY file header: MAGIC + flags (BE i32) +
    /// header-extension-length 0 (BE u32), wrapped in a `Cursor<Bytes>`.
    fn header_buf(flags: i32) -> Cursor<Bytes> {
        let mut buf = BytesMut::new();
        buf.put_slice(MAGIC);
        buf.put_i32(flags);
        buf.put_u32(0); // header extension length
        Cursor::new(buf.freeze())
    }

    #[test]
    fn header_accepts_oid_flag() {
        // Bit 16 is the recognized has-OIDs flag — it must NOT be rejected as
        // an unknown critical bit, and must surface as `has_oids == true`.
        let mut chunk = header_buf(1 << 16);
        let header = parse_binary_copy_header(&mut chunk).expect("OID-flag header must parse");
        assert!(header.has_oids, "bit 16 must be reported as has_oids");
    }

    #[test]
    fn header_rejects_unknown_critical_flag() {
        // Bit 17 is in the unknown-critical range (17-31): must abort.
        let mut chunk = header_buf(1 << 17);
        assert!(
            parse_binary_copy_header(&mut chunk).is_err(),
            "an unknown critical flag (bit 17) must be rejected"
        );
    }

    #[test]
    fn header_ignores_noncritical_low_bits() {
        // Bits 0-15 are backward-compatible (non-critical): accept, no OIDs.
        let mut chunk = header_buf(1 << 0);
        let header = parse_binary_copy_header(&mut chunk).expect("low-bit header must parse");
        assert!(!header.has_oids, "low bits must not set has_oids");
    }

    #[test]
    fn header_plain_no_oids() {
        let mut chunk = header_buf(0);
        let header = parse_binary_copy_header(&mut chunk).expect("plain header must parse");
        assert!(!header.has_oids, "no flags means no OIDs");
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut buf = BytesMut::new();
        buf.put_slice(b"NOTPGCOPY\0\0"); // 11 bytes, wrong magic
        buf.put_i32(0);
        buf.put_u32(0);
        let mut chunk = Cursor::new(buf.freeze());
        assert!(
            parse_binary_copy_header(&mut chunk).is_err(),
            "a bad magic value must be rejected"
        );
    }

    /// A peer that sets the has-OIDs flag and declares `i16::MAX` fields must
    /// get an error, not a panic.
    ///
    /// Both inputs are chosen by the peer: the flag is bit 16 of the file
    /// header and the count is the tuple's own `i16`. Adding one to
    /// `i16::MAX` overflows, and `[profile.dev]` in the workspace root sets no
    /// `overflow-checks`, so it inherits the default ON and the add ABORTS the
    /// process. Release wraps instead, and the wrapped value then fails the
    /// comparison, so this is a debug-profile panic rather than a wrong
    /// answer -- but a driver should not abort on bytes a peer chose.
    ///
    /// No conforming PostgreSQL sends this: has-OIDs went away in PG12 and the
    /// server's column ceiling is 1664. It takes a hostile or broken peer,
    /// which is the threat model `tests/hostile_peer.rs` already works in.
    /// WHAT THIS DOES NOT CATCH. Its discriminating power is DEBUG-ONLY. The
    /// regression it guards is reverting to `raw + has_oids as i16`, and that
    /// only aborts where overflow checks are on. In a release build the same
    /// arithmetic wraps to -32768, which still fails the `!= expected`
    /// comparison, so the error below still arrives and this test still passes.
    ///
    /// That is acceptable rather than fixed because `cargo test` builds in
    /// debug, so the ordinary path does discriminate - but a release-profile
    /// run of this suite would not, and nobody should read a green release run
    /// as evidence about the overflow.
    #[test]
    fn a_tuple_field_count_at_i16_max_with_oids_errors_rather_than_overflowing() {
        let error = tuple_field_count(i16::MAX, true, 1)
            .expect_err("a field count that cannot match must be refused");
        assert!(
            error
                .to_string()
                .contains("error parsing response from server"),
            "unexpected error: {error}"
        );
    }

    /// One variable away from the case above: same count, no OID flag, so no
    /// addition happens. It must still be REFUSED (32767 != 1). This is what
    /// keeps the test above honest -- were this one to panic as well, the
    /// overflow would not be the thing the other test caught.
    #[test]
    fn the_same_count_without_the_oid_flag_is_refused_without_arithmetic() {
        let error =
            tuple_field_count(i16::MAX, false, 1).expect_err("32767 fields cannot match 1 column");
        assert!(
            error
                .to_string()
                .contains("error parsing response from server"),
            "unexpected error: {error}"
        );
    }

    /// The counting still has to WORK, or "never panics" could be satisfied by
    /// refusing everything.
    #[test]
    fn a_matching_tuple_field_count_is_accepted_with_and_without_oids() {
        assert_eq!(
            tuple_field_count(2, false, 2).expect("2 fields against 2 columns"),
            2
        );
        // With the OID flag the wire count is one SHORT of the column count,
        // because the OID is the extra value.
        assert_eq!(
            tuple_field_count(1, true, 2).expect("1 field plus an OID against 2 columns"),
            2
        );
    }

    /// A column count that does not fit the wire's `int16` is refused, not
    /// wrapped.
    ///
    /// `BinaryCopyInWriter::new` takes the column types from the CALLER, so the
    /// count is caller-controlled. `types.len() as i16` wraps SILENTLY - a cast
    /// does not panic on overflow the way arithmetic does, so this was quiet in
    /// every build - and 32768 columns became -32768 on the wire. A negative
    /// field count desynchronises the COPY stream, which costs the connection
    /// rather than just the row.
    ///
    /// The read side already refuses its twin (`tuple_field_count` widens to
    /// `i32` before comparing, precisely so a hostile `i16::MAX` cannot
    /// overflow). This is the same rule applied to what we WRITE.
    #[test]
    fn a_column_count_that_cannot_fit_the_wire_is_refused() {
        let types = vec![Type::INT4; usize::from(u16::MAX) + 2];
        let mut buf = BytesMut::new();
        let values: Vec<&(dyn ToSql + Sync)> = Vec::new();
        let error = encode_row(&mut buf, &types, values.into_iter())
            .expect_err("a column count past i16::MAX must be refused, not wrapped");
        let chain = std::iter::successors(
            std::error::Error::source(&error),
            |e| std::error::Error::source(*e),
        )
        .fold(format!("{error}"), |acc, e| format!("{acc}: {e}"));
        assert!(
            chain.contains("at most") && chain.contains("columns"),
            "the refusal should name the column limit and the count given: {chain}"
        );
    }

    /// THE CONTROL, one variable: a count that DOES fit is still written, and
    /// written as itself. Refusing everything would satisfy the test above.
    #[test]
    fn a_column_count_that_fits_is_written_unchanged() {
        let types = vec![Type::INT4, Type::INT4, Type::INT4];
        let mut buf = BytesMut::new();
        let values: Vec<&(dyn ToSql + Sync)> = vec![&1i32, &2i32, &3i32];
        encode_row(&mut buf, &types, values.into_iter()).expect("a normal row encodes");
        assert_eq!(
            i16::from_be_bytes([buf[0], buf[1]]),
            3,
            "the field count on the wire must be the real column count"
        );
    }
}
