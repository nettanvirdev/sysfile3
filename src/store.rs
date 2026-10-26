//! File contents in object storage, with a small in-memory cache in front.
//!
//! Every write goes to a fresh object (`{prefix}/{workspace}/{blob}`), and an object is never
//! rewritten. That is what makes the cache simple: an entry can only ever be right or absent,
//! so nothing is invalidated on write. Entries leave when they go unread for `cache_idle`
//! (each read renews them) or when memory is short, least-used first.

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, BoxStream, Stream, StreamExt, TryStreamExt};
use moka::future::Cache;
use object_store::path::Path;
use object_store::signer::{Method, Signer};
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::Config;
use crate::error::{Error, Result};

pub(crate) struct Blobs {
    store: Arc<dyn ObjectStore>,
    signer: Option<Arc<dyn Signer>>,
    cache: Cache<Uuid, Bytes>,
    prefix: String,
    cache_item_bytes: u64,
}

/// A stored blob: its id, size and SHA-256 when known.
pub(crate) struct Stored {
    pub id: Uuid,
    pub size: u64,
    pub sha256: Option<Vec<u8>>,
}

impl Blobs {
    pub fn new(store: Arc<dyn ObjectStore>, signer: Option<Arc<dyn Signer>>, cfg: &Config) -> Self {
        let cache = Cache::builder()
            .weigher(|_: &Uuid, v: &Bytes| u32::try_from(v.len()).unwrap_or(u32::MAX))
            .max_capacity(cfg.cache_bytes)
            .time_to_idle(cfg.cache_idle)
            .build();
        Blobs {
            store,
            signer,
            cache,
            prefix: cfg.key_prefix.trim_matches('/').to_string(),
            cache_item_bytes: cfg.cache_item_bytes,
        }
    }

    pub fn key(&self, ws: Uuid, blob: Uuid) -> Path {
        Path::from(format!("{}/{ws}/{blob}", self.prefix))
    }

    pub fn workspace_prefix(&self, ws: Uuid) -> Path {
        Path::from(format!("{}/{ws}", self.prefix))
    }

    pub fn root_prefix(&self) -> Path {
        Path::from(self.prefix.as_str())
    }

    pub fn store(&self) -> &Arc<dyn ObjectStore> {
        &self.store
    }

    /// `(workspace, blob)` from an object key, or `None` for anything sysfile3 did not write.
    pub fn parse_key(&self, key: &Path) -> Option<(Uuid, Uuid)> {
        let rest = key.as_ref().strip_prefix(self.prefix.as_str())?.strip_prefix('/')?;
        let (ws, blob) = rest.split_once('/')?;
        Some((ws.parse().ok()?, blob.parse().ok()?))
    }

    pub async fn put(&self, ws: Uuid, bytes: Bytes) -> Result<Stored> {
        let id = Uuid::now_v7();
        let sha256 = Sha256::digest(&bytes).to_vec();
        let size = bytes.len() as u64;
        self.store.put(&self.key(ws, id), PutPayload::from(bytes.clone())).await?;
        if size <= self.cache_item_bytes {
            self.cache.insert(id, bytes).await;
        }
        Ok(Stored { id, size, sha256: Some(sha256) })
    }

    /// Stream into a multipart upload, refusing more than `limit` bytes. Nothing is left behind
    /// in storage when the stream fails or runs over.
    pub async fn put_stream<S>(&self, ws: Uuid, mut input: S, limit: u64) -> Result<Stored>
    where
        S: Stream<Item = std::io::Result<Bytes>> + Unpin + Send,
    {
        let id = Uuid::now_v7();
        let upload = self.store.put_multipart(&self.key(ws, id)).await?;
        let mut writer = WriteMultipart::new(upload);
        let mut hasher = Sha256::new();
        let mut size: u64 = 0;
        let outcome: Result<()> = async {
            while let Some(chunk) = input.next().await {
                let chunk = chunk?;
                size += chunk.len() as u64;
                if size > limit {
                    return Err(Error::TooLarge { size, limit });
                }
                hasher.update(&chunk);
                writer.wait_for_capacity(8).await?;
                writer.put(chunk);
            }
            Ok(())
        }
        .await;
        if let Err(e) = outcome {
            let _ = writer.abort().await;
            return Err(e);
        }
        writer.finish().await?;
        Ok(Stored { id, size, sha256: Some(hasher.finalize().to_vec()) })
    }

    pub async fn get(&self, ws: Uuid, blob: Uuid, size: u64) -> Result<Bytes> {
        if let Some(hit) = self.cache.get(&blob).await {
            return Ok(hit);
        }
        let bytes = self.store.get(&self.key(ws, blob)).await?.bytes().await?;
        if size <= self.cache_item_bytes {
            self.cache.insert(blob, bytes.clone()).await;
        }
        Ok(bytes)
    }

    pub async fn get_range(&self, ws: Uuid, blob: Uuid, range: Range<u64>) -> Result<Bytes> {
        if let Some(hit) = self.cache.get(&blob).await {
            return Ok(hit.slice(range.start as usize..range.end as usize));
        }
        Ok(self.store.get_range(&self.key(ws, blob), range).await?)
    }

    pub async fn stream(&self, ws: Uuid, blob: Uuid) -> Result<BoxStream<'static, Result<Bytes>>> {
        if let Some(hit) = self.cache.get(&blob).await {
            return Ok(stream::once(async move { Ok(hit) }).boxed());
        }
        let got = self.store.get(&self.key(ws, blob)).await?;
        Ok(got.into_stream().map_err(Error::from).boxed())
    }

    /// The stored size, or `None` when the object does not exist.
    pub async fn head(&self, ws: Uuid, blob: Uuid) -> Result<Option<u64>> {
        match self.store.head(&self.key(ws, blob)).await {
            Ok(meta) => Ok(Some(meta.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Delete objects. Best effort: anything left behind is found later by `sweep_orphans`.
    pub async fn delete(&self, items: impl IntoIterator<Item = (Uuid, Uuid)>) {
        let mut keys = Vec::new();
        for (ws, blob) in items {
            self.cache.invalidate(&blob).await;
            keys.push(Ok(self.key(ws, blob)));
        }
        if keys.is_empty() {
            return;
        }
        let mut done = self.store.delete_stream(stream::iter(keys).boxed());
        while done.next().await.is_some() {}
    }

    pub async fn delete_keys(&self, keys: Vec<Path>) -> usize {
        let count = keys.len();
        let mut done = self.store.delete_stream(stream::iter(keys.into_iter().map(Ok)).boxed());
        while done.next().await.is_some() {}
        count
    }

    pub async fn signed_url(&self, method: Method, ws: Uuid, blob: Uuid, ttl: Duration) -> Result<String> {
        let signer = self.signer.as_ref().ok_or(Error::Unsupported("signed URLs need a signer"))?;
        Ok(signer.signed_url(method, &self.key(ws, blob), ttl).await?.to_string())
    }
}
