//! `MinIO`-backed integration smoke test for `compio-s3`.
//!
//! This test is **self-contained**: it starts its own `MinIO` container, creates
//! a bucket, exercises the full v1 client surface (put / get / head / list /
//! delete plus a multipart round-trip with parts ≥ 5 `MiB`), and tears the
//! container down. It **skips cleanly** when Docker is unavailable so CI / dev
//! machines without Docker stay green.

#![allow(clippy::future_not_send)]
//!
//! Run explicitly:
//!   `cargo test -p compio-s3 --test minio_smoke -- --nocapture`
//!
//! The container is path-style, plaintext HTTP on loopback (`dev_http=true`),
//! which is the only mode `MinIO` is wired for here.

use std::process::Command;
use std::time::Duration;

use bytes::Bytes;
use compio_s3::client::{PutOptions, S3Client};
use compio_s3::{S3Config, S3Credentials};

const ACCESS_KEY: &str = "minioadmin";
const SECRET_KEY: &str = "minioadmin";
const CONTAINER: &str = "zs-compio-s3-minio-test";
const PORT: u16 = 9111;
const BUCKET: &str = "zs-test-bucket";

/// Whether `docker` is on PATH and the daemon answers.
fn docker_available() -> bool {
    Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Best-effort `docker rm -f` of any prior run.
fn cleanup() {
    let _ = Command::new("docker")
        .args(["rm", "-f", CONTAINER])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Start `MinIO` and create the test bucket via the bundled `mc` client.
/// Returns `true` on success.
fn start_minio() -> bool {
    cleanup();
    let run = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            CONTAINER,
            "-p",
            &format!("{PORT}:9000"),
            "-e",
            &format!("MINIO_ROOT_USER={ACCESS_KEY}"),
            "-e",
            &format!("MINIO_ROOT_PASSWORD={SECRET_KEY}"),
            "minio/minio",
            "server",
            "/data",
        ])
        .status();
    if !matches!(run, Ok(s) if s.success()) {
        eprintln!("skip: failed to start MinIO container");
        return false;
    }

    // Wait for readiness, then create the bucket using `mc` inside the
    // container (avoids needing the AWS CLI on the host).
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(500));
        let alias = Command::new("docker")
            .args([
                "exec",
                CONTAINER,
                "mc",
                "alias",
                "set",
                "local",
                "http://127.0.0.1:9000",
                ACCESS_KEY,
                SECRET_KEY,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if matches!(alias, Ok(s) if s.success()) {
            let mb = Command::new("docker")
                .args([
                    "exec",
                    CONTAINER,
                    "mc",
                    "mb",
                    "-p",
                    &format!("local/{BUCKET}"),
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            if matches!(mb, Ok(s) if s.success()) {
                return true;
            }
        }
    }
    eprintln!("skip: MinIO did not become ready / bucket create failed");
    cleanup();
    false
}

fn client() -> S3Client {
    let url = format!(
        "s3://{BUCKET}/it?provider=minio&endpoint=http://127.0.0.1:{PORT}&region=us-east-1&style=path&dev_http=true&checksum=none"
    );
    let cfg = S3Config::parse_url(&url).expect("parse minio url");
    S3Client::new(cfg, S3Credentials::new(ACCESS_KEY, SECRET_KEY, None))
}

#[test]
fn minio_full_surface_and_multipart() {
    if !docker_available() {
        eprintln!("skip: docker unavailable");
        return;
    }
    if !start_minio() {
        return;
    }

    // The whole async body runs on a single compio thread; tear down the
    // container at the end regardless of outcome.
    let result = std::panic::catch_unwind(|| {
        compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(run_smoke());
    });

    cleanup();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

async fn run_smoke() {
    let c = client();

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
}
