//! `MinIO`-backed integration smoke test for `compio-s3`.
//!
//! This test is **self-contained**: it starts its own `MinIO` container, creates
//! a bucket, exercises the full v1 client surface (put / get / head / list /
//! delete plus a multipart round-trip with parts ≥ 5 `MiB`), and tears the
//! container down. It **REFUSES** when Docker is unavailable: a machine
//! without Docker gets a named failure saying what to install, never a green
//! that exercised no client at all.

#![allow(clippy::future_not_send)]
//!
//! Run explicitly:
//!   `cargo test -p compio-s3 --test minio_smoke -- --nocapture`
//!
//! The container is path-style, plaintext HTTP on loopback (`dev_http=true`),
//! which is the only mode `MinIO` is wired for here.


use bytes::Bytes;
use compio_s3::client::{PutOptions, S3Client};

#[path = "../../../tests/fixtures/s3.rs"]
mod s3_fixture;

#[test]
fn minio_full_surface_and_multipart() {
    let minio = s3_fixture::Minio::start();
    let client = S3Client::new(minio.config("smoke"), minio.credentials());
    compio::runtime::Runtime::new()
        .expect("compio runtime")
        .block_on(run_smoke(client));
}

async fn run_smoke(c: S3Client) {

    // ---- single-object put / head / get / list / delete ----
    let key = "hello.txt";
    let body = b"hello, zeroship s3";
    c.put(
        key,
        body,
        PutOptions {
            content_type: "text/plain",
            ..Default::default()
        },
    )
    .await
    .expect("put");

    let meta = c.head_object(key).await.expect("head").expect("head Some");
    assert_eq!(meta.len, body.len() as u64);
    assert_eq!(meta.content_type.as_deref(), Some("text/plain"));

    let (got, gmeta) = c.get(key, 1024).await.expect("get");
    assert_eq!(got.as_ref(), body);
    assert_eq!(gmeta.len, body.len() as u64);

    // head of a missing key => None
    assert!(c.head_object("nope.txt").await.expect("head missing").is_none());

    // list should contain our key (logical key, prefix stripped)
    let entries = c.list("").await.expect("list");
    assert!(entries.iter().any(|e| e.key == key), "list missing key: {entries:?}");

    c.delete(key).await.expect("delete");
    assert!(c.head_object(key).await.expect("head after delete").is_none());

    // ---- multipart round-trip: 2 parts, first ≥ 5 MiB, last smaller ----
    let mp_key = "big.bin";
    let part_size = 5 * 1024 * 1024; // 5 MiB (S3 multipart minimum part size)
    let part1 = vec![0xABu8; part_size];
    let part2 = vec![0xCDu8; 1024]; // small last part

    let upload = c
        .create_multipart(mp_key, "application/octet-stream")
        .await
        .expect("create_multipart");

    // Wrap the part loop so any error aborts the upload (orphaned parts bill).
    let mp_result = async {
        let e1 = c
            .upload_part(mp_key, &upload, 1, Bytes::from(part1.clone()))
            .await?;
        let e2 = c
            .upload_part(mp_key, &upload, 2, Bytes::from(part2.clone()))
            .await?;
        c.complete_multipart(mp_key, &upload, &[e1, e2]).await
    }
    .await;

    if let Err(e) = mp_result {
        // Mandatory abort on any mid-upload error.
        let _ = c.abort_multipart(mp_key, &upload).await;
        panic!("multipart failed: {e:?}");
    }

    // Verify the assembled object: size + content via streaming download.
    let total = (part_size + 1024) as u64;
    let head = c
        .head_object(mp_key)
        .await
        .expect("head mp")
        .expect("mp exists");
    assert_eq!(head.len, total);

    let (bytes, _meta) = c.get(mp_key, total + 1).await.expect("get mp");
    assert_eq!(bytes.len() as u64, total);
    assert!(bytes[..part_size].iter().all(|&b| b == 0xAB));
    assert!(bytes[part_size..].iter().all(|&b| b == 0xCD));

    c.delete(mp_key).await.expect("delete mp");

    // ---- abort path: create then abort, ensure object never materializes ----
    let abort_key = "aborted.bin";
    let up2 = c
        .create_multipart(abort_key, "application/octet-stream")
        .await
        .expect("create_multipart 2");
    let _ = c
        .upload_part(abort_key, &up2, 1, Bytes::from(vec![1u8; part_size]))
        .await
        .expect("upload part for abort");
    c.abort_multipart(abort_key, &up2).await.expect("abort");
    assert!(
        c.head_object(abort_key).await.expect("head aborted").is_none(),
        "aborted multipart should not produce an object"
    );

    // ---- H1: early-cancel of a streaming GET must not desync a later op ----
    h1_early_cancel_is_clean(&c).await;
}

/// H1 regression: open a streaming GET, read ONE chunk, drop the stream before
/// EOF (the V8 `cancelStream` path), then run another operation and assert it
/// succeeds. A dirty (half-read) keep-alive connection reused by the next op
/// would desync; the fresh-client-per-op + `KeepAlive` ownership guarantees it
/// can't. Also asserts the early drop does not hang.
async fn h1_early_cancel_is_clean(c: &S3Client) {
    use futures::StreamExt;

    // Store a multi-chunk object (> 1 MiB so the body arrives in several
    // network reads, making a *partial* read meaningful).
    let key = "stream-cancel.bin";
    let body = vec![0x5Au8; 2 * 1024 * 1024];
    c.put(
        key,
        &body,
        PutOptions { content_type: "application/octet-stream", ..Default::default() },
    )
    .await
    .expect("put stream-cancel object");

    {
        // Open the streaming GET and pull exactly one chunk, then drop.
        let (_meta, stream) = c.get_stream(key).await.expect("get_stream");
        futures::pin_mut!(stream);
        let first = stream.next().await;
        assert!(
            matches!(first, Some(Ok(_))),
            "H1: expected at least one streamed chunk, got {first:?}"
        );
        // `stream` (and the KeepAlive-owned client) drops here, mid-body.
    }

    // A fresh op on a brand-new client must be clean — no dirty-connection
    // desync, no hang.
    let meta = c
        .head_object(key)
        .await
        .expect("H1: head after early-cancel")
        .expect("H1: object still present");
    assert_eq!(meta.len, body.len() as u64, "H1: post-cancel head size");

    let (got, _m) = c
        .get(key, body.len() as u64 + 1)
        .await
        .expect("H1: full get after early-cancel must succeed");
    assert_eq!(got.len(), body.len(), "H1: post-cancel full get size");

    c.delete(key).await.expect("delete stream-cancel object");
}
