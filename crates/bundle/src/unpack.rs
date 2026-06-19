//! `.zship` ingestion. Streaming tar.zst → blob store + manifest.
//!
//! See `docs/reference/zship.md` for the wire format and the
//! ingestion algorithm. This is the content-addressed blob-store
//! ingestion path.
//!
//! # Streaming
//!
//! Decompressed-side peak memory is O(64 KiB chunks), independent of
//! bundle size. Each tar entry is read by the blob store one chunk at
//! a time via `BlobStore::put_blob_stream`, which hashes as it writes
//! and atomic-renames on a hash + size match.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::blob::{BlobError, BlobStore, PutOutcome};
use crate::limits::{
    MAX_BLOBS_PER_DEPLOY, MAX_BLOB_BYTES, MAX_DECOMPRESSED_BYTES, MAX_MANIFEST_BYTES,
};
use crate::manifest::Manifest;

// ---------------------------------------------------------------------------
// Result + error types
// ---------------------------------------------------------------------------

/// What the ingester returns to the HTTP layer.
#[derive(Debug)]
pub struct IngestSuccess {
    pub deploy_hash: String,
    pub manifest_json: String,
    pub blobs_uploaded: usize,
    pub blobs_deduped: usize,
}

/// Structured error, mapped to an HTTP status by the caller.
#[derive(Debug)]
pub enum IngestError {
    /// 400 Bad Request — malformed input (manifest, hashes, ordering, …).
    BadRequest { error: String, detail: String },
    /// 413 Payload Too Large — compressed or decompressed cap exceeded.
    TooLarge { cap_bytes: u64, observed_bytes: u64 },
    /// 415 Unsupported Media Type — Content-Type wasn't application/x-zship.
    UnsupportedMediaType,
    /// 503 Service Unavailable — blob store backend errored.
    BlobStoreUnavailable(String),
    /// 500 Internal — re-serializing the canonical manifest failed.
    Internal(String),
}

