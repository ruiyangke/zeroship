//! Shared WebIDL boundary types — sugar that the `#[v8_class]` proc macro
//! recognises in arg position and converts before user method bodies run.
//!
//! These types fall out of the WebIDL spec's "extended attributes" and
//! the basic string conversions — `[Clamp]`, `[EnforceRange]`,
//! `ByteString`, `USVString`. They're crate-internal scaffolding,
//! re-exported at the crate root for convenience.

pub mod byte_string;
pub mod clamp;
pub mod convert;
pub mod enforce_range;
pub mod usv_string;
