//! Workflow output blob store.
//!
//! Workflow blobs live in the `wfblob/` namespace. That namespace is separate
//! from deploy bundle blobs and has its own reference table plus GC policy.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bytes::Bytes;
use compio_s3::{PutOptions, S3Client, S3Config, S3Credentials};
use sha2::Digest;

use crate::blob::{sha256_hex, validate_hash_format, BlobError};

const LOCAL_WORKFLOW_BLOB_DIR: &str = "wfblob";
const REMOTE_WORKFLOW_BLOB_PREFIX: &str = "wfblob/";

#[derive(Debug, Clone)]
pub struct WorkflowBlobEntry {
    pub hash: String,
    pub size: u64,
    pub last_modified: SystemTime,
}

#[async_trait::async_trait(?Send)]
pub trait WorkflowBlobStore: Send + Sync + std::fmt::Debug {
    async fn get_blob(&self, hash: &str) -> Result<Bytes, BlobError>;

    async fn put_blob(&self, hash: &str, data: &[u8]) -> Result<(), BlobError> {
        let mut cursor = std::io::Cursor::new(data);
        self.put_blob_stream(hash, data.len() as u64, &mut cursor)
            .await
    }

    async fn put_blob_stream(
        &self,
        hash: &str,
        expected_size: u64,
        reader: &mut dyn std::io::Read,
    ) -> Result<(), BlobError>;

    async fn delete_blob(&self, hash: &str) -> Result<(), BlobError>;

    async fn list_blobs(&self) -> Result<Vec<WorkflowBlobEntry>, BlobError>;
}

#[derive(Debug, Clone)]
pub struct LocalWorkflowBlobStore {
    root: PathBuf,
}

impl LocalWorkflowBlobStore {
    pub fn new(root: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(root.join(LOCAL_WORKFLOW_BLOB_DIR))?;
        Ok(Self { root })
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        let (shard, rest) = hash.split_at(2);
        self.root
            .join(LOCAL_WORKFLOW_BLOB_DIR)
            .join(shard)
            .join(rest)
    }

    fn root_path(&self) -> PathBuf {
        self.root.join(LOCAL_WORKFLOW_BLOB_DIR)
    }
}

#[async_trait::async_trait(?Send)]
impl WorkflowBlobStore for LocalWorkflowBlobStore {
    async fn get_blob(&self, hash: &str) -> Result<Bytes, BlobError> {
        if !validate_hash_format(hash) {
            return Err(malformed_hash(hash));
        }
        let path = self.blob_path(hash);
        let data = match compio::fs::read(&path).await {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BlobError::NotFound(format!("wfblob:{hash}")));
            }
            Err(e) => return Err(BlobError::Io(e)),
        };
        let actual = sha256_hex(&data);
        if actual != hash {
            return Err(BlobError::HashMismatch {
                expected: hash.to_string(),
                got: actual,
            });
        }
        Ok(Bytes::from(data))
    }

    async fn put_blob_stream(
        &self,
        hash: &str,
        expected_size: u64,
        reader: &mut dyn std::io::Read,
    ) -> Result<(), BlobError> {
        if !validate_hash_format(hash) {
            return Err(malformed_hash(hash));
        }
        let path = self.blob_path(hash);
        if let Some(parent) = path.parent() {
            compio::fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
        let file = compio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await?;

        let result = write_verified_stream(hash, expected_size, reader, &file).await;
        drop(file);

        match result {
            Ok(()) => {
                compio::fs::rename(&tmp, &path).await?;
                Ok(())
            }
            Err(e) => {
                let _ = compio::fs::remove_file(&tmp).await;
                Err(e)
            }
        }
    }

    async fn delete_blob(&self, hash: &str) -> Result<(), BlobError> {
        if !validate_hash_format(hash) {
            return Err(malformed_hash(hash));
        }
        let path = self.blob_path(hash);
        match compio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(BlobError::Io(e)),
        }
    }

    async fn list_blobs(&self) -> Result<Vec<WorkflowBlobEntry>, BlobError> {
        let mut out = Vec::new();
        let root = self.root_path();
        if !root.exists() {
            return Ok(out);
        }
        for shard in std::fs::read_dir(&root).map_err(BlobError::Io)? {
            let shard = shard.map_err(BlobError::Io)?;
            let shard_name = shard.file_name();
            let shard_name = match shard_name.to_str() {
                Some(name) if name.len() == 2 => name.to_string(),
                _ => continue,
            };
            let shard_path = shard.path();
            if !shard_path.is_dir() {
                continue;
            }
            list_local_shard(&mut out, &shard_name, &shard_path)?;
        }
        out.sort_by(|a, b| a.hash.cmp(&b.hash));
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct RemoteWorkflowBlobStore {
    client: S3Client,
}

impl RemoteWorkflowBlobStore {
    #[must_use]
    pub fn new(config: S3Config, credentials: S3Credentials) -> Self {
        Self {
            client: S3Client::new(config, credentials),
        }
    }

    #[must_use]
    pub const fn from_client(client: S3Client) -> Self {
        Self { client }
    }

    fn key(hash: &str) -> String {
        format!("{REMOTE_WORKFLOW_BLOB_PREFIX}{hash}")
    }
}

#[async_trait::async_trait(?Send)]
impl WorkflowBlobStore for RemoteWorkflowBlobStore {
    async fn get_blob(&self, hash: &str) -> Result<Bytes, BlobError> {
        if !validate_hash_format(hash) {
            return Err(malformed_hash(hash));
        }
        let (bytes, _) = self
            .client
            .get(&Self::key(hash), u64::MAX)
            .await
            .map_err(remote_error)?;
        let actual = sha256_hex(&bytes);
        if actual != hash {
            return Err(BlobError::HashMismatch {
                expected: hash.to_string(),
                got: actual,
            });
        }
        Ok(bytes)
    }

    async fn put_blob_stream(
        &self,
        hash: &str,
        expected_size: u64,
        reader: &mut dyn std::io::Read,
    ) -> Result<(), BlobError> {
        if !validate_hash_format(hash) {
            return Err(malformed_hash(hash));
        }
        let mut data = Vec::with_capacity(expected_size.min(1024 * 1024) as usize);
        let mut hasher = sha2::Sha256::new();
        let mut total: u64 = 0;
        let mut scratch = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut scratch).map_err(BlobError::Io)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > expected_size {
                return Err(BlobError::Backend(format!(
                    "workflow blob exceeds declared size {expected_size}"
                )));
            }
            hasher.update(&scratch[..n]);
            data.extend_from_slice(&scratch[..n]);
        }
        if total != expected_size {
            return Err(BlobError::Backend(format!(
                "workflow blob size mismatch: expected {expected_size}, observed {total}"
            )));
        }
        let computed = hex::encode(hasher.finalize());
        if computed != hash {
            return Err(BlobError::HashMismatch {
                expected: hash.to_string(),
                got: computed,
            });
        }
        self.client
            .put(
                &Self::key(hash),
                &data,
                PutOptions {
                    content_type: "application/octet-stream",
                    if_none_match: false,
                    user_meta: &[("sha256", hash.to_string())],
                    cache_control: None,
                },
            )
            .await
            .map_err(remote_error)?;
        Ok(())
    }

    async fn delete_blob(&self, hash: &str) -> Result<(), BlobError> {
        if !validate_hash_format(hash) {
            return Err(malformed_hash(hash));
        }
        self.client
            .delete(&Self::key(hash))
            .await
            .map_err(remote_error)
    }

    async fn list_blobs(&self) -> Result<Vec<WorkflowBlobEntry>, BlobError> {
        let mut out = Vec::new();
        for entry in self
            .client
            .list(REMOTE_WORKFLOW_BLOB_PREFIX)
            .await
            .map_err(remote_error)?
        {
            let Some(hash) = entry.key.strip_prefix(REMOTE_WORKFLOW_BLOB_PREFIX) else {
                continue;
            };
            if !validate_hash_format(hash) {
                continue;
            }
            out.push(WorkflowBlobEntry {
                hash: hash.to_string(),
                size: entry.size,
                last_modified: entry.last_modified,
            });
        }
        out.sort_by(|a, b| a.hash.cmp(&b.hash));
        Ok(out)
    }
}

