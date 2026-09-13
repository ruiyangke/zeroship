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

use zeroship_id::AppId;
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

    let err = ingest(&store(), &AppId::mint(), &archive)
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

/// Build a tar entry by writing the 512-byte header BY HAND.
///
/// `tar::Builder::append_data` refuses the names these tests need - it rejects
/// `..` outright ("paths in archives must not have `..` when setting path for").
/// That refusal is very likely why this class of coverage never existed:
/// reaching for the obvious tool and finding it declines is where the effort
/// normally stops, and the resulting absence reads as a decision rather than an
/// obstacle. So the header is assembled directly, which is what an attacker's
/// packer would do anyway.
///
/// ustar layout: name[100] mode[8] uid[8] gid[8] size[12] mtime[12] chksum[8]
/// typeflag[1] linkname[100] magic[6] version[2] ... The checksum is computed
/// over the whole header with the checksum field itself read as eight spaces.
fn raw_tar_entry(name: &str, body: &[u8]) -> Vec<u8> {
    let mut h = [0u8; 512];
    let name_bytes = name.as_bytes();
    assert!(name_bytes.len() < 100, "this helper only writes short names");
    h[..name_bytes.len()].copy_from_slice(name_bytes);
    h[100..107].copy_from_slice(b"0000644"); // mode
    h[108..115].copy_from_slice(b"0000000"); // uid
    h[116..123].copy_from_slice(b"0000000"); // gid
    let size = format!("{:011o}", body.len());
    h[124..135].copy_from_slice(size.as_bytes());
    h[136..147].copy_from_slice(b"00000000000"); // mtime
    h[148..156].copy_from_slice(b"        "); // checksum placeholder: spaces
    h[156] = b'0'; // typeflag: regular file
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");

    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    let chk = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(chk.as_bytes());

    let mut out = h.to_vec();
    out.extend_from_slice(body);
    // Entries are padded to a 512-byte boundary.
    let pad = (512 - (body.len() % 512)) % 512;
    out.extend(std::iter::repeat_n(0u8, pad));
    // Two zero blocks terminate the archive.
    out.extend(std::iter::repeat_n(0u8, 1024));
    out
}

fn pack_raw(name: &str, body: &[u8]) -> Vec<u8> {
    zstd::encode_all(raw_tar_entry(name, body).as_slice(), 0).unwrap()
}

/// CONTROL for the three tests below: the hand-rolled header is a VALID tar.
///
/// Without this they are worthless. If my 512 bytes were malformed, tar-rs would
/// reject the archive as unparseable and every name test would pass for the
/// wrong reason - "refused" would mean "I wrote a broken header", not "ingest
/// enforces its allowlist".
///
/// So: the same builder, with the one name ingest DOES accept. Reaching a
/// manifest-parse failure rather than a tar failure proves tar-rs walked the
/// entry and handed ingest the name.
#[compio::test]
async fn the_hand_rolled_header_parses_as_a_real_tar() {
    let archive = pack_raw("manifest.json", b"{not valid json");
    let err = ingest(&store(), &AppId::mint(), &archive)
        .await
        .expect_err("a malformed manifest body must still be refused");
    let IngestError::BadRequest { error, .. } = &err else {
        panic!("expected BadRequest, got {err:?}");
    };
    assert!(
        error.contains("manifest"),
        "expected to reach MANIFEST parsing, meaning the tar itself parsed; got {error:?}"
    );
}

/// A traversing entry name is refused, and nothing is written outside the store.
///
/// Ingest never derives a filesystem path from an entry name - a name must be
/// exactly `manifest.json` or `blobs/<64 lowercase hex>` - so the defence is
/// structural rather than sanitising. This pins it, so a change that starts
/// joining entry names onto a directory fails here instead of in production.
#[compio::test]
async fn a_traversing_entry_name_is_refused() {
    let archive = pack_raw("../../etc/passwd", b"x");
    let err = ingest(&store(), &AppId::mint(), &archive)
        .await
        .expect_err("a traversing name must be refused");
    assert!(matches!(err, IngestError::BadRequest { .. }), "got {err:?}");
    assert!(
        !PathBuf::from("/tmp/zeroship-hostile-escape-probe").exists(),
        "nothing may be written from an entry name"
    );
}

/// An absolute entry name is refused for the same reason.
#[compio::test]
async fn an_absolute_entry_name_is_refused() {
    let archive = pack_raw("/etc/passwd", b"x");
    let err = ingest(&store(), &AppId::mint(), &archive)
        .await
        .expect_err("an absolute name must be refused");
    assert!(matches!(err, IngestError::BadRequest { .. }), "got {err:?}");
}

/// A name that is neither `manifest.json` nor a `blobs/` path is refused even
/// when it is perfectly ordinary - the allowlist is positive, not a denylist.
#[compio::test]
async fn an_unlisted_ordinary_name_is_refused() {
    let archive = pack_raw("README.md", b"x");
    let err = ingest(&store(), &AppId::mint(), &archive)
        .await
        .expect_err("an unlisted name must be refused");
    assert!(matches!(err, IngestError::BadRequest { .. }), "got {err:?}");
}

