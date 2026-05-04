use zeroship_bundle::store::{BundleStore, LocalFs, VfsError};

fn temp_dir(suffix: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "zeroship-vfs-test-{}-{}",
        std::process::id(),
        suffix
    ))
}

#[test]
fn put_and_get() {
    let dir = temp_dir("put_and_get");
    let store = LocalFs::new(&dir).unwrap();

    store.put("app1", b"hello world").unwrap();
    let data = store.get("app1").unwrap();
    assert_eq!(data, b"hello world");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn get_nonexistent() {
    let dir = temp_dir("get_nonexistent");
    let store = LocalFs::new(&dir).unwrap();

    let result = store.get("no-such-app");
    assert!(
        matches!(result, Err(VfsError::NotFound(ref id)) if id == "no-such-app"),
        "expected NotFound, got: {:?}",
        result
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn exists() {
    let dir = temp_dir("exists");
    let store = LocalFs::new(&dir).unwrap();

    assert_eq!(store.exists("app1").unwrap(), false);
    store.put("app1", b"data").unwrap();
    assert_eq!(store.exists("app1").unwrap(), true);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn delete() {
    let dir = temp_dir("delete");
    let store = LocalFs::new(&dir).unwrap();

    store.put("app1", b"data").unwrap();
    assert_eq!(store.exists("app1").unwrap(), true);

    store.delete("app1").unwrap();
    assert_eq!(store.exists("app1").unwrap(), false);

    // Second delete should return NotFound.
    let result = store.delete("app1");
    assert!(
        matches!(result, Err(VfsError::NotFound(_))),
        "expected NotFound on second delete, got: {:?}",
        result
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn overwrite() {
    let dir = temp_dir("overwrite");
    let store = LocalFs::new(&dir).unwrap();

    store.put("app1", b"version1").unwrap();
    store.put("app1", b"version2").unwrap();
    let data = store.get("app1").unwrap();
    assert_eq!(data, b"version2");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn multiple_apps_isolated() {
    let dir = temp_dir("multi_apps");
    let store = LocalFs::new(&dir).unwrap();

    store.put("app-a", b"bundle-a").unwrap();
    store.put("app-b", b"bundle-b").unwrap();

    // Delete app-a; app-b must still be intact.
    store.delete("app-a").unwrap();

    assert_eq!(store.exists("app-a").unwrap(), false);
    assert_eq!(store.exists("app-b").unwrap(), true);
    assert_eq!(store.get("app-b").unwrap(), b"bundle-b");

    std::fs::remove_dir_all(&dir).ok();
}
