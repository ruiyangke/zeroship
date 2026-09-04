//! The primitive layer: big-endian integers, length-prefixed bytes, and the
//! checked reads every frame decoder is built from.
//!
//! The layout rules, quoted from the proposal so they are checkable against it:
//!
//! - "Integers are big-endian; byte and UTF-8 strings carry a `u32_be` length;
//!   enum discriminants are `u8`."
//! - "`Option<T>` is `u8(0)` or `u8(1) || T`."
//! - "Structs concatenate fields in declaration order with no field numbers or
//!   padding."
//! - "`0x00` and `0x01` are the only Boolean encodings."
//! - "Counts, lengths and multiplication are checked before allocation."
//!
//! The last one is why [`Reader::count`] exists and why no decoder in this crate
//! calls `Vec::with_capacity` on a peer-supplied number directly: a `u32` count
//! is four bytes that can ask for four gigabytes, and the check that stops it has
//! to happen before the allocation, not after.

use core::fmt;

use crate::error::{DecodeError, EncodeError};
use crate::limits::{FRAME_LENGTH_PREFIX_BYTES, MAX_FRAME_BYTES};

/// A bounds-checked cursor over a byte slice.
///
/// Every read either advances the cursor or returns an error; there is no
/// partial-consumption arm, so a failed field cannot leave the cursor pointing
/// into the middle of one.
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl fmt::Debug for Reader<'_> {
    /// Prints the remaining length and nothing else. The buffer is peer bytes -
    /// a derived `Debug` here is a row-value leak into any log line that formats
    /// a decoder.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reader")
            .field("remaining", &self.buf.len())
            .finish()
    }
}

impl<'a> Reader<'a> {
    /// Wrap a slice.
    #[must_use]
    pub const fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    /// Bytes not yet consumed.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.buf.len()
    }

    /// Consume exactly `n` bytes.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`] when fewer than `n` bytes remain.
    pub const fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        match self.buf.split_at_checked(n) {
            Some((head, tail)) => {
                self.buf = tail;
                Ok(head)
            }
            None => Err(DecodeError::Truncated {
                needed: n,
                available: self.buf.len(),
            }),
        }
    }

    /// Consume one byte.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`] on an empty cursor.
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        let head = self.take(1)?;
        head.first().copied().ok_or(DecodeError::Truncated {
            needed: 1,
            available: 0,
        })
    }

    /// Consume a big-endian `u16`.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`] when fewer than two bytes remain.
    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.array::<2>()?))
    }

    /// Consume a big-endian `u32`.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`] when fewer than four bytes remain.
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.array::<4>()?))
    }

    /// Consume a big-endian `u64`.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`] when fewer than eight bytes remain.
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.array::<8>()?))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let head = self.take(N)?;
        <[u8; N]>::try_from(head).map_err(|_| DecodeError::Truncated {
            needed: N,
            available: head.len(),
        })
    }

    /// Consume a Boolean. Only `0x00` and `0x01` decode; anything else is fatal.
    ///
    /// # Errors
    /// [`DecodeError::InvalidBoolean`] for any other byte, or
    /// [`DecodeError::Truncated`].
    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0x00 => Ok(false),
            0x01 => Ok(true),
            value => Err(DecodeError::InvalidBoolean { value }),
        }
    }

    /// Consume a `u32_be`-length-prefixed byte string.
    ///
    /// # Errors
    /// [`DecodeError::LengthOverflow`] when the declared length does not fit
    /// `usize`, or [`DecodeError::Truncated`].
    pub fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = usize::try_from(self.u32()?).map_err(|_| DecodeError::LengthOverflow)?;
        self.take(len)
    }

    /// Consume a `u32_be`-length-prefixed UTF-8 string.
    ///
    /// # Errors
    /// [`DecodeError::InvalidUtf8`] for non-UTF-8 bytes, plus whatever
    /// [`Self::bytes`] refuses.
    pub fn string(&mut self) -> Result<String, DecodeError> {
        let raw = self.bytes()?;
        core::str::from_utf8(raw)
            .map(ToOwned::to_owned)
            .map_err(|_| DecodeError::InvalidUtf8)
    }

    /// Consume an `Option<T>`: `0x00`, or `0x01` followed by `T`.
    ///
    /// # Errors
    /// [`DecodeError::InvalidBoolean`] for any other tag byte, plus whatever
    /// `decode` returns.
    pub fn option<T, F>(&mut self, decode: F) -> Result<Option<T>, DecodeError>
    where
        F: FnOnce(&mut Self) -> Result<T, DecodeError>,
    {
        if self.bool()? {
            Ok(Some(decode(self)?))
        } else {
            Ok(None)
        }
    }

    /// Consume a `u32_be` element count and prove the remaining input could hold
    /// that many elements at `min_element_bytes` each.
    ///
    /// This is the pre-allocation check. `min_element_bytes` is the smallest
    /// encoding any element of that vector can have - never zero, or the check
    /// is vacuous.
    ///
    /// # Errors
    /// [`DecodeError::CountExceedsInput`] when the input cannot hold the claimed
    /// count, or [`DecodeError::LengthOverflow`] when the multiplication does not
    /// fit `usize`.
    pub fn count(
        &mut self,
        kind: &'static str,
        min_element_bytes: usize,
    ) -> Result<u32, DecodeError> {
        let count = self.u32()?;
        let needed = usize::try_from(count)
            .ok()
            .and_then(|n| n.checked_mul(min_element_bytes))
            .ok_or(DecodeError::LengthOverflow)?;
        if needed > self.remaining() {
            return Err(DecodeError::CountExceedsInput { kind, count });
        }
        Ok(count)
    }

    /// Consume a counted vector, checking the count before allocating.
    ///
    /// # Errors
    /// Whatever [`Self::count`] refuses, plus whatever `decode` returns.
    pub fn vec<T, F>(
        &mut self,
        kind: &'static str,
        min_element_bytes: usize,
        mut decode: F,
    ) -> Result<Vec<T>, DecodeError>
    where
        F: FnMut(&mut Self) -> Result<T, DecodeError>,
    {
        let count = self.count(kind, min_element_bytes)?;
        let mut out = Vec::with_capacity(count as usize);
        for _ in 0..count {
            out.push(decode(self)?);
        }
        Ok(out)
    }

    /// Assert the input is fully consumed. "Trailing payload bytes ... is a fatal
    /// protocol violation": a payload longer than its declared fields is a
    /// different message, not a tolerable one.
    ///
    /// # Errors
    /// [`DecodeError::TrailingBytes`] when anything is left.
    pub const fn finish(self) -> Result<(), DecodeError> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes {
                remaining: self.buf.len(),
            })
        }
    }
}