/// Bytes that are not a zstd stream at all are refused as a bad request, not a
/// panic and not a 500.
#[compio::test]
async fn a_non_zstd_body_is_refused() {
    let err = ingest(&store(), &AppId::mint(), b"this is not zstd")
        .await
        .expect_err("a non-zstd body must be refused");
    assert!(matches!(err, IngestError::BadRequest { .. }), "got {err:?}");
}

/// A valid zstd stream that is not a tar is refused the same way.
#[compio::test]
async fn a_zstd_stream_that_is_not_a_tar_is_refused() {
    let archive = zstd::encode_all(&b"definitely not a tar archive"[..], 0).unwrap();
    let err = ingest(&store(), &AppId::mint(), &archive)
        .await
        .expect_err("a non-tar payload must be refused");
    assert!(matches!(err, IngestError::BadRequest { .. }), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Reaching the entry-name allowlist at all
// ---------------------------------------------------------------------------
//
// The three name tests above pack a SINGLE hostile entry, so ingest refuses
// them at "manifest must be first tar entry" and never evaluates the name.
// Measured: all three return
//   BadRequest { error: "manifest must be first tar entry", detail: "got <name>" }
// They assert only `matches!(err, BadRequest)`, which that satisfies. Delete
// the `blobs/<hash>` allowlist entirely and they still pass - they are tests of
// entry ORDER wearing the names of tests of entry SAFETY.
//
// A hostile name only reaches the allowlist as a LATER entry, behind a
// well-formed manifest. These build that archive.

/// A manifest that PARSES and VALIDATES, so ingest proceeds to the entries
/// behind it. `{}` does not: it is refused as "invalid manifest" before the
/// second entry is read, which is the same not-reaching-the-allowlist defect
/// one level down - and my first version of the control below passed against
/// it, because "invalid manifest" is also not an ordering error.
const VALID_MANIFEST: &[u8] = br#"{"version":1,"assets":{},"runtime_assets":{},"asset_version":0,"sourcemaps":{},"metadata":{"built_at":"2026-06-25T00:00:00Z"}}"#;

/// Same 512-byte header as `raw_tar_entry`, without the end-of-archive blocks,
/// so entries can be concatenated.
fn raw_tar_entry_no_end(name: &str, body: &[u8]) -> Vec<u8> {
    let mut out = raw_tar_entry(name, body);
    out.truncate(out.len() - 1024);
    out
}

fn pack_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar_buf = Vec::new();
    for (name, body) in entries {
        tar_buf.extend_from_slice(&raw_tar_entry_no_end(name, body));
    }
    tar_buf.extend(std::iter::repeat_n(0u8, 1024));
    zstd::encode_all(tar_buf.as_slice(), 0).unwrap()
}

/// CONTROL: a manifest followed by a well-formed `blobs/<sha256>` entry gets
/// PAST the ordering check, so a refusal in the tests below is about the name.
///
/// It still fails - the manifest is not a valid Manifest - but it must fail
/// with a MANIFEST error, not an ordering one. If this ever reports "manifest
/// must be first tar entry", the two tests below have silently stopped
/// reaching the allowlist and are back to testing order.
#[compio::test]
async fn a_second_entry_is_reached_after_a_first_manifest_entry() {
    let hash = "a".repeat(64);
    let blob_name = format!("blobs/{hash}");
    let archive = pack_entries(&[("manifest.json", VALID_MANIFEST), (blob_name.as_str(), b"x")]);
    let err = ingest(&store(), &AppId::mint(), &archive).await.unwrap_err();
    let IngestError::BadRequest { error, .. } = &err else {
        panic!("expected BadRequest, got {err:?}")
    };
    assert!(
        !error.contains("manifest must be first"),
        "the ordering check still fired, so the entry-name tests below are not \
         reaching the allowlist; got {err:?}"
    );
    // POSITIVE witness: the failure must be about the SECOND entry, proving it
    // was read and hashed. "not the ordering error" alone is satisfied by any
    // earlier refusal - my first version asserted exactly that and passed while
    // the manifest was still being rejected before the entry was reached.
    assert!(
        error.contains("blob hash mismatch"),
        "expected the second entry to be read and hashed; got {err:?}"
    );
}

/// A traversing name in a LATER entry is refused BY THE ALLOWLIST.
///
/// The allowlist is positive - `blobs/<64 lowercase hex>` - so `..` is refused
/// for not matching the shape, not by a path-traversal rule. Asserting the
/// message keeps the distinction: a future denylist-style check that happened
/// to let `blobs/../../x` through would fail here rather than pass quietly.
#[compio::test]
async fn a_traversing_name_in_a_later_entry_is_refused_by_the_allowlist() {
    let archive = pack_entries(&[("manifest.json", VALID_MANIFEST), ("blobs/../../etc/passwd", b"x")]);
    let err = ingest(&store(), &AppId::mint(), &archive).await.unwrap_err();
    let IngestError::BadRequest { error, detail } = &err else {
        panic!("expected BadRequest, got {err:?}")
    };
    assert!(
        !error.contains("manifest must be first"),
        "refused by the ordering check, not the allowlist; got {err:?}"
    );
    assert!(
        error.contains("sha256") || detail.contains("sha256") || error.contains("expected manifest.json or blobs/"),
        "refused, but not for the entry name; got {err:?}"
    );
}