async fn write_verified_stream(
    hash: &str,
    expected_size: u64,
    reader: &mut dyn std::io::Read,
    file: &compio::fs::File,
) -> Result<(), BlobError> {
    use compio::io::AsyncWriteAtExt;

    let mut hasher = sha2::Sha256::new();
    let mut total: u64 = 0;
    let mut offset: u64 = 0;
    let mut scratch = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut scratch).map_err(BlobError::Io)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > expected_size {
            return Err(BlobError::Backend(format!(
                "workflow blob exceeds declared size {expected_size}"
            )));
        }
        hasher.update(&scratch[..n]);
        let mut chunk = Vec::with_capacity(n);
        chunk.extend_from_slice(&scratch[..n]);
        let mut wref: &compio::fs::File = file;
        let compio::BufResult(res, _) = wref.write_all_at(chunk, offset).await;
        res.map_err(BlobError::Io)?;
        offset += n as u64;
    }
    if total != expected_size {
        return Err(BlobError::Backend(format!(
            "workflow blob size mismatch: expected {expected_size}, observed {total}"
        )));
    }
    let computed = hex::encode(hasher.finalize());
    if computed != hash {
        return Err(BlobError::HashMismatch {
            expected: hash.to_string(),
            got: computed,
        });
    }
    file.sync_all().await?;
    Ok(())
}

fn list_local_shard(
    out: &mut Vec<WorkflowBlobEntry>,
    shard_name: &str,
    shard_path: &Path,
) -> Result<(), BlobError> {
    for entry in std::fs::read_dir(shard_path).map_err(BlobError::Io)? {
        let entry = entry.map_err(BlobError::Io)?;
        let name = entry.file_name();
        let Some(rest) = name.to_str() else {
            continue;
        };
        let hash = format!("{shard_name}{rest}");
        if !validate_hash_format(&hash) {
            continue;
        }
        let meta = entry.metadata().map_err(BlobError::Io)?;
        if !meta.is_file() {
            continue;
        }
        out.push(WorkflowBlobEntry {
            hash,
            size: meta.len(),
            last_modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    Ok(())
}

fn malformed_hash(hash: &str) -> BlobError {
    BlobError::Backend(format!(
        "malformed workflow blob hash {hash:?}: expected 64-char lowercase hex"
    ))
}

fn remote_error(err: compio_s3::S3Error) -> BlobError {
    BlobError::Backend(err.to_string())
}