/// An append-only encode buffer.
#[derive(Default, Clone)]
pub struct Writer {
    buf: Vec<u8>,
}

impl fmt::Debug for Writer {
    /// Length only. The buffer holds encoded row values and, for a
    /// `SubscribeRequest`, a credential.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Writer")
            .field("len", &self.buf.len())
            .finish()
    }
}

impl Writer {
    /// A new empty buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Bytes written so far.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Append one byte.
    pub fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    /// Append a big-endian `u16`.
    pub fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// Append a big-endian `u32`.
    pub fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// Append a big-endian `u64`.
    pub fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// Append a Boolean as `0x00` or `0x01`.
    pub fn bool(&mut self, value: bool) {
        self.buf.push(u8::from(value));
    }

    /// Append raw bytes with no length prefix.
    pub fn raw(&mut self, value: &[u8]) {
        self.buf.extend_from_slice(value);
    }

    /// Append a `u32_be`-length-prefixed byte string.
    ///
    /// # Errors
    /// [`EncodeError::ValueTooLarge`] when the length does not fit `u32`.
    pub fn bytes(&mut self, value: &[u8]) -> Result<(), EncodeError> {
        let len = u32::try_from(value.len())
            .map_err(|_| EncodeError::ValueTooLarge { len: value.len() })?;
        self.u32(len);
        self.buf.extend_from_slice(value);
        Ok(())
    }

    /// Append a `u32_be`-length-prefixed UTF-8 string.
    ///
    /// # Errors
    /// [`EncodeError::ValueTooLarge`] when the length does not fit `u32`.
    pub fn string(&mut self, value: &str) -> Result<(), EncodeError> {
        self.bytes(value.as_bytes())
    }

    /// Append a `u32_be` element count.
    ///
    /// # Errors
    /// [`EncodeError::TooManyElements`] when the count does not fit `u32`.
    pub fn count(&mut self, count: usize) -> Result<(), EncodeError> {
        let count = u32::try_from(count).map_err(|_| EncodeError::TooManyElements { count })?;
        self.u32(count);
        Ok(())
    }

    /// Append an `Option<T>` as `0x00`, or `0x01` followed by `T`.
    ///
    /// # Errors
    /// Whatever `encode` returns.
    pub fn option<T, F>(&mut self, value: Option<&T>, encode: F) -> Result<(), EncodeError>
    where
        F: FnOnce(&mut Self, &T) -> Result<(), EncodeError>,
    {
        match value {
            None => {
                self.bool(false);
                Ok(())
            }
            Some(inner) => {
                self.bool(true);
                encode(self, inner)
            }
        }
    }

    /// Take the encoded bytes.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// Peek the total on-wire size of the frame a buffer starts with.
///
/// Returns `Ok(None)` when fewer than four bytes are available - the caller has
/// not read the length prefix yet. Otherwise returns the WHOLE frame size,
/// prefix included, so a reader can buffer exactly that much.
///
/// The two fatal length checks happen here rather than after the body arrives,
/// which is the point: a peer that declares 4 GiB must be refused before the
/// process tries to buffer it.
///
/// # Errors
/// [`DecodeError::ZeroFrameLength`], [`DecodeError::FrameTooLarge`] or
/// [`DecodeError::LengthOverflow`].
pub fn frame_length_prefix(buf: &[u8]) -> Result<Option<usize>, DecodeError> {
    let Some((head, _)) = buf.split_at_checked(FRAME_LENGTH_PREFIX_BYTES) else {
        return Ok(None);
    };
    let declared = <[u8; FRAME_LENGTH_PREFIX_BYTES]>::try_from(head)
        .map(u32::from_be_bytes)
        .map_err(|_| DecodeError::LengthOverflow)?;
    if declared == 0 {
        return Err(DecodeError::ZeroFrameLength);
    }
    if declared > MAX_FRAME_BYTES {
        return Err(DecodeError::FrameTooLarge { declared });
    }
    let total = usize::try_from(declared)
        .ok()
        .and_then(|n| n.checked_add(FRAME_LENGTH_PREFIX_BYTES))
        .ok_or(DecodeError::LengthOverflow)?;
    Ok(Some(total))
}
