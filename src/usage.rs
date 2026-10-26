//! How much is stored, where and in what: totals, a breakdown, sizes of folders, storage by
//! type, the largest files, and totals across an app.

use std::collections::BTreeMap;

use crate::Sysfile3;
use crate::db::{self, NodeRow, Scope, node_cols};
use crate::error::{Error, Result};
use crate::model::{AppUsage, Breakdown, Entry, FolderUsage, TypeUsage, Usage};
use crate::workspace::{Lock, Workspace};

const MIB: f64 = 1024.0 * 1024.0;
const GIB: f64 = MIB * 1024.0;

impl Usage {
    pub fn used_mib(&self) -> f64 {
        self.used_bytes as f64 / MIB
    }

    pub fn used_gib(&self) -> f64 {
        self.used_bytes as f64 / GIB
    }

    /// Bytes left before the quota; `None` when unlimited.
    pub fn remaining_bytes(&self) -> Option<u64> {
        self.quota_bytes.map(|q| q.saturating_sub(self.used_bytes))
    }

    /// Files and folders left before the quota; `None` when unlimited.
    pub fn remaining_nodes(&self) -> Option<u64> {
        self.quota_nodes.map(|q| q.saturating_sub(self.node_count))
    }

    /// Share of the byte quota in use, 0 to 100 (more if a quota was lowered below usage);
    /// `None` when unlimited.
    pub fn used_percent(&self) -> Option<f64> {
        self.quota_bytes.map(|q| if q == 0 { 100.0 } else { self.used_bytes as f64 * 100.0 / q as f64 })
    }
}

impl AppUsage {
    pub fn used_gib(&self) -> f64 {
        self.used_bytes as f64 / GIB
    }
}

impl FolderUsage {
    pub fn mib(&self) -> f64 {
        self.bytes as f64 / MIB
    }
}

/// The storage category of a content type, as storage screens show it.
pub fn category(mime: &str) -> &'static str {
    let (top, sub) = mime.split_once('/').unwrap_or((mime, ""));
    match top {
        "image" => "images",
        "video" => "videos",
        "audio" => "audio",
        "text" => "text",
        "application" => {
            let documents =
                ["pdf", "msword", "rtf", "epub+zip", "vnd.openxmlformats", "vnd.ms-", "vnd.oasis.opendocument"];
            let text = ["json", "xml", "javascript", "x-yaml", "yaml", "toml", "x-sh", "sql"];
            let archives = ["zip", "gzip", "x-tar", "x-7z-compressed", "x-rar", "vnd.rar", "x-bzip2", "x-xz", "zstd"];
            if documents.iter().any(|d| sub.starts_with(d)) {
                "documents"
            } else if text.iter().any(|t| sub == *t || sub.ends_with(&format!("+{t}"))) {
                "text"
            } else if archives.contains(&sub) {
                "archives"
            } else {
                "other"
            }
        }
        _ => "other",
    }
}

