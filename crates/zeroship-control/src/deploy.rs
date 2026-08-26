//! `.zship` ingestion. The streaming pipeline lives in
//! `zeroship_bundle::unpack`. This module re-exports it for callers
//! that haven't migrated to the `zeroship_bundle::ingest` path
//! directly (notably `api.rs` and the `deploy_test.rs` integration
//! tests).

pub use zeroship_bundle::{
    ingest, IngestError, IngestSuccess, MAX_BLOBS_PER_DEPLOY, MAX_BLOB_BYTES,
    MAX_COMPRESSED_BYTES, MAX_DECOMPRESSED_BYTES, MAX_MANIFEST_BYTES,
};
