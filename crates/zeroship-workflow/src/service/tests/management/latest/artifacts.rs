use super::*;
use std::{path::PathBuf, sync::Mutex};
use zeroship_bundle::{BlobError, BlobStore, PutOutcome};

#[derive(Debug)]
pub(super) struct Gate {
    entered: flume::Sender<()>,
    resume: flume::Receiver<()>,
}

pub(super) struct Probe {
    pub entered: flume::Receiver<()>,
    pub resume: flume::Sender<()>,
}

#[derive(Debug)]
pub(super) struct Artifacts {
    pub inner: Arc<dyn BlobStore>,
    gate: Mutex<Option<Gate>>,
}
impl Artifacts {
    pub fn new(inner: Arc<dyn BlobStore>) -> Self {
        Self {
            inner,
            gate: Mutex::new(None),
        }
    }
    pub fn block(&self) -> Probe {
        let (entered, observed) = flume::bounded(1);
        let (resume, waiting) = flume::bounded(1);
        assert!(self
            .gate
            .lock()
            .unwrap()
            .replace(Gate {
                entered,
                resume: waiting
            })
            .is_none());
        Probe {
            entered: observed,
            resume,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl BlobStore for Artifacts {
    async fn get_blob(&self, hash: &str) -> Result<bytes::Bytes, BlobError> {
        self.inner.get_blob(hash).await
    }
    fn local_path(&self, hash: &str) -> Option<PathBuf> {
        self.inner.local_path(hash)
    }
    async fn put_blob_stream(
        &self,
        hash: &str,
        size: u64,
        reader: &mut dyn std::io::Read,
    ) -> Result<PutOutcome, BlobError> {
        self.inner.put_blob_stream(hash, size, reader).await
    }
    async fn has_blob(&self, hash: &str) -> Result<bool, BlobError> {
        self.inner.has_blob(hash).await
    }
    async fn probe(&self) -> Result<(), BlobError> {
        self.inner.probe().await
    }
    async fn get_blob_to_file(
        &self,
        hash: &str,
        out: &compio::fs::File,
        size: Option<u64>,
        max: u64,
    ) -> Result<u64, BlobError> {
        self.inner.get_blob_to_file(hash, out, size, max).await
    }
    async fn put_manifest(&self, app: &AppId, hash: &str, bytes: &[u8]) -> Result<(), BlobError> {
        self.inner.put_manifest(app, hash, bytes).await
    }
    async fn get_manifest(&self, app: &AppId, hash: &str) -> Result<bytes::Bytes, BlobError> {
        let bytes = self.inner.get_manifest(app, hash).await?;
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.entered.send_async(()).await.unwrap();
            gate.resume.recv_async().await.unwrap();
        }
        Ok(bytes)
    }
    async fn delete_manifest(&self, app: &AppId, hash: &str) -> Result<bool, BlobError> {
        self.inner.delete_manifest(app, hash).await
    }
    async fn delete_app_manifests(&self, app: &AppId) -> Result<(), BlobError> {
        self.inner.delete_app_manifests(app).await
    }
}
