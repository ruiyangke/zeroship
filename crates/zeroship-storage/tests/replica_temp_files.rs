//! Writers in other processes sharing one `LocalFs` root.
//!
//! Worker replicas share the local storage root (the `app-storage` volume), and
//! each runs in its own container, where process ids start over: two replicas
//! can hold the same pid. A temp file named from the pid and a per-process
//! counter then names the same file in both, and a writer that creates its temp
//! by truncating takes over the other's in-flight bytes.
//!
//! This file holds one test so its process makes no other write before it: the
//! names it plants are the ones a pid-and-counter scheme gives this process's
//! first writes, which is exactly what a freshly started replica with the same
//! pid is using.

#![allow(clippy::future_not_send)]

use zeroship_storage::backend::{Backend, LocalFs};

const APP: &str = "app_replica";
const BUCKET: &str = "uploads";
const KEY: &str = "report.bin";

/// Another replica's temp files, in flight beside the key this process writes,
/// are neither truncated nor published over the key.
#[test]
fn another_replicas_temp_files_beside_a_key_are_left_alone() {
    let root = tempfile::tempdir().expect("storage root");
    let bucket = root.path().join(APP).join(BUCKET);
    // The layout `LocalFs` documents: object bytes under `o/`, metadata under `m/`.
    let dirs = [bucket.join("o"), bucket.join("m")];
    let pid = std::process::id();
    let mut planted = Vec::new();
    for dir in &dirs {
        std::fs::create_dir_all(dir).expect("bucket directory");
        for n in 0..16 {
            let path = dir.join(format!(".{KEY}.tmp.{pid}.{n}"));
            let body = format!("another replica's write {n}");
            std::fs::write(&path, &body).expect("plant a temp file");
            planted.push((path, body));
        }
    }

    let written = compio::runtime::Runtime::new().expect("runtime").block_on(async {
        let be = LocalFs::new(root.path());
        be.put(APP, BUCKET, KEY, b"this replica's bytes", Some("application/octet-stream"))
            .await
            .expect("put");
        be.get(APP, BUCKET, KEY, 1024).await.expect("get")
    });

    let (bytes, _) = written.expect("the object exists");
    assert_eq!(bytes, b"this replica's bytes".to_vec());
    for (path, body) in &planted {
        assert_eq!(
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!(
                "another replica's temp file {} is gone: {e}",
                path.display()
            )),
            *body,
            "another replica's temp file {} was overwritten",
            path.display()
        );
    }
}
