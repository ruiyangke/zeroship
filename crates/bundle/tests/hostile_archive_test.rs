//! Ingest against archives a friendly packer would never emit.
//!
//! Every other archive in this crate's tests is built by `tar::Builder` from the
//! same shapes the vite packer produces, so the parser has only ever been
//! exercised on well-formed input - while the parser is the part that sees
//! untrusted bytes. A review found a defect here that a single fixture of this
//! kind would have caught, so this file exists to hold that kind.
//!
//! These are mostly CHARACTERIZATION tests: they pin defences that already
//! work, so that a refactor which removes one fails here rather than in
//! production. The exception is the long-name case, which pins a bound added
//! after the defect was measured.

use std::path::PathBuf;
use std::sync::Arc;

use uuid::Uuid;
use zeroship_bundle::blob::{BlobStore, LocalDiskBlobStore};
use zeroship_bundle::{ingest, IngestError};

fn tmpdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zeroship-hostile-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn store() -> Arc<dyn BlobStore> {
    Arc::new(LocalDiskBlobStore::new(tmpdir()).expect("local store"))
}

/// Build a tar.zst whose FIRST entry has the given name and body.
fn pack_first_entry(name: &str, body: &[u8]) -> Vec<u8> {
    let mut tar_buf: Vec<u8> = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, body).unwrap();
        builder.finish().unwrap();
    }
    zstd::encode_all(tar_buf.as_slice(), 0).unwrap()
}

/// A very long entry name must not turn into a very long error body.
///
/// This is the shape that was measured at roughly 290,000x amplification: tar-rs
/// reads a GNU long-name entry WHOLE inside `entries.next()`, before any ingest
/// limit runs, and the resulting name was then formatted into an `IngestError`
/// detail that the control plane puts straight into the 400 response.
///
/// **What this catches**: an unbounded creator-controlled string reaching the
/// error detail through the real ingest path. A modest 64 KiB name is used
/// rather than the 200 MiB of the original measurement - the amplification is
/// linear, so the bound is what matters, not the size.
///
/// **What this does NOT catch**: the allocation `entries.next()` performs before
/// ingest sees anything. That is a HOLE - nothing asserts it, and bounding it
/// means bounding the legitimate 256 MiB bundle budget, which is a separate
/// decision.
#[compio::test]
async fn a_long_entry_name_does_not_become_a_long_error_body() {
    let long_name = "a".repeat(64 * 1024);
    let archive = pack_first_entry(&long_name, b"x");

    let err = ingest(&store(), &Uuid::now_v7(), &archive)
        .await
        .expect_err("an archive whose first entry is not manifest.json must be refused");

    let detail = match err {
        IngestError::BadRequest { detail, .. } => detail,
        other => panic!("expected BadRequest, got {other:?}"),
    };
    assert!(
        detail.len() < 4096,
        "a {} byte entry name produced a {} byte error detail",
        long_name.len(),
        detail.len()
    );
}

// NOT COVERED HERE: traversing (`../../etc/passwd`) and absolute (`/etc/passwd`)
// entry names.
//
// I wrote both and they failed - not because ingest accepted them, but because
// `tar::Builder::append_data` REFUSES TO WRITE them: "paths in archives must not
// have `..` when setting path for". The friendly writer will not produce a
// hostile archive, which is very likely why this whole class of coverage never
// existed: reaching for the obvious tool and finding it declines is where the
// effort stops.
//
// Constructing those cases needs hand-written 512-byte tar headers with the name
// field set directly, bypassing Builder entirely. That is worth doing and is not
// done here.
//
// This is a HOLE, not a handoff. Nothing else in the repo feeds ingest such a
// name. What IS established, by reading rather than by test: no filesystem write
// is ever derived from an entry name - a name must be exactly `manifest.json` or
// `blobs/<64 lowercase hex>` or it is a 400 - and the workspace contains zero
// uses of `Archive::unpack`, `Entry::unpack` or `unpack_in`. So the defence is
// structural rather than sanitising, which is the stronger arrangement; it is
// simply unasserted.

/// Bytes that are not a zstd stream at all are refused as a bad request, not a
/// panic and not a 500.
#[compio::test]
async fn a_non_zstd_body_is_refused() {
    let err = ingest(&store(), &Uuid::now_v7(), b"this is not zstd")
        .await
        .expect_err("a non-zstd body must be refused");
    assert!(matches!(err, IngestError::BadRequest { .. }), "got {err:?}");
}

/// A valid zstd stream that is not a tar is refused the same way.
#[compio::test]
async fn a_zstd_stream_that_is_not_a_tar_is_refused() {
    let archive = zstd::encode_all(&b"definitely not a tar archive"[..], 0).unwrap();
    let err = ingest(&store(), &Uuid::now_v7(), &archive)
        .await
        .expect_err("a non-tar payload must be refused");
    assert!(matches!(err, IngestError::BadRequest { .. }), "got {err:?}");
}
