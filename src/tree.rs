//! Folders, moving, copying, the trash and file versions.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::db::{self, NodeRow, node_cols};
use crate::error::{Error, Result};
use crate::model::{Entry, Kind, TrashItem, VersionInfo};
use crate::path::VPath;
use crate::workspace::{Content, Expect, Lock, Pending, Workspace, guess_mime, insert_version};

impl Workspace {
    /// Create a folder. With `parents`, missing folders above it are created too and an existing
    /// folder is fine (`mkdir -p`); without, the parent must exist and the path must be free.
    pub async fn mkdir(&self, path: &str, parents: bool) -> Result<Entry> {
        self.need_write()?;
        let abs = self.abs(path)?;
        if self.is_folder_root(&abs) {
            if parents {
                return self.stat("/").await;
            }
            return Err(Error::AlreadyExists(self.shown(&abs)));
        }
        let mut tx = self.tx().await?;
        if let Some(row) = Self::find(&mut tx, self.id, &abs, Lock::None).await? {
            return match (row.is_dir(), parents) {
                (true, true) => {
                    tx.commit().await?;
                    Ok(self.entry(row))
                }
                (false, _) => Err(Error::NotADirectory(self.shown(&abs))),
                (true, false) => Err(Error::AlreadyExists(self.shown(&abs))),
            };
        }
        let parent = abs.parent().expect("not the root");
        if parents {
            self.ensure_dirs(&mut tx, &abs).await?;
        } else {
            match Self::find(&mut tx, self.id, &parent, Lock::Share).await? {
                Some(row) if !row.is_dir() => return Err(Error::NotADirectory(self.shown(&parent))),
                None if !self.is_folder_root(&parent) => return Err(Error::NotFound(self.shown(&parent))),
                _ => {}
            }
            // Locks the parent against a concurrent delete, and creates a confined handle's
            // root if it is not stored yet.
            self.ensure_dirs(&mut tx, &parent).await?;
            db::charge_nodes(&mut tx, self.id, 1).await?;
            sqlx::query(
                "INSERT INTO sf3_nodes (id, workspace_id, path, depth, kind, created_by, updated_by)
                 VALUES ($1, $2, $3, $4, 'dir', $5, $5)",
            )
            .bind(Uuid::now_v7())
            .bind(self.id)
            .bind(abs.as_str())
            .bind(abs.depth())
            .bind(self.access.actor.as_deref())
            .execute(&mut *tx)
            .await
            .map_err(Error::from)
            .map_err(|e| if e.is_unique_violation() { Error::AlreadyExists(self.shown(&abs)) } else { e })?;
        }
        let row = Self::find(&mut tx, self.id, &abs, Lock::None).await?.expect("just created");
        tx.commit().await?;
        Ok(self.entry(row))
    }

    /// Move a file or folder to the trash. A folder with anything in it needs `recursive`.
    pub async fn delete(&self, path: &str, recursive: bool) -> Result<TrashItem> {
        self.need_write()?;
        let abs = self.abs(path)?;
        if self.is_folder_root(&abs) {
            return Err(Error::PermissionDenied("the root folder cannot be deleted"));
        }
        let mut tx = self.tx().await?;
        let row =
            Self::find(&mut tx, self.id, &abs, Lock::Update).await?.ok_or_else(|| Error::NotFound(self.shown(&abs)))?;
        let item = self.trash_node(&mut tx, &row, recursive).await?;
        tx.commit().await?;
        Ok(item)
    }

