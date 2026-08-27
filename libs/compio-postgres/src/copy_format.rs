use crate::Error;
use fallible_iterator::FallibleIterator;
use std::io;

/// The format PostgreSQL selected for a `COPY` stream or one of its columns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopyFormat {
    /// PostgreSQL's textual `COPY` representation.
    Text,
    /// PostgreSQL's binary `COPY` representation.
    Binary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CopyResponse {
    format: CopyFormat,
    column_formats: Box<[CopyFormat]>,
}

impl CopyResponse {
    pub(crate) fn from_backend<I>(format: u8, formats: I) -> Result<Self, Error>
    where
        I: FallibleIterator<Item = u16, Error = io::Error>,
    {
        Self::from_formats(format, formats).map_err(Error::parse)
    }

    pub(crate) fn from_wire(body: &[u8]) -> Result<Self, Error> {
        validate_wire(body).map_err(Error::parse)?;

        let format = decode_format(u16::from(body[0])).map_err(Error::parse)?;
        let count = usize::from(u16::from_be_bytes([body[1], body[2]]));
        let mut column_formats = Vec::with_capacity(count);
        for code in body[3..].chunks_exact(2) {
            column_formats
                .push(decode_format(u16::from_be_bytes([code[0], code[1]])).map_err(Error::parse)?);
        }

        Ok(Self {
            format,
            column_formats: column_formats.into_boxed_slice(),
        })
    }

    fn from_formats<I>(format: u8, mut formats: I) -> io::Result<Self>
    where
        I: FallibleIterator<Item = u16, Error = io::Error>,
    {
        let format = decode_format(u16::from(format))?;
        // The protocol count is a u16. Preserve that hard bound even if this
        // helper is later called with a different iterator implementation.
        let capacity = formats.size_hint().0.min(usize::from(u16::MAX));
        let mut column_formats = Vec::with_capacity(capacity);
        while let Some(code) = formats.next()? {
            let column_format = decode_format(code)?;
            if format == CopyFormat::Text && column_format != CopyFormat::Text {
                return Err(invalid_data(
                    "binary column format in a textual COPY response",
                ));
            }
            column_formats.push(column_format);
        }

        Ok(Self {
            format,
            column_formats: column_formats.into_boxed_slice(),
        })
    }

    pub(crate) const fn format(&self) -> CopyFormat {
        self.format
    }

    pub(crate) fn column_formats(&self) -> &[CopyFormat] {
        &self.column_formats
    }
}

impl Default for CopyResponse {
    fn default() -> Self {
        Self {
            format: CopyFormat::Text,
            column_formats: Box::default(),
        }
    }
}

pub(crate) fn validate_wire(body: &[u8]) -> io::Result<()> {
    if body.len() < 3 {
        return Err(invalid_data(format!(
            "COPY response body is {} bytes; expected at least 3",
            body.len()
        )));
    }

    let format = decode_format(u16::from(body[0]))?;
    let count = usize::from(u16::from_be_bytes([body[1], body[2]]));
    let expected = 3usize
        .checked_add(count.checked_mul(2).ok_or_else(|| {
            invalid_data("COPY response column-format byte count overflowed usize")
        })?)
        .ok_or_else(|| invalid_data("COPY response length overflowed usize"))?;
    if body.len() != expected {
        return Err(invalid_data(format!(
            "COPY response declares {count} columns but has {} format bytes",
            body.len() - 3
        )));
    }

    for code in body[3..].chunks_exact(2) {
        let column_format = decode_format(u16::from_be_bytes([code[0], code[1]]))?;
        if format == CopyFormat::Text && column_format != CopyFormat::Text {
            return Err(invalid_data(
                "binary column format in a textual COPY response",
            ));
        }
    }
    Ok(())
}

fn decode_format(code: u16) -> io::Result<CopyFormat> {
    match code {
        0 => Ok(CopyFormat::Text),
        1 => Ok(CopyFormat::Binary),
        _ => Err(invalid_data(format!(
            "invalid COPY format code {code}; expected 0 (text) or 1 (binary)"
        ))),
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