impl IngestError {
    fn bad(error: &str, detail: String) -> Self {
        Self::BadRequest {
            error: error.to_string(),
            detail,
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the streaming ingest over an already-buffered compressed body.
///
/// Caller is responsible for enforcing `MAX_COMPRESSED_BYTES` while
/// draining the request payload — this function takes the buffered
/// bytes as-is and decompresses them in-process.
///
/// Internally:
/// 1. zstd::Decoder over the compressed bytes,
/// 2. tar::Archive over the decoder,
/// 3. first entry must be `manifest.json` (capped at 1 MB),
/// 4. parse + validate the manifest,
/// 5. compute `deploy_hash` from the canonical, deploy_hash-omitted form,
/// 6. for each subsequent `blobs/<hash>` entry: stream into the blob
///    store one chunk at a time (hash-verified by the store, dedup
///    detected via the `PutOutcome` return),
/// 7. assert every referenced hash was present in the tar,
/// 8. write the manifest under `manifests/<app_id>/<deploy_hash>.json`,
/// 9. return the success record so the HTTP layer can update the DB.
pub async fn ingest(
    blob_store: &Arc<dyn BlobStore>,
    app_id: &Uuid,
    compressed: &[u8],
) -> Result<IngestSuccess, IngestError> {
    // Step 1-2: open the zstd → tar streaming pipeline.
    let cursor = std::io::Cursor::new(compressed);
    let decoder = zstd::Decoder::new(cursor)
        .map_err(|e| IngestError::bad("invalid zstd stream", format!("zstd init: {e}")))?;
    // Cap the decompressed flow so a malicious archive (zip-bomb-style)
    // can't blow past the decompressed-body limit even if its tar
    // entries individually look fine.
    let limited = std::io::Read::take(decoder, MAX_DECOMPRESSED_BYTES + 1);
    let mut archive = tar::Archive::new(limited);
    let mut entries = archive
        .entries()
        .map_err(|e| IngestError::bad("invalid tar", format!("entries(): {e}")))?;

    // Step 3: first entry must be manifest.json. Buffer it (small,
    // capped at MAX_MANIFEST_BYTES).
    let mut first = match entries.next() {
        Some(Ok(e)) => e,
        Some(Err(e)) => {
            return Err(IngestError::bad(
                "invalid tar",
                format!("first entry read: {e}"),
            ));
        }
        None => {
            return Err(IngestError::bad("empty archive", "no entries".to_string()));
        }
    };
    let first_path = first
        .path()
        .map_err(|e| IngestError::bad("invalid tar", format!("path: {e}")))?;
    if first_path.as_ref() != std::path::Path::new("manifest.json") {
        return Err(IngestError::bad(
            "manifest must be first tar entry",
            format!("got {}", first_path.display()),
        ));
    }
    let first_size = first.size();
    if first_size > MAX_MANIFEST_BYTES {
        return Err(IngestError::TooLarge {
            cap_bytes: MAX_MANIFEST_BYTES,
            observed_bytes: first_size,
        });
    }
    let mut manifest_bytes = Vec::with_capacity(first_size as usize);
    first
        .read_to_end(&mut manifest_bytes)
        .map_err(|e| IngestError::bad("invalid tar", format!("manifest read: {e}")))?;
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(IngestError::TooLarge {
            cap_bytes: MAX_MANIFEST_BYTES,
            observed_bytes: manifest_bytes.len() as u64,
        });
    }

    // Step 4: parse + validate the manifest.
    let mut manifest: Manifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| IngestError::bad("invalid manifest", format!("parse: {e}")))?;
    if manifest.version != 1 {
        return Err(IngestError::bad(
            "unsupported manifest version",
            format!(
                "version {} not supported: only version 1 is accepted",
                manifest.version
            ),
        ));
    }
    // Fresh-deploy invariants — see manifest spec.
    if !manifest.runtime_assets.is_empty() {
        return Err(IngestError::bad(
            "invalid manifest",
            "runtime_assets must be {} on fresh deploy".to_string(),
        ));
    }
    if manifest.asset_version != 0 {
        return Err(IngestError::bad(
            "invalid manifest",
            format!(
                "asset_version must be 0 on fresh deploy, got {}",
                manifest.asset_version
            ),
        ));
    }
    if manifest.metadata.built_at.is_empty() {
        return Err(IngestError::bad(
            "invalid manifest",
            "metadata.built_at is required".to_string(),
        ));
    }
    manifest
        .validate()
        .map_err(|e| IngestError::bad("invalid manifest", e))?;

    // Step 5: build the expected-hash set from the manifest.
    let expected = collect_expected_hashes(&manifest)?;

    // Step 6 (deploy_hash): compute deploy_hash from the canonical,
    // deploy_hash-omitted manifest.
    let canonical_omit = canonical_manifest_for_hash(&manifest_bytes)?;
    let deploy_hash = sha256_hex(&canonical_omit);

    // Step 7: stream subsequent entries directly into the blob store,
    // one at a time. The store hashes as it writes and rejects on
    // mismatch — we don't buffer the entry first.
    let mut blobs_uploaded = 0usize;
    let mut blobs_deduped = 0usize;
    let mut seen: HashSet<String> = HashSet::new();
    let mut blob_count: usize = 0;

    for entry_result in entries {
        let mut entry = entry_result
            .map_err(|e| IngestError::bad("invalid tar", format!("entry: {e}")))?;

        // Skip directory + PAX/global entries; we only care about regular files.
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let path = entry
            .path()
            .map_err(|e| IngestError::bad("invalid tar", format!("path: {e}")))?
            .to_path_buf();
        let path_str = path.to_str().ok_or_else(|| {
            IngestError::bad("invalid tar entry name", "non-UTF-8 path".to_string())
        })?;
        let hash = path_str
            .strip_prefix("blobs/")
            .ok_or_else(|| {
                IngestError::bad(
                    "unexpected tar entry",
                    format!("expected manifest.json or blobs/<hash>, got {path_str}"),
                )
            })?
            .to_string();
        if !crate::blob::validate_hash_format(&hash) {
            return Err(IngestError::bad(
                "invalid blob name",
                format!("entry blobs/{hash} is not a 64-char lowercase sha256"),
            ));
        }

        let size = entry.size();
        if size > MAX_BLOB_BYTES {
            return Err(IngestError::TooLarge {
                cap_bytes: MAX_BLOB_BYTES,
                observed_bytes: size,
            });
        }

        blob_count += 1;
        if blob_count > MAX_BLOBS_PER_DEPLOY {
            return Err(IngestError::bad(
                "too many blobs",
                format!("limit {MAX_BLOBS_PER_DEPLOY}"),
            ));
        }

        // Pipe the tar entry directly into the blob store. `entry`
        // implements `std::io::Read`; the store reads in 64 KiB chunks,
        // hashes as it goes, and atomic-renames on a match.
        match blob_store.put_blob_stream(&hash, size, &mut entry).await {
            Ok(PutOutcome::Wrote) => {
                blobs_uploaded += 1;
            }
            Ok(PutOutcome::Deduped) => {
                tracing::debug!(hash = %hash, "deploy: blob dedup hit");
                blobs_deduped += 1;
            }
            Err(BlobError::HashMismatch { expected, got }) => {
                return Err(IngestError::bad(
                    "blob hash mismatch",
                    format!("expected {expected}, got {got}"),
                ));
            }
            Err(e) => {
                return Err(IngestError::BlobStoreUnavailable(format!(
                    "put_blob_stream({hash}): {e}"
                )));
            }
        }
        seen.insert(hash);
    }

    // Step 8: assert every expected hash was present in the tar.
    for h in &expected {
        if !seen.contains(h) {
            return Err(IngestError::bad(
                "missing blob",
                format!("manifest references {h} but no tar entry blobs/{h}"),
            ));
        }
    }

    // Step 9: re-serialize the manifest with deploy_hash inserted, write
    // it to the blob store, and hand the canonical JSON back so the
    // caller can persist it inline on the apps row.
    manifest.deploy_hash = Some(deploy_hash.clone());
    let final_json = serde_json::to_string(&manifest)
        .map_err(|e| IngestError::Internal(format!("re-serialize manifest: {e}")))?;
    if let Err(e) = blob_store
        .put_manifest(app_id, &deploy_hash, final_json.as_bytes())
        .await
    {
        return Err(IngestError::BlobStoreUnavailable(format!(
            "put_manifest: {e}"
        )));
    }

    Ok(IngestSuccess {
        deploy_hash,
        manifest_json: final_json,
        blobs_uploaded,
        blobs_deduped,
    })
}

// ---------------------------------------------------------------------------
// Cross-references + deploy_hash canonicalization
// ---------------------------------------------------------------------------

/// Walk the manifest and gather every hash it references — worker
/// modules, asset hashes, and both keys+values of `sourcemaps`.
fn collect_expected_hashes(manifest: &Manifest) -> Result<HashSet<String>, IngestError> {
    let mut out: HashSet<String> = HashSet::new();
    if let Some(worker) = &manifest.worker {
        if !worker.modules.contains_key(&worker.entry) {
            return Err(IngestError::bad(
                "invalid manifest",
                format!(
                    "worker.entry {entry:?} is not in modules",
                    entry = worker.entry
                ),
            ));
        }
        for (spec, hash) in &worker.modules {
            if !crate::blob::validate_hash_format(hash) {
                return Err(IngestError::bad(
                    "invalid manifest",
                    format!("worker.modules[{spec}] {hash:?} is not lowercase sha256 hex"),
                ));
            }
            out.insert(hash.clone());
        }
    }
    for (path, entry) in &manifest.assets {
        if !crate::blob::validate_hash_format(&entry.hash) {
            return Err(IngestError::bad(
                "invalid manifest",
                format!(
                    "assets[{path}].hash {h:?} is not lowercase sha256 hex",
                    h = entry.hash
                ),
            ));
        }
        out.insert(entry.hash.clone());
    }
    for (k, v) in &manifest.sourcemaps {
        // Manifest::validate() already enforces sha256 hex on both — defence in depth.
        out.insert(k.clone());
        out.insert(v.clone());
    }
    // Migration file blobs (schema-authority §8): each carried migration's
    // body is content-addressed exactly like a worker module. `validate()`
    // already enforced the hash format + bare-filename safety; gather the
    // hashes so step 8 asserts every migration blob was present in the tar.
    for entry in &manifest.migrations {
        if !crate::blob::validate_hash_format(&entry.hash) {
            return Err(IngestError::bad(
                "invalid manifest",
                format!(
                    "migrations[{name}].hash {hash:?} is not lowercase sha256 hex",
                    name = entry.name,
                    hash = entry.hash
                ),
            ));
        }
        out.insert(entry.hash.clone());
    }
    Ok(out)
}

/// Build the canonical bytes used for `deploy_hash` computation:
/// the manifest with the `deploy_hash` field omitted, all object keys
/// sorted lexicographically, no insignificant whitespace, UTF-8.
///
/// We start from the raw bytes (rather than the typed `Manifest`) so
/// the canonicalization matches whatever fields the client included —
/// the build pipeline must produce the same canonical form to get a
/// reproducible deploy_hash, and stripping unknown fields would
/// silently break that invariant.
fn canonical_manifest_for_hash(manifest_bytes: &[u8]) -> Result<Vec<u8>, IngestError> {
    let mut value: Value = serde_json::from_slice(manifest_bytes)
        .map_err(|e| IngestError::bad("invalid manifest", format!("canonical parse: {e}")))?;
    if let Value::Object(map) = &mut value {
        map.remove("deploy_hash");
    } else {
        return Err(IngestError::bad(
            "invalid manifest",
            "top-level must be an object".to_string(),
        ));
    }
    let canonical = canonicalize_value(&value);
    serde_json::to_vec(&canonical)
        .map_err(|e| IngestError::Internal(format!("canonical serialize: {e}")))
}

/// Recursively rebuild a `Value` so every object's keys are sorted.
/// `serde_json::Map` preserves insertion order with the default
/// features; sorting up-front gives us the canonical form when
/// `serde_json::to_vec` walks it in order.
fn canonicalize_value(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let sorted: BTreeMap<String, Value> = map
                .iter()
                .map(|(k, vv)| (k.clone(), canonicalize_value(vv)))
                .collect();
            let mut out = serde_json::Map::with_capacity(sorted.len());
            for (k, vv) in sorted {
                out.insert(k, vv);
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize_value).collect()),
        other => other.clone(),
    }
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_sorts_keys_recursively() {
        let raw = br#"{"b":1,"a":{"y":2,"x":[3,{"q":4,"p":5}]}}"#;
        let canonical = canonical_manifest_for_hash(raw).unwrap();
        let s = std::str::from_utf8(&canonical).unwrap();
        assert_eq!(s, r#"{"a":{"x":[3,{"p":5,"q":4}],"y":2},"b":1}"#);
    }

    #[test]
    fn canonical_drops_deploy_hash_field() {
        let raw = br#"{"deploy_hash":"deadbeef","z":1,"a":2}"#;
        let canonical = canonical_manifest_for_hash(raw).unwrap();
        let s = std::str::from_utf8(&canonical).unwrap();
        assert_eq!(s, r#"{"a":2,"z":1}"#);
    }

    #[test]
    fn canonical_rejects_non_object_root() {
        let raw = br#"[1,2,3]"#;
        assert!(canonical_manifest_for_hash(raw).is_err());
    }

    #[test]
    fn collect_expected_walks_all_reference_sites() {
        let h = "a".repeat(64);
        let h2 = "b".repeat(64);
        let h3 = "c".repeat(64);
        let m: Manifest = serde_json::from_value(json!({
            "version": 2,
            "worker": {
                "entry": "index.js",
                "modules": { "index.js": h }
            },
            "rules": [],
            "assets": { "/index.html": {
                "hash": h2,
                "content_type": "text/html",
                "size": 0,
            }},
            "runtime_assets": {},
            "asset_version": 0,
            "sourcemaps": { h2.clone(): h3 },
            "metadata": { "built_at": "2026-04-29T00:00:00Z" }
        }))
        .unwrap();
        let set = collect_expected_hashes(&m).unwrap();
        assert!(set.contains(&"a".repeat(64)));
        assert!(set.contains(&"b".repeat(64)));
        assert!(set.contains(&"c".repeat(64)));
    }

    #[test]
    fn collect_expected_walks_migration_blobs() {
        // A manifest carrying two migration files: their blob hashes must be
        // in the expected set so ingest asserts they were present in the tar.
        let m1 = "e".repeat(64);
        let m2 = "f".repeat(64);
        let m: Manifest = serde_json::from_value(json!({
            "version": 1,
            "rules": [],
            "assets": {},
            "runtime_assets": {},
            "asset_version": 0,
            "sourcemaps": {},
            "migrations": [
                { "name": "V0001__create_users.sql", "hash": m1 },
                { "name": "V0002__add_index.sql", "hash": m2 },
            ],
            "metadata": { "built_at": "2026-04-29T00:00:00Z" }
        }))
        .unwrap();
        let set = collect_expected_hashes(&m).unwrap();
        assert!(set.contains(&"e".repeat(64)), "first migration blob expected");
        assert!(set.contains(&"f".repeat(64)), "second migration blob expected");
    }

    #[test]
    fn collect_expected_walks_worker_modules() {
        let entry_hash = "a".repeat(64);
        let lib_hash = "d".repeat(64);
        let m: Manifest = serde_json::from_value(json!({
            "version": 2,
            "worker": {
                "entry": "src/index.js",
                "modules": {
                    "src/index.js": entry_hash,
                    "src/lib.js": lib_hash,
                }
            },
            "rules": [],
            "assets": {},
            "runtime_assets": {},
            "asset_version": 0,
            "sourcemaps": {},
            "metadata": { "built_at": "2026-04-29T00:00:00Z" }
        }))
        .unwrap();
        let set = collect_expected_hashes(&m).unwrap();
        assert!(set.contains(&"a".repeat(64)));
        assert!(set.contains(&"d".repeat(64)));
        assert_eq!(set.len(), 2, "worker covers all module hashes");
    }
}