    /// Trash `row` and everything under it, inside the caller's transaction.
    async fn trash_node(&self, tx: &mut db::Tx, row: &NodeRow, recursive: bool) -> Result<TrashItem> {
        let abs = VPath::trusted(row.path.clone());
        let (lo, hi) = abs.subtree();
        if row.is_dir() && !recursive {
            let has_children: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM sf3_nodes
                 WHERE workspace_id = $1 AND trash_id IS NULL AND path >= $2 AND path < $3)",
            )
            .bind(self.id)
            .bind(&lo)
            .bind(&hi)
            .fetch_one(&mut **tx)
            .await?;
            if has_children {
                return Err(Error::NotEmpty(self.shown(&abs)));
            }
        }
        let trash_id = Uuid::now_v7();
        let (size,): (Option<i64>,) = sqlx::query_as(
            "WITH moved AS (
                 UPDATE sf3_nodes SET trash_id = $2, trashed_at = now()
                 WHERE workspace_id = $1 AND trash_id IS NULL
                   AND (path = $3 OR (path >= $4 AND path < $5))
                 RETURNING size)
             SELECT sum(size)::bigint FROM moved",
        )
        .bind(self.id)
        .bind(trash_id)
        .bind(abs.as_str())
        .bind(&lo)
        .bind(&hi)
        .fetch_one(&mut **tx)
        .await?;
        let (trashed_at,): (DateTime<Utc>,) = sqlx::query_as(
            "INSERT INTO sf3_trash (id, workspace_id, path, kind, size, trashed_by)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING trashed_at",
        )
        .bind(trash_id)
        .bind(self.id)
        .bind(abs.as_str())
        .bind(&row.kind)
        .bind(size.unwrap_or(0))
        .bind(self.access.actor.as_deref())
        .fetch_one(&mut **tx)
        .await?;
        Ok(TrashItem {
            id: trash_id,
            path: self.shown(&abs),
            kind: Kind::from_db(&row.kind),
            size: size.unwrap_or(0).max(0) as u64,
            trashed_at,
            trashed_by: self.access.actor.clone(),
        })
    }

    /// Move or rename. `to` is the new path itself, not a folder to move into. An existing `to`
    /// is replaced (and trashed) only with `overwrite`, and only by the same kind; a folder can
    /// only replace an empty folder.
    pub async fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<Entry> {
        self.need_write()?;
        let (src, dst) = (self.abs(from)?, self.abs(to)?);
        if self.is_folder_root(&src) {
            return Err(Error::PermissionDenied("the root folder cannot be moved"));
        }
        if self.is_folder_root(&dst) {
            return Err(Error::AlreadyExists(self.shown(&dst)));
        }
        if dst.within(&src) && dst != src {
            return Err(Error::InvalidInput("a folder cannot be moved into itself".into()));
        }
        let mut tx = self.tx().await?;
        let row =
            Self::find(&mut tx, self.id, &src, Lock::Update).await?.ok_or_else(|| Error::NotFound(self.shown(&src)))?;
        if src == dst {
            tx.commit().await?;
            return Ok(self.entry(row));
        }
        self.clear_target(&mut tx, &row, &dst, overwrite).await?;
        self.ensure_dirs(&mut tx, &dst.parent().expect("not the root")).await?;
        let (lo, hi) = src.subtree();
        let delta = dst.depth() - src.depth();
        // Every path in the subtree gets the new prefix in one statement. The destination and
        // everything under it is free (checked above), so no two rows collide on the way.
        sqlx::query(
            "UPDATE sf3_nodes
             SET path = $3 || substr(path, char_length($2) + 1), depth = depth + $6,
                 updated_at = now(), updated_by = $7
             WHERE workspace_id = $1 AND trash_id IS NULL AND (path = $2 OR (path >= $4 AND path < $5))",
        )
        .bind(self.id)
        .bind(src.as_str())
        .bind(dst.as_str())
        .bind(&lo)
        .bind(&hi)
        .bind(delta)
        .bind(self.access.actor.as_deref())
        .execute(&mut *tx)
        .await?;
        if !row.is_dir() {
            sqlx::query("UPDATE sf3_nodes SET mime = $2 WHERE id = $1")
                .bind(row.id)
                .bind(guess_mime(&dst))
                .execute(&mut *tx)
                .await?;
        }
        let moved = Self::find(&mut tx, self.id, &dst, Lock::None).await?.expect("just moved");
        tx.commit().await?;
        Ok(self.entry(moved))
    }

    /// Make `dst` free for `src` to land on, trashing what is there when `overwrite` allows.
    async fn clear_target(&self, tx: &mut db::Tx, src: &NodeRow, dst: &VPath, overwrite: bool) -> Result<()> {
        let Some(existing) = Self::find(tx, self.id, dst, Lock::Update).await? else {
            return Ok(());
        };
        if !overwrite {
            return Err(Error::AlreadyExists(self.shown(dst)));
        }
        match (src.is_dir(), existing.is_dir()) {
            (false, true) => return Err(Error::IsADirectory(self.shown(dst))),
            (true, false) => return Err(Error::NotADirectory(self.shown(dst))),
            _ => {}
        }
        // A folder only replaces an empty folder: trash_node refuses a non-empty one.
        self.trash_node(tx, &existing, false).await?;
        Ok(())
    }

    /// Copy a file or a whole folder. Contents are shared, not duplicated: a copy costs no
    /// storage until one side changes. The copy starts its own history at version 1.
    pub async fn copy(&self, from: &str, to: &str, overwrite: bool) -> Result<Entry> {
        self.need_write()?;
        let (src, dst) = (self.abs(from)?, self.abs(to)?);
        if self.is_folder_root(&dst) {
            return Err(Error::AlreadyExists(self.shown(&dst)));
        }
        if dst.within(&src) {
            return Err(Error::InvalidInput("a folder cannot be copied into itself".into()));
        }
        // (The root itself is never copied: every destination is inside it.)
        let mut tx = self.tx().await?;
        let source =
            Self::find(&mut tx, self.id, &src, Lock::Share).await?.ok_or_else(|| Error::NotFound(self.shown(&src)))?;
        let is_dir = source.is_dir();
        let mut nodes = vec![source];
        if is_dir {
            let (lo, hi) = src.subtree();
            let limit = self.inner.cfg.max_copy_nodes;
            let children: Vec<NodeRow> = sqlx::query_as(concat!(
                "SELECT ",
                node_cols!(),
                " FROM sf3_nodes
                 WHERE workspace_id = $1 AND trash_id IS NULL AND path >= $2 AND path < $3
                 ORDER BY path LIMIT $4 FOR SHARE"
            ))
            .bind(self.id)
            .bind(&lo)
            .bind(&hi)
            .bind(limit as i64 + 1)
            .fetch_all(&mut *tx)
            .await?;
            if nodes.len() + children.len() > limit {
                return Err(Error::InvalidInput(format!("more than {limit} files and folders to copy at once")));
            }
            nodes.extend(children);
        }
        self.clear_target(&mut tx, &nodes[0], &dst, overwrite).await?;
        self.ensure_dirs(&mut tx, &dst.parent().expect("not the root")).await?;
        let mut created = 0i64;
        for row in &nodes {
            let target = VPath::trusted(row.path.clone()).rebase(&src, &dst).expect("inside the source");
            insert_copy(&mut tx, self, row, &target).await?;
            created += 1;
        }
        db::charge_nodes(&mut tx, self.id, created).await?;
        let root = Self::find(&mut tx, self.id, &dst, Lock::None).await?.expect("just copied");
        tx.commit().await?;
        Ok(self.entry(root))
    }

    // ---- trash ---------------------------------------------------------------------------

    /// What is in the trash under this handle's root, newest first.
    pub async fn list_trash(&self) -> Result<Vec<TrashItem>> {
        self.need_read()?;
        let root = &self.access.root;
        let (lo, hi) = root.subtree();
        let mut tx = self.tx().await?;
        let rows: Vec<(Uuid, String, String, i64, DateTime<Utc>, Option<String>)> = sqlx::query_as(
            "SELECT id, path, kind, size, trashed_at, trashed_by FROM sf3_trash
             WHERE workspace_id = $1 AND (path = $2 OR (path >= $3 AND path < $4))
             ORDER BY trashed_at DESC, id DESC LIMIT 1000",
        )
        .bind(self.id)
        .bind(root.as_str())
        .bind(&lo)
        .bind(&hi)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|(id, path, kind, size, trashed_at, trashed_by)| TrashItem {
                id,
                path: self.shown_str(&path),
                kind: Kind::from_db(&kind),
                size: size.max(0) as u64,
                trashed_at,
                trashed_by,
            })
            .collect())
    }

    /// Put a trashed item back where it was. Fails if that path is taken again; folders above
    /// it that are gone meanwhile are recreated.
    pub async fn restore(&self, trash_id: Uuid) -> Result<Entry> {
        self.need_write()?;
        let mut tx = self.tx().await?;
        let path = self.trash_path(&mut tx, trash_id).await?;
        if Self::find(&mut tx, self.id, &path, Lock::Update).await?.is_some() {
            return Err(Error::AlreadyExists(self.shown(&path)));
        }
        self.ensure_dirs(&mut tx, &path.parent().expect("not the root")).await?;
        sqlx::query(
            "UPDATE sf3_nodes SET trash_id = NULL, trashed_at = NULL
             WHERE workspace_id = $1 AND trash_id = $2",
        )
        .bind(self.id)
        .bind(trash_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM sf3_trash WHERE id = $1").bind(trash_id).execute(&mut *tx).await?;
        let row = Self::find(&mut tx, self.id, &path, Lock::None).await?.expect("just restored");
        tx.commit().await?;
        Ok(self.entry(row))
    }

    /// Delete a trashed item for good, freeing its storage.
    pub async fn purge(&self, trash_id: Uuid) -> Result<()> {
        self.need_write()?;
        let mut tx = self.tx().await?;
        self.trash_path(&mut tx, trash_id).await?;
        let freed = purge_in(&mut tx, self.id, trash_id).await?;
        tx.commit().await?;
        self.inner.blobs.delete(freed).await;
        Ok(())
    }

    /// Purge everything in the trash under this handle's root.
    pub async fn empty_trash(&self) -> Result<usize> {
        let items = self.list_trash().await?;
        for item in &items {
            self.purge(item.id).await?;
        }
        Ok(items.len())
    }

    /// The original path of a trash item this handle may see, locking the item.
    async fn trash_path(&self, tx: &mut db::Tx, trash_id: Uuid) -> Result<VPath> {
        let path: Option<String> =
            sqlx::query_scalar("SELECT path FROM sf3_trash WHERE workspace_id = $1 AND id = $2 FOR UPDATE")
                .bind(self.id)
                .bind(trash_id)
                .fetch_optional(&mut **tx)
                .await?;
        let path = path.map(VPath::trusted).filter(|p| p.within(&self.access.root) && *p != self.access.root);
        path.ok_or_else(|| Error::NotFound(format!("trash item {trash_id}")))
    }

    // ---- versions ------------------------------------------------------------------------

    /// A file's kept versions, newest first.
    pub async fn versions(&self, path: &str) -> Result<Vec<VersionInfo>> {
        self.need_read()?;
        let abs = self.abs(path)?;
        let row = self.file(&abs).await?;
        let mut tx = self.tx().await?;
        let rows: Vec<(i32, i64, DateTime<Utc>, Option<String>)> = sqlx::query_as(
            "SELECT version, size, created_at, created_by FROM sf3_versions
             WHERE workspace_id = $1 AND node_id = $2 ORDER BY version DESC",
        )
        .bind(self.id)
        .bind(row.id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|(version, size, created_at, created_by)| VersionInfo {
                version,
                size: size.max(0) as u64,
                created_at,
                created_by,
            })
            .collect())
    }

    pub async fn read_version(&self, path: &str, version: i32) -> Result<Bytes> {
        self.need_read()?;
        let abs = self.abs(path)?;
        let (blob, size) = self.version_blob(&abs, version).await?;
        let limit = self.inner.cfg.max_inline_bytes;
        if size > limit {
            return Err(Error::TooLarge { size, limit });
        }
        self.inner.blobs.get(self.id, blob, size).await
    }

    /// Make an old version current again, as a new version (history is never rewritten).
    pub async fn restore_version(&self, path: &str, version: i32) -> Result<Entry> {
        self.need_write()?;
        let abs = self.abs(path)?;
        let (blob, size) = self.version_blob(&abs, version).await?;
        let pending = Pending { abs, content: Content::Existing { blob, size }, mime: None, expect: Expect::Any };
        Ok(self.commit(vec![pending]).await?.remove(0))
    }

    async fn version_blob(&self, abs: &VPath, version: i32) -> Result<(Uuid, u64)> {
        let row = self.file(abs).await?;
        let mut tx = self.tx().await?;
        let found: Option<(Uuid, i64)> = sqlx::query_as(
            "SELECT blob_id, size FROM sf3_versions WHERE workspace_id = $1 AND node_id = $2 AND version = $3",
        )
        .bind(self.id)
        .bind(row.id)
        .bind(version)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let (blob, size) = found.ok_or_else(|| Error::NotFound(format!("{} version {version}", self.shown(abs))))?;
        Ok((blob, size.max(0) as u64))
    }
}

