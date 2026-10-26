//! Large files: streamed through the server, or sent by the client straight to object storage.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::Stream;
use object_store::signer::Method;
use uuid::Uuid;

use crate::error::{Error, Quota, Result};
use crate::model::{Entry, UploadTicket};
use crate::path::VPath;
use crate::store::Stored;
use crate::workspace::{Content, Expect, Pending, Workspace, WriteOptions};

impl Workspace {
    /// Write a file of any size (up to `Config::max_file_bytes`) from a stream of chunks. The
    /// bytes go to object storage as they arrive; nothing is kept in memory.
    pub async fn write_stream<S>(&self, path: &str, input: S, opts: WriteOptions) -> Result<Entry>
    where
        S: Stream<Item = std::io::Result<Bytes>> + Unpin + Send,
    {
        self.need_write()?;
        let abs = self.abs(path)?;
        self.precheck(&abs, 0).await?;
        let usage = self.usage().await?;
        let max = self.inner.cfg.max_file_bytes;
        let room = usage.quota_bytes.map(|q| q.saturating_sub(usage.used_bytes)).unwrap_or(u64::MAX);
        let stored = match self.inner.blobs.put_stream(self.id, input, max.min(room)).await {
            Err(Error::TooLarge { .. }) if room < max => return Err(Error::QuotaExceeded(Quota::Bytes)),
            other => other?,
        };
        let pending = Pending { abs, content: Content::New(stored), mime: opts.mime, expect: opts.expect };
        Ok(self.commit(vec![pending]).await?.remove(0))
    }

    /// Reserve an upload of exactly `size` bytes: the client PUTs them to the returned URL, then
    /// you call `finish_upload`. Needs a signer. Unfinished uploads expire after
    /// `Config::upload_ttl` and are cleaned up by `Sysfile3::expire_uploads`.
    pub async fn begin_upload(&self, path: &str, size: u64, opts: WriteOptions) -> Result<UploadTicket> {
        self.need_write()?;
        let abs = self.abs(path)?;
        let max = self.inner.cfg.max_file_bytes;
        if size > max {
            return Err(Error::TooLarge { size, limit: max });
        }
        self.precheck(&abs, size).await?;
        match self.node(&abs).await? {
            Some(row) if row.is_dir() => return Err(Error::IsADirectory(self.shown(&abs))),
            Some(_) if opts.expect == Expect::Absent => return Err(Error::AlreadyExists(self.shown(&abs))),
            _ => {}
        }
        let blob = Uuid::now_v7();
        let ttl = self.inner.cfg.upload_ttl;
        let url = self.inner.blobs.signed_url(Method::PUT, self.id, blob, ttl).await?;
        let id = Uuid::now_v7();
        let mut tx = self.tx().await?;
        let (expires_at,): (DateTime<Utc>,) = sqlx::query_as(
            "INSERT INTO sf3_uploads (id, workspace_id, path, blob_id, size, mime, created_by, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(secs => $8))
             RETURNING expires_at",
        )
        .bind(id)
        .bind(self.id)
        .bind(abs.as_str())
        .bind(blob)
        .bind(size as i64)
        .bind(opts.mime.as_deref())
        .bind(self.access.actor.as_deref())
        .bind(ttl.as_secs_f64())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(UploadTicket { id, url, size, expires_at })
    }

    /// Commit an upload once the client has sent the bytes. The stored object must be exactly
    /// the size that was reserved. Each ticket can be finished once.
    pub async fn finish_upload(&self, upload_id: Uuid, expect: Expect) -> Result<Entry> {
        self.need_write()?;
        let (path, blob, size, mime) = self.claim_upload(upload_id, true).await?;
        let abs = VPath::trusted(path);
        match self.inner.blobs.head(self.id, blob).await? {
            Some(actual) if actual == size => {}
            found => {
                self.inner.blobs.delete([(self.id, blob)]).await;
                return Err(Error::InvalidInput(match found {
                    None => "nothing was uploaded".into(),
                    Some(actual) => format!("uploaded {actual} bytes, expected {size}"),
                }));
            }
        }
        let stored = Stored { id: blob, size, sha256: None };
        let pending = Pending { abs, content: Content::New(stored), mime, expect };
        Ok(self.commit(vec![pending]).await?.remove(0))
    }

    /// Abandon an upload and delete whatever was sent.
    pub async fn cancel_upload(&self, upload_id: Uuid) -> Result<()> {
        self.need_write()?;
        let (_, blob, _, _) = self.claim_upload(upload_id, false).await?;
        self.inner.blobs.delete([(self.id, blob)]).await;
        Ok(())
    }

    /// Take the upload row out of the table, so no two calls can finish one ticket.
    async fn claim_upload(&self, id: Uuid, live_only: bool) -> Result<(String, Uuid, u64, Option<String>)> {
        let mut tx = self.tx().await?;
        let row: Option<(String, Uuid, i64, Option<String>, bool)> = sqlx::query_as(
            "DELETE FROM sf3_uploads WHERE workspace_id = $1 AND id = $2
             RETURNING path, blob_id, size, mime, expires_at > now()",
        )
        .bind(self.id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let not_found = || Error::NotFound(format!("upload {id}"));
        let (path, blob, size, mime, live) = row.ok_or_else(not_found)?;
        if !VPath::trusted(path.clone()).within(&self.access.root) {
            return Err(not_found());
        }
        tx.commit().await?;
        if live_only && !live {
            self.inner.blobs.delete([(self.id, blob)]).await;
            return Err(Error::InvalidInput("this upload has expired".into()));
        }
        Ok((path, blob, size.max(0) as u64, mime))
    }
}
