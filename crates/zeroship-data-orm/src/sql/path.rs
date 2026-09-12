//! Field paths: a column, optionally with nested JSON access.
//!
//! The grammar retains nested segments, but ORM reads currently refuse them
//! until their portable typing and compilation contract is implemented.

use crate::sql::ident::Ident;
use core::fmt;

/// The deepest nested access a path may express.
///
/// The deepest nested access accepted by the grammar.
pub const MAX_PATH_SEGMENTS: usize = 8;

/// The longest a single JSON key may be.
pub const MAX_JSON_KEY_BYTES: usize = 255;

/// A JSON object key.
///
/// This is **not** an [`Ident`] and must not be validated as one: it names a
/// key inside a document, not a database object, so it is legitimately any
/// UTF-8 string and the 63-byte ASCII identifier fence would be wrong. It is
/// still a validated newtype so compilers never receive an unchecked key.
///
/// When a lowering is written, a key must be emitted as a **bind parameter**
/// (`"col" ->> $1`), never interpolated. That keeps it a value, which is what
/// it is, and it means two queries differing only in their key share one
/// prepared statement.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JsonKey(String);

impl JsonKey {
    /// Validate a JSON object key.
    ///
    /// # Errors
    ///
    /// [`PathError::EmptyJsonKey`], [`PathError::NulByteInJsonKey`], or
    /// [`PathError::JsonKeyTooLong`].
    pub fn new(raw: impl Into<String>) -> Result<Self, PathError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(PathError::EmptyJsonKey);
        }
        if raw.contains('\0') {
            return Err(PathError::NulByteInJsonKey);
        }
        if raw.len() > MAX_JSON_KEY_BYTES {
            return Err(PathError::JsonKeyTooLong { len: raw.len() });
        }
        Ok(Self(raw))
    }

    /// The validated key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A column, optionally with nested JSON access below it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldPath {
    source: Option<Ident>,
    root: Ident,
    segments: Vec<JsonKey>,
}

impl FieldPath {
    /// A plain column reference.
    #[must_use]
    pub const fn column(root: Ident) -> Self {
        Self {
            source: None,
            root,
            segments: Vec::new(),
        }
    }

    /// A column with nested JSON access.
    ///
    /// # Errors
    ///
    /// [`PathError::EmptyPathSegments`] if `segments` is empty - that is
    /// [`FieldPath::column`], and having two spellings of one value would
    /// break the canonical-form property. [`PathError::PathTooDeep`] past
    /// [`MAX_PATH_SEGMENTS`].
    pub fn nested(root: Ident, segments: Vec<JsonKey>) -> Result<Self, PathError> {
        if segments.is_empty() {
            return Err(PathError::EmptyPathSegments);
        }
        if segments.len() > MAX_PATH_SEGMENTS {
            return Err(PathError::PathTooDeep {
                depth: segments.len(),
            });
        }
        Ok(Self {
            source: None,
            root,
            segments,
        })
    }

    /// Qualify the column independently of its JSON path.
    #[must_use]
    pub fn in_source(mut self, source: Ident) -> Self {
        self.source = Some(source);
        self
    }

    #[must_use]
    pub fn source(&self) -> Option<&Ident> {
        self.source.as_ref()
    }

    /// The column the path starts at.
    #[must_use]
    pub const fn root(&self) -> &Ident {
        &self.root
    }

    /// The nested keys, outermost first. Empty for a plain column.
    #[must_use]
    pub fn segments(&self) -> &[JsonKey] {
        &self.segments
    }

    /// Whether this path reaches inside a document.
    #[must_use]
    pub const fn is_nested(&self) -> bool {
        !self.segments.is_empty()
    }
}

/// Why a path was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    EmptyJsonKey,
    NulByteInJsonKey,
    JsonKeyTooLong { len: usize },
    EmptyPathSegments,
    PathTooDeep { depth: usize },
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyJsonKey => f.write_str("a JSON path segment cannot be empty"),
            Self::NulByteInJsonKey => {
                f.write_str("a JSON path segment must not contain a NUL byte")
            }
            Self::JsonKeyTooLong { len } => write!(
                f,
                "a JSON path segment of {len} bytes exceeds the maximum of \
                 {MAX_JSON_KEY_BYTES}"
            ),
            Self::EmptyPathSegments => f.write_str(
                "a nested path needs at least one segment; use FieldPath::column for a \
                 plain column",
            ),
            Self::PathTooDeep { depth } => write!(
                f,
                "a path of depth {depth} exceeds the maximum of {MAX_PATH_SEGMENTS}"
            ),
        }
    }
}

impl std::error::Error for PathError {}