impl Workspace {
    /// Size of a folder (or a single file) as it is now: its files, its folders and the bytes of
    /// their current contents. Old versions and the trash are not included; see `breakdown`.
    pub async fn disk_usage(&self, path: &str) -> Result<FolderUsage> {
        self.need_read()?;
        let abs = self.abs(path)?;
        let mut tx = self.tx().await?;
        if Self::find(&mut tx, self.id, &abs, Lock::None).await?.is_none() && !self.is_folder_root(&abs) {
            return Err(Error::NotFound(self.shown(&abs)));
        }
        let (lo, hi) = abs.subtree();
        let (files, folders, bytes): (i64, i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE kind = 'file'), count(*) FILTER (WHERE kind = 'dir'),
                    coalesce(sum(size), 0)::bigint
             FROM sf3_nodes
             WHERE workspace_id = $1 AND trash_id IS NULL AND (path = $2 OR (path >= $3 AND path < $4))",
        )
        .bind(self.id)
        .bind(abs.as_str())
        .bind(&lo)
        .bind(&hi)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(FolderUsage { files: files as u64, folders: folders as u64, bytes: bytes.max(0) as u64 })
    }

    /// Where the space goes under this handle's root: current files, older versions and the
    /// trash, plus storage by type. Sizes here are per file, so a copy counts again; the
    /// workspace's `billed_bytes` counts shared contents once and is what quotas use.
    pub async fn breakdown(&self) -> Result<Breakdown> {
        self.need_read()?;
        let root = &self.access.root;
        let (lo, hi) = root.subtree();
        let mut tx = self.tx().await?;
        let usage = crate::workspace::usage_of(&mut tx, self.id).await?;
        let (files, folders, file_bytes, trashed_files, trash_bytes): (i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE kind = 'file' AND trash_id IS NULL),
                    count(*) FILTER (WHERE kind = 'dir' AND trash_id IS NULL),
                    coalesce(sum(size) FILTER (WHERE trash_id IS NULL), 0)::bigint,
                    count(*) FILTER (WHERE kind = 'file' AND trash_id IS NOT NULL),
                    coalesce(sum(size) FILTER (WHERE trash_id IS NOT NULL), 0)::bigint
             FROM sf3_nodes n
             WHERE workspace_id = $1 AND (n.path = $2 OR (n.path >= $3 AND n.path < $4))",
        )
        .bind(self.id)
        .bind(root.as_str())
        .bind(&lo)
        .bind(&hi)
        .fetch_one(&mut *tx)
        .await?;
        let (old_versions, version_bytes): (i64, i64) = sqlx::query_as(
            "SELECT count(*), coalesce(sum(v.size), 0)::bigint
             FROM sf3_versions v JOIN sf3_nodes n ON n.id = v.node_id
             WHERE n.workspace_id = $1 AND v.version < n.version
               AND (n.path = $2 OR (n.path >= $3 AND n.path < $4))",
        )
        .bind(self.id)
        .bind(root.as_str())
        .bind(&lo)
        .bind(&hi)
        .fetch_one(&mut *tx)
        .await?;
        let trash_items: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sf3_trash n
             WHERE workspace_id = $1 AND (n.path = $2 OR (n.path >= $3 AND n.path < $4))",
        )
        .bind(self.id)
        .bind(root.as_str())
        .bind(&lo)
        .bind(&hi)
        .fetch_one(&mut *tx)
        .await?;
        let by_mime: Vec<(Option<String>, i64, i64)> = sqlx::query_as(
            "SELECT mime, count(*), coalesce(sum(size), 0)::bigint FROM sf3_nodes n
             WHERE workspace_id = $1 AND kind = 'file' AND trash_id IS NULL
               AND (n.path = $2 OR (n.path >= $3 AND n.path < $4))
             GROUP BY mime",
        )
        .bind(self.id)
        .bind(root.as_str())
        .bind(&lo)
        .bind(&hi)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        let mut types: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new();
        for (mime, count, bytes) in by_mime {
            let slot = types.entry(category(mime.as_deref().unwrap_or(""))).or_default();
            slot.0 += count as u64;
            slot.1 += bytes.max(0) as u64;
        }
        let mut by_type: Vec<TypeUsage> = types
            .into_iter()
            .map(|(category, (files, bytes))| TypeUsage { category: category.to_string(), files, bytes })
            .collect();
        by_type.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.category.cmp(&b.category)));

        Ok(Breakdown {
            usage,
            files: files as u64,
            folders: folders as u64,
            file_bytes: file_bytes.max(0) as u64,
            old_versions: old_versions as u64,
            version_bytes: version_bytes.max(0) as u64,
            trash_items: trash_items as u64,
            trashed_files: trashed_files as u64,
            trash_bytes: trash_bytes.max(0) as u64,
            by_type,
        })
    }

    /// The biggest files under a folder, largest first (at most 1000).
    pub async fn largest_files(&self, under: &str, limit: usize) -> Result<Vec<Entry>> {
        self.need_read()?;
        let abs = self.abs(under)?;
        let (lo, hi) = abs.subtree();
        let mut tx = self.tx().await?;
        let rows: Vec<NodeRow> = sqlx::query_as(concat!(
            "SELECT ",
            node_cols!(),
            " FROM sf3_nodes
             WHERE workspace_id = $1 AND trash_id IS NULL AND kind = 'file'
               AND (path = $2 OR (path >= $3 AND path < $4))
             ORDER BY size DESC, path LIMIT $5"
        ))
        .bind(self.id)
        .bind(abs.as_str())
        .bind(&lo)
        .bind(&hi)
        .bind(limit.clamp(1, 1000) as i64)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(|r| self.entry(r)).collect())
    }
}

impl Sysfile3 {
    /// Totals across every workspace of an app.
    pub async fn app_usage(&self, app: &str) -> Result<AppUsage> {
        let mut tx = db::begin(&self.inner.pool, Scope::All).await?;
        let (workspaces, bytes, nodes): (i64, i64, i64) = sqlx::query_as(
            "SELECT count(*), coalesce(sum(used_bytes), 0)::bigint, coalesce(sum(node_count), 0)::bigint
             FROM sf3_workspaces WHERE app = $1",
        )
        .bind(app)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(AppUsage { workspaces: workspaces as u64, used_bytes: bytes.max(0) as u64, node_count: nodes.max(0) as u64 })
    }
}

#[cfg(test)]
mod tests {
    use super::category;

    #[test]
    fn categories() {
        for (mime, want) in [
            ("image/png", "images"),
            ("video/mp4", "videos"),
            ("audio/mpeg", "audio"),
            ("text/markdown", "text"),
            ("application/json", "text"),
            ("application/ld+json", "text"),
            ("application/pdf", "documents"),
            ("application/vnd.openxmlformats-officedocument.wordprocessingml.document", "documents"),
            ("application/zip", "archives"),
            ("application/octet-stream", "other"),
            ("", "other"),
        ] {
            assert_eq!(category(mime), want, "{mime}");
        }
    }
}
