//! Operations for the app and its scheduled jobs, not for end users: quotas, usage for
//! billing, deleting a workspace, and cleaning up.

use std::collections::HashSet;
use std::time::Duration;

use futures::TryStreamExt;
use object_store::ObjectStore;
use uuid::Uuid;

use crate::Sysfile3;
use crate::db::{self, Scope};
use crate::error::{Error, Result};
use crate::model::Usage;
use crate::tree::purge_in;
use crate::workspace::usage_of;

impl Sysfile3 {
    /// Set a workspace's limits; `None` is unlimited. Lowering a limit below current usage is
    /// allowed: nothing is deleted, but new writes are refused until usage drops.
    pub async fn set_quota(
        &self,
        app: &str,
        external_id: &str,
        bytes: Option<u64>,
        nodes: Option<u64>,
    ) -> Result<Usage> {
        let id = self.workspace_id(app, external_id).await?;
        let mut tx = db::begin(&self.inner.pool, Scope::Workspace(id)).await?;
        sqlx::query("UPDATE sf3_workspaces SET quota_bytes = $2, quota_nodes = $3 WHERE id = $1")
            .bind(id)
            .bind(bytes.map(|b| b as i64))
            .bind(nodes.map(|n| n as i64))
            .execute(&mut *tx)
            .await?;
        let usage = usage_of(&mut tx, id).await?;
        tx.commit().await?;
        Ok(usage)
    }

    pub async fn usage(&self, app: &str, external_id: &str) -> Result<Usage> {
        let id = self.workspace_id(app, external_id).await?;
        let mut tx = db::begin(&self.inner.pool, Scope::Workspace(id)).await?;
        let usage = usage_of(&mut tx, id).await?;
        tx.commit().await?;
        Ok(usage)
    }

    /// Usage of every workspace of an app, by external id, a page at a time (for billing).
    pub async fn usage_report(&self, app: &str, after: Option<&str>, limit: usize) -> Result<Vec<(String, Usage)>> {
        let mut tx = db::begin(&self.inner.pool, Scope::All).await?;
        let rows: Vec<(String, i64, i64, Option<i64>, Option<i64>)> = sqlx::query_as(
            "SELECT external_id, used_bytes, node_count, quota_bytes, quota_nodes FROM sf3_workspaces
             WHERE app = $1 AND ($2::text IS NULL OR external_id > $2)
             ORDER BY external_id LIMIT $3",
        )
        .bind(app)
        .bind(after)
        .bind(limit.clamp(1, 10_000) as i64)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|(ext, used, nodes, qb, qn)| {
                let usage = Usage {
                    used_bytes: used.max(0) as u64,
                    node_count: nodes.max(0) as u64,
                    quota_bytes: qb.map(|q| q as u64),
                    quota_nodes: qn.map(|q| q as u64),
                };
                (ext, usage)
            })
            .collect())
    }

    /// Delete a workspace and everything in it, for good (an account deletion). Returns the
    /// number of objects deleted from storage.
    pub async fn purge_workspace(&self, app: &str, external_id: &str) -> Result<usize> {
        let mut tx = db::begin(&self.inner.pool, Scope::All).await?;
        let id: Option<Uuid> =
            sqlx::query_scalar("DELETE FROM sf3_workspaces WHERE app = $1 AND external_id = $2 RETURNING id")
                .bind(app)
                .bind(external_id)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        self.inner.ids.invalidate(&(app.to_string(), external_id.to_string())).await;
        let Some(id) = id else { return Ok(0) };
        let blobs = &self.inner.blobs;
        let keys: Vec<_> =
            blobs.store().list(Some(&blobs.workspace_prefix(id))).map_ok(|meta| meta.location).try_collect().await?;
        Ok(blobs.delete_keys(keys).await)
    }

    /// Purge trash items deleted more than `age` ago, in every workspace. Returns how many.
    pub async fn purge_trash_older_than(&self, age: Duration) -> Result<usize> {
        let mut purged = 0;
        loop {
            let mut tx = db::begin(&self.inner.pool, Scope::All).await?;
            let batch: Vec<(Uuid, Uuid)> = sqlx::query_as(
                "SELECT workspace_id, id FROM sf3_trash
                 WHERE trashed_at < now() - make_interval(secs => $1) ORDER BY trashed_at LIMIT 500",
            )
            .bind(age.as_secs_f64())
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            if batch.is_empty() {
                return Ok(purged);
            }
            for (ws, trash_id) in batch {
                let mut tx = db::begin(&self.inner.pool, Scope::Workspace(ws)).await?;
                let freed = purge_in(&mut tx, ws, trash_id).await?;
                tx.commit().await?;
                self.inner.blobs.delete(freed).await;
                purged += 1;
            }
        }
    }

    /// Delete expired direct uploads and whatever their clients sent. Returns how many.
    pub async fn expire_uploads(&self) -> Result<usize> {
        let mut tx = db::begin(&self.inner.pool, Scope::All).await?;
        let gone: Vec<(Uuid, Uuid)> =
            sqlx::query_as("DELETE FROM sf3_uploads WHERE expires_at < now() RETURNING workspace_id, blob_id")
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        let count = gone.len();
        self.inner.blobs.delete(gone).await;
        Ok(count)
    }

    /// Delete objects older than `min_age` that no file, version or pending upload points at:
    /// leftovers of crashes between an upload and its commit, or of failed deletes. Objects
    /// sysfile3 did not write (other key shapes) are never touched. Returns how many.
    pub async fn sweep_orphans(&self, min_age: Duration) -> Result<usize> {
        let blobs = &self.inner.blobs;
        let cutoff = chrono::Utc::now()
            - chrono::Duration::from_std(min_age).map_err(|_| Error::InvalidInput("age too large".into()))?;
        let mut listing = blobs.store().list(Some(&blobs.root_prefix()));
        let mut swept = 0;
        let mut batch = Vec::new();
        loop {
            let next = listing.try_next().await?;
            if let Some(meta) = &next
                && meta.last_modified < cutoff
                && let Some(ids) = blobs.parse_key(&meta.location)
            {
                batch.push((ids, meta.location.clone()));
            }
            if batch.len() >= 1000 || (next.is_none() && !batch.is_empty()) {
                swept += self.sweep_batch(std::mem::take(&mut batch)).await?;
            }
            if next.is_none() {
                return Ok(swept);
            }
        }
    }

    async fn sweep_batch(&self, batch: Vec<((Uuid, Uuid), object_store::path::Path)>) -> Result<usize> {
        let ids: Vec<Uuid> = batch.iter().map(|((_, blob), _)| *blob).collect();
        let mut tx = db::begin(&self.inner.pool, Scope::All).await?;
        let known: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM sf3_blobs WHERE id = ANY($1)
             UNION SELECT blob_id FROM sf3_uploads WHERE blob_id = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let known: HashSet<Uuid> = known.into_iter().collect();
        let orphans: Vec<_> =
            batch.into_iter().filter(|((_, blob), _)| !known.contains(blob)).map(|(_, key)| key).collect();
        Ok(self.inner.blobs.delete_keys(orphans).await)
    }
}