async fn insert_copy(tx: &mut db::Tx, ws: &Workspace, row: &NodeRow, target: &VPath) -> Result<()> {
    let id = Uuid::now_v7();
    let actor = ws.access.actor.as_deref();
    let version = if row.is_dir() { 0 } else { 1 };
    sqlx::query(
        "INSERT INTO sf3_nodes (id, workspace_id, path, depth, kind, blob_id, size, mime, version,
             created_by, updated_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10)",
    )
    .bind(id)
    .bind(ws.id)
    .bind(target.as_str())
    .bind(target.depth())
    .bind(&row.kind)
    .bind(row.blob_id)
    .bind(row.size)
    .bind(row.mime.as_deref())
    .bind(version)
    .bind(actor)
    .execute(&mut **tx)
    .await?;
    if let Some(blob) = row.blob_id {
        insert_version(tx, ws.id, id, 1, blob, row.size, actor).await?;
    }
    Ok(())
}

/// Delete the nodes of a trash item and the item itself; return blobs freed for deletion.
pub(crate) async fn purge_in(tx: &mut db::Tx, ws: Uuid, trash_id: Uuid) -> Result<Vec<(Uuid, Uuid)>> {
    let mut candidates: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT v.blob_id FROM sf3_versions v JOIN sf3_nodes n ON n.id = v.node_id
         WHERE n.workspace_id = $1 AND n.trash_id = $2",
    )
    .bind(ws)
    .bind(trash_id)
    .fetch_all(&mut **tx)
    .await?;
    let deleted: Vec<Option<Uuid>> =
        sqlx::query_scalar("DELETE FROM sf3_nodes WHERE workspace_id = $1 AND trash_id = $2 RETURNING blob_id")
            .bind(ws)
            .bind(trash_id)
            .fetch_all(&mut **tx)
            .await?;
    candidates.extend(deleted.iter().flatten());
    candidates.sort();
    candidates.dedup();
    db::charge_nodes(tx, ws, -(deleted.len() as i64)).await?;
    sqlx::query("DELETE FROM sf3_trash WHERE workspace_id = $1 AND id = $2")
        .bind(ws)
        .bind(trash_id)
        .execute(&mut **tx)
        .await?;
    db::collect_garbage(tx, ws, candidates).await
}
