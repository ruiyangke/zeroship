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

        this.buf.put_i16(this.types.len() as i16);

        for (i, (value, type_)) in values.zip(this.types).enumerate() {
            let idx = this.buf.len();
            this.buf.put_i32(0);
            let len = match value
                .borrow_to_sql()
                .to_sql_checked(type_, this.buf)
                .map_err(|e| Error::to_sql(e, i))?
            {
                IsNull::Yes => -1,
                IsNull::No => i32::try_from(this.buf.len() - idx - 4)
                    .map_err(|e| Error::encode(io::Error::new(io::ErrorKind::InvalidInput, e)))?,
            };
            BigEndian::write_i32(&mut this.buf[idx..], len);
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
        let mut len = chunk.get_i16();
        if len == -1 {
            return Poll::Ready(None);
        }

        if has_oids {
            len += 1;
        }
        if len as usize != this.types.len() {
            return Poll::Ready(Some(Err(Error::parse(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("expected {} values but got {}", this.types.len(), len),
            )))));
        }

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
}
