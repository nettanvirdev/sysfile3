//! The workspace handle: reading and writing files.

use std::collections::HashSet;
use std::sync::Arc;

use bytes::Bytes;
use chrono::Utc;
use futures::stream::BoxStream;
use object_store::signer::Method;
use uuid::Uuid;

use crate::db::{self, NodeRow, Scope, Tx, node_cols};
use crate::edit::{self, Edit};
use crate::error::{Error, Result};
use crate::model::{Entry, Kind, Listing, TextWindow, Usage};
use crate::path::VPath;
use crate::store::Stored;
use crate::{Access, Inner};

/// One workspace, as one caller may see it. Every path a handle takes or returns is relative to
/// its access root, and nothing outside that root can be named.
#[derive(Clone)]
pub struct Workspace {
    pub(crate) inner: Arc<Inner>,
    pub(crate) id: Uuid,
    pub(crate) access: Access,
}

/// A precondition on a write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Expect {
    /// Write whatever is there.
    #[default]
    Any,
    /// Only create: fail with `AlreadyExists` if the path is taken.
    Absent,
    /// Only replace this version: fail with `Conflict` if the file changed since it was read.
    Version(i32),
}

/// Options for `write`, `write_stream` and uploads.
#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    /// When the write may happen; see [`Expect`].
    pub expect: Expect,
    /// Content type. Guessed from the file name when not given on creation; kept on overwrite.
    pub mime: Option<String>,
}

/// Options for `list`.
#[derive(Debug, Clone)]
pub struct ListOptions {
    /// Levels to descend: 1 lists the folder's own children.
    pub depth: u32,
    /// Page size, at most `Config::list_limit`.
    pub limit: Option<usize>,
    /// Continue after this path (a `Listing::next`).
    pub after: Option<String>,
}

impl Default for ListOptions {
    fn default() -> Self {
        ListOptions { depth: 1, limit: None, after: None }
    }
}

/// The edits for one file in `edit_many`.
#[derive(Debug, Clone)]
pub struct FileEdits {
    /// The file to edit.
    pub path: String,
    /// Its edits, applied in order.
    pub edits: Vec<Edit>,
    /// When the edit may happen; see [`Expect`].
    pub expect: Expect,
}

/// The result of an edit.
#[derive(Debug, Clone)]
pub struct EditOutcome {
    /// The file after the edit.
    pub entry: Entry,
    /// Replacements made.
    pub replacements: usize,
}

/// A file about to be committed: where, what, and on what condition.
pub(crate) struct Pending {
    pub abs: VPath,
    pub content: Content,
    pub mime: Option<String>,
    pub expect: Expect,
}

/// New bytes just uploaded, or a blob the workspace already has (copies, restored versions).
pub(crate) enum Content {
    New(Stored),
    Existing { blob: Uuid, size: u64 },
}

impl Content {
    fn blob(&self) -> Uuid {
        match self {
            Content::New(s) => s.id,
            Content::Existing { blob, .. } => *blob,
        }
    }

    fn size(&self) -> u64 {
        match self {
            Content::New(s) => s.size,
            Content::Existing { size, .. } => *size,
        }
    }
}

impl Workspace {
    /// The workspace id (stable, internal).
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// What this handle may do and where.
    pub fn access(&self) -> &Access {
        &self.access
    }

    pub(crate) fn need_read(&self) -> Result<()> {
        if self.access.read { Ok(()) } else { Err(Error::PermissionDenied("this handle cannot read")) }
    }

    pub(crate) fn need_write(&self) -> Result<()> {
        if self.access.write { Ok(()) } else { Err(Error::PermissionDenied("this handle is read-only")) }
    }

    pub(crate) fn need_share(&self) -> Result<()> {
        if self.access.share { Ok(()) } else { Err(Error::PermissionDenied("this handle cannot share")) }
    }

    /// A caller's path as a workspace path.
    pub(crate) fn abs(&self, path: &str) -> Result<VPath> {
        Ok(self.access.root.join(&VPath::parse(path)?))
    }

    /// A workspace path as the caller sees it. Paths outside the root are shown as `/` rather
    /// than revealed (they only come up in errors about the root's own parents).
    pub(crate) fn shown(&self, abs: &VPath) -> String {
        abs.strip(&self.access.root).map(|p| p.to_string()).unwrap_or_else(|| "/".into())
    }

    pub(crate) fn shown_str(&self, abs: &str) -> String {
        self.shown(&VPath::trusted(abs.to_string()))
    }

    pub(crate) async fn tx(&self) -> Result<Tx> {
        db::begin(&self.inner.pool, Scope::Workspace(self.id)).await
    }

    pub(crate) fn entry(&self, row: NodeRow) -> Entry {
        Entry {
            id: row.id,
            path: self.shown_str(&row.path),
            kind: Kind::from_db(&row.kind),
            size: row.size.max(0) as u64,
            mime: row.mime,
            version: row.version,
            created_at: row.created_at,
            updated_at: row.updated_at,
            created_by: row.created_by,
            updated_by: row.updated_by,
        }
    }

    /// The handle's root when no folder is stored there (the workspace root, or a confined root
    /// not created yet): an empty folder.
    fn virtual_root(&self) -> Entry {
        let now = Utc::now();
        Entry {
            id: Uuid::nil(),
            path: "/".into(),
            kind: Kind::Dir,
            size: 0,
            mime: None,
            version: 0,
            created_at: now,
            updated_at: now,
            created_by: None,
            updated_by: None,
        }
    }

    pub(crate) async fn find(tx: &mut Tx, ws: Uuid, abs: &VPath, lock: Lock) -> Result<Option<NodeRow>> {
        if abs.is_root() {
            return Ok(None);
        }
        let sql = match lock {
            Lock::None => concat!(
                "SELECT ",
                node_cols!(),
                " FROM sf3_nodes WHERE workspace_id = $1 AND path = $2 AND trash_id IS NULL"
            ),
            Lock::Share => concat!(
                "SELECT ",
                node_cols!(),
                " FROM sf3_nodes WHERE workspace_id = $1 AND path = $2 AND trash_id IS NULL FOR SHARE"
            ),
            Lock::Update => concat!(
                "SELECT ",
                node_cols!(),
                " FROM sf3_nodes WHERE workspace_id = $1 AND path = $2 AND trash_id IS NULL FOR UPDATE"
            ),
        };
        Ok(sqlx::query_as(sql).bind(ws).bind(abs.as_str()).fetch_optional(&mut **tx).await?)
    }

    /// The node at `abs`, read in its own short transaction.
    pub(crate) async fn node(&self, abs: &VPath) -> Result<Option<NodeRow>> {
        let mut tx = self.tx().await?;
        let row = Self::find(&mut tx, self.id, abs, Lock::None).await?;
        tx.commit().await?;
        Ok(row)
    }

    /// The file at `abs`, or the error a caller expects for a folder or a missing path.
    pub(crate) async fn file(&self, abs: &VPath) -> Result<NodeRow> {
        match self.node(abs).await? {
            Some(row) if !row.is_dir() => Ok(row),
            Some(_) => Err(Error::IsADirectory(self.shown(abs))),
            None if self.is_folder_root(abs) => Err(Error::IsADirectory(self.shown(abs))),
            None => Err(Error::NotFound(self.shown(abs))),
        }
    }

    /// True for paths that are a folder even without a stored row: the workspace root, and a
    /// handle's own root.
    pub(crate) fn is_folder_root(&self, abs: &VPath) -> bool {
        abs.is_root() || *abs == self.access.root
    }

    // ---- reading -------------------------------------------------------------------------

    /// A file or folder.
    pub async fn stat(&self, path: &str) -> Result<Entry> {
        self.need_read()?;
        let abs = self.abs(path)?;
        match self.node(&abs).await? {
            Some(row) => Ok(self.entry(row)),
            None if self.is_folder_root(&abs) => Ok(self.virtual_root()),
            None => Err(Error::NotFound(self.shown(&abs))),
        }
    }

    /// Whether a path exists.
    pub async fn exists(&self, path: &str) -> Result<bool> {
        match self.stat(path).await {
            Ok(_) => Ok(true),
            Err(Error::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// A folder's contents in path order, `depth` levels down, one page at a time.
    pub async fn list(&self, path: &str, opts: ListOptions) -> Result<Listing> {
        self.need_read()?;
        let abs = self.abs(path)?;
        let limit = opts.limit.unwrap_or(self.inner.cfg.list_limit).clamp(1, self.inner.cfg.list_limit);
        let depth = opts.depth.clamp(1, 64) as i32;
        let after = match &opts.after {
            Some(a) => Some(self.abs(a)?.to_string()),
            None => None,
        };
        let mut tx = self.tx().await?;
        match Self::find(&mut tx, self.id, &abs, Lock::None).await? {
            Some(row) if !row.is_dir() => return Err(Error::NotADirectory(self.shown(&abs))),
            None if !self.is_folder_root(&abs) => return Err(Error::NotFound(self.shown(&abs))),
            _ => {}
        }
        let (lo, hi) = abs.subtree();
        let base = abs.depth();
        let mut rows: Vec<NodeRow> = sqlx::query_as(concat!(
            "SELECT ",
            node_cols!(),
            " FROM sf3_nodes
             WHERE workspace_id = $1 AND trash_id IS NULL AND path >= $2 AND path < $3
               AND depth > $4 AND depth <= $5 AND ($6::text IS NULL OR path > $6)
             ORDER BY path LIMIT $7"
        ))
        .bind(self.id)
        .bind(&lo)
        .bind(&hi)
        .bind(base)
        .bind(base + depth)
        .bind(after)
        .bind(limit as i64 + 1)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let next = if rows.len() > limit {
            rows.truncate(limit);
            rows.last().map(|r| self.shown_str(&r.path))
        } else {
            None
        };
        Ok(Listing { entries: rows.into_iter().map(|r| self.entry(r)).collect(), next })
    }

    /// A whole file, up to `Config::max_inline_bytes`.
    pub async fn read(&self, path: &str) -> Result<Bytes> {
        self.need_read()?;
        let abs = self.abs(path)?;
        let row = self.file(&abs).await?;
        let size = row.size as u64;
        let limit = self.inner.cfg.max_inline_bytes;
        if size > limit {
            return Err(Error::TooLarge { size, limit });
        }
        self.inner.blobs.get(self.id, row.blob_id.expect("files have a blob"), size).await
    }

    /// Up to `limit` lines of a text file from line `first_line` (1-based; 0 counts as 1).
    pub async fn read_text(&self, path: &str, first_line: usize, limit: Option<usize>) -> Result<TextWindow> {
        self.need_read()?;
        let abs = self.abs(path)?;
        let (row, bytes) = self.text_bytes(&abs).await?;
        let text = edit::as_text(&bytes).ok_or_else(|| Error::NotText(self.shown(&abs)))?;
        let all = edit::lines(text);
        let first = first_line.max(1);
        let lines = all.iter().skip(first - 1).take(limit.unwrap_or(usize::MAX)).map(|l| l.to_string()).collect();
        Ok(TextWindow { lines, first_line: first, total_lines: all.len(), version: row.version })
    }

    pub(crate) async fn text_bytes(&self, abs: &VPath) -> Result<(NodeRow, Bytes)> {
        let row = self.file(abs).await?;
        let size = row.size as u64;
        let limit = self.inner.cfg.max_text_bytes;
        if size > limit {
            return Err(Error::TooLarge { size, limit });
        }
        let bytes = self.inner.blobs.get(self.id, row.blob_id.expect("files have a blob"), size).await?;
        Ok((row, bytes))
    }

    /// `len` bytes from `offset`, clipped to the end of the file.
    pub async fn read_range(&self, path: &str, offset: u64, len: u64) -> Result<Bytes> {
        self.need_read()?;
        let abs = self.abs(path)?;
        let row = self.file(&abs).await?;
        let size = row.size as u64;
        let end = offset.saturating_add(len).min(size);
        if offset >= end {
            return Ok(Bytes::new());
        }
        if end - offset > self.inner.cfg.max_inline_bytes {
            return Err(Error::TooLarge { size: end - offset, limit: self.inner.cfg.max_inline_bytes });
        }
        self.inner.blobs.get_range(self.id, row.blob_id.expect("files have a blob"), offset..end).await
    }

    /// A file of any size as a stream of chunks.
    pub async fn open(&self, path: &str) -> Result<(Entry, BoxStream<'static, Result<Bytes>>)> {
        self.need_read()?;
        let abs = self.abs(path)?;
        let row = self.file(&abs).await?;
        let stream = self.inner.blobs.stream(self.id, row.blob_id.expect("files have a blob")).await?;
        Ok((self.entry(row), stream))
    }

    /// A URL anyone can GET the file from for `Config::download_ttl`. Needs a signer.
    pub async fn download_url(&self, path: &str) -> Result<String> {
        self.need_read()?;
        let abs = self.abs(path)?;
        let row = self.file(&abs).await?;
        let ttl = self.inner.cfg.download_ttl;
        self.inner.blobs.signed_url(Method::GET, self.id, row.blob_id.expect("files have a blob"), ttl).await
    }

    /// The workspace's stored bytes, file count and limits (for the whole workspace, even on a confined handle).
    pub async fn usage(&self) -> Result<Usage> {
        let mut tx = self.tx().await?;
        let usage = usage_of(&mut tx, self.id).await?;
        tx.commit().await?;
        Ok(usage)
    }

    // ---- writing -------------------------------------------------------------------------

    /// Create or replace a file, up to `Config::max_inline_bytes`. Missing folders are created.
    pub async fn write(&self, path: &str, data: impl Into<Bytes>, opts: WriteOptions) -> Result<Entry> {
        self.need_write()?;
        let abs = self.abs(path)?;
        let data = data.into();
        let size = data.len() as u64;
        let limit = self.inner.cfg.max_inline_bytes.min(self.inner.cfg.max_file_bytes);
        if size > limit {
            return Err(Error::TooLarge { size, limit });
        }
        self.precheck(&abs, size).await?;
        let mime = opts.mime.clone().unwrap_or_else(|| guess_mime(&abs));
        let stored = self.inner.blobs.put(self.id, data, &mime).await?;
        let pending = Pending { abs, content: Content::New(stored), mime: opts.mime, expect: opts.expect };
        Ok(self.commit(vec![pending]).await?.remove(0))
    }

    /// Replace exact text in a file. Without an `Expect::Version`, a concurrent change to the
    /// file is retried against the new contents (up to three times).
    pub async fn edit(&self, path: &str, edits: &[Edit], expect: Expect) -> Result<EditOutcome> {
        let mut out = self.edit_many(vec![FileEdits { path: path.to_string(), edits: edits.to_vec(), expect }]).await?;
        Ok(out.remove(0))
    }

    /// Edit several files at once: either every edit applies or none does.
    pub async fn edit_many(&self, files: Vec<FileEdits>) -> Result<Vec<EditOutcome>> {
        self.need_write()?;
        if files.is_empty() {
            return Err(Error::InvalidInput("no files to edit".into()));
        }
        let mut seen = HashSet::new();
        let mut targets = Vec::with_capacity(files.len());
        for f in &files {
            let abs = self.abs(&f.path)?;
            if !seen.insert(abs.clone()) {
                return Err(Error::InvalidInput(format!("{} is listed twice", self.shown(&abs))));
            }
            targets.push(abs);
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut pending = Vec::with_capacity(files.len());
            let mut counts = Vec::with_capacity(files.len());
            let result: Result<()> = async {
                for (f, abs) in files.iter().zip(&targets) {
                    let (row, bytes) = self.text_bytes(abs).await?;
                    if let Expect::Version(v) = f.expect
                        && v != row.version
                    {
                        return Err(Error::Conflict { path: self.shown(abs), expected: v, actual: row.version });
                    }
                    let text = edit::as_text(&bytes).ok_or_else(|| Error::NotText(self.shown(abs)))?;
                    let (new_text, count) = edit::apply(text, &f.edits, &self.shown(abs))?;
                    let size = new_text.len() as u64;
                    if size > self.inner.cfg.max_text_bytes {
                        return Err(Error::TooLarge { size, limit: self.inner.cfg.max_text_bytes });
                    }
                    let stored = self.inner.blobs.put(self.id, Bytes::from(new_text), &guess_mime(abs)).await?;
                    pending.push(Pending {
                        abs: abs.clone(),
                        content: Content::New(stored),
                        mime: None,
                        expect: Expect::Version(row.version),
                    });
                    counts.push(count);
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                self.discard(&pending).await;
                return Err(e);
            }
            match self.commit(pending).await {
                Ok(entries) => {
                    return Ok(entries
                        .into_iter()
                        .zip(counts)
                        .map(|(entry, replacements)| EditOutcome { entry, replacements })
                        .collect());
                }
                // Someone changed a file between our read and our commit: redo the edits on
                // the new contents, unless the caller pinned the version they meant.
                Err(Error::Conflict { .. }) if attempt < 3 && files.iter().all(|f| f.expect == Expect::Any) => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Refuse early, before uploading anything, when the bytes plainly will not fit.
    pub(crate) async fn precheck(&self, abs: &VPath, size: u64) -> Result<()> {
        if abs.is_root() || *abs == self.access.root {
            return Err(Error::IsADirectory(self.shown(abs)));
        }
        let usage = self.usage().await?;
        if let Some(q) = usage.quota_bytes
            && usage.used_bytes.saturating_add(size) > q
        {
            return Err(Error::QuotaExceeded(crate::Quota::Bytes));
        }
        Ok(())
    }

    /// Delete freshly uploaded objects that will not be committed.
    pub(crate) async fn discard(&self, pending: &[Pending]) {
        let new: Vec<_> = pending
            .iter()
            .filter_map(|p| match &p.content {
                Content::New(s) => Some((self.id, s.id)),
                Content::Existing { .. } => None,
            })
            .collect();
        self.inner.blobs.delete(new).await;
    }

    /// Commit files in one transaction, all or none. New uploads are deleted again if the
    /// commit fails; blobs freed by version pruning are deleted once it succeeds.
    pub(crate) async fn commit(&self, pending: Vec<Pending>) -> Result<Vec<Entry>> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.commit_once(&pending).await {
                Ok((entries, freed)) => {
                    self.inner.blobs.delete(freed).await;
                    return Ok(entries);
                }
                // Two writers created the same new path at once; the loser retries and now
                // finds the file there.
                Err(e) if e.is_unique_violation() && attempt < 4 => continue,
                Err(e) => {
                    self.discard(&pending).await;
                    return Err(e);
                }
            }
        }
    }

    async fn commit_once(&self, pending: &[Pending]) -> Result<(Vec<Entry>, Vec<(Uuid, Uuid)>)> {
        let mut tx = self.tx().await?;
        let mut entries = Vec::with_capacity(pending.len());
        let mut dropped = Vec::new();
        for p in pending {
            let (row, pruned) = self.put_file(&mut tx, p).await?;
            entries.push(self.entry(row));
            dropped.extend(pruned);
        }
        let freed = db::collect_garbage(&mut tx, self.id, dropped).await?;
        tx.commit().await?;
        Ok((entries, freed))
    }

    /// Point `p.abs` at `p.content`, as a new file or a new version. Returns the node and the
    /// blobs of versions pruned past `Config::max_versions`.
    pub(crate) async fn put_file(&self, tx: &mut Tx, p: &Pending) -> Result<(NodeRow, Vec<Uuid>)> {
        let ws = self.id;
        let actor = self.access.actor.as_deref();
        if self.is_folder_root(&p.abs) {
            return Err(Error::IsADirectory(self.shown(&p.abs)));
        }
        let blob = p.content.blob();
        let size = p.content.size() as i64;
        match Self::find(tx, ws, &p.abs, Lock::Update).await? {
            Some(row) if row.is_dir() => Err(Error::IsADirectory(self.shown(&p.abs))),
            Some(row) => {
                match p.expect {
                    Expect::Absent => return Err(Error::AlreadyExists(self.shown(&p.abs))),
                    Expect::Version(v) if v != row.version => {
                        return Err(Error::Conflict { path: self.shown(&p.abs), expected: v, actual: row.version });
                    }
                    _ => {}
                }
                if let Content::New(s) = &p.content {
                    db::insert_blob(tx, ws, s.id, s.size, s.sha256.as_deref()).await?;
                }
                let version = row.version + 1;
                let updated: NodeRow = sqlx::query_as(concat!(
                    "UPDATE sf3_nodes SET blob_id = $2, size = $3, mime = coalesce($4, mime), version = $5,
                         updated_at = now(), updated_by = $6
                     WHERE id = $1 RETURNING ",
                    node_cols!()
                ))
                .bind(row.id)
                .bind(blob)
                .bind(size)
                .bind(p.mime.as_deref())
                .bind(version)
                .bind(actor)
                .fetch_one(&mut **tx)
                .await?;
                insert_version(tx, ws, row.id, version, blob, size, actor).await?;
                let pruned: Vec<Uuid> = sqlx::query_scalar(
                    "DELETE FROM sf3_versions WHERE node_id = $1 AND version <= $2 RETURNING blob_id",
                )
                .bind(row.id)
                .bind(version - self.inner.cfg.max_versions.max(1) as i32)
                .fetch_all(&mut **tx)
                .await?;
                Ok((updated, pruned))
            }
            None => {
                if let Expect::Version(_) = p.expect {
                    return Err(Error::NotFound(self.shown(&p.abs)));
                }
                if let Some(parent) = p.abs.parent() {
                    self.ensure_dirs(tx, &parent).await?;
                }
                db::charge_nodes(tx, ws, 1).await?;
                if let Content::New(s) = &p.content {
                    db::insert_blob(tx, ws, s.id, s.size, s.sha256.as_deref()).await?;
                }
                let mime = p.mime.clone().unwrap_or_else(|| guess_mime(&p.abs));
                let id = Uuid::now_v7();
                let row: NodeRow = sqlx::query_as(concat!(
                    "INSERT INTO sf3_nodes (id, workspace_id, path, depth, kind, blob_id, size, mime, version,
                         created_by, updated_by)
                     VALUES ($1, $2, $3, $4, 'file', $5, $6, $7, 1, $8, $8)
                     RETURNING ",
                    node_cols!()
                ))
                .bind(id)
                .bind(ws)
                .bind(p.abs.as_str())
                .bind(p.abs.depth())
                .bind(blob)
                .bind(size)
                .bind(mime)
                .bind(actor)
                .fetch_one(&mut **tx)
                .await?;
                insert_version(tx, ws, id, 1, blob, size, actor).await?;
                Ok((row, Vec::new()))
            }
        }
    }

    /// Make `dir` and every folder above it exist, creating what is missing (`mkdir -p`).
    ///
    /// Existing folders are locked `FOR SHARE` until the transaction ends, so a concurrent
    /// delete or move of one of them waits for this write instead of stranding it.
    pub(crate) async fn ensure_dirs(&self, tx: &mut Tx, dir: &VPath) -> Result<()> {
        if dir.is_root() {
            return Ok(());
        }
        let mut wanted = dir.ancestors();
        wanted.push(dir.clone());
        let paths: Vec<String> = wanted.iter().map(|p| p.to_string()).collect();
        for _ in 0..3 {
            let existing: Vec<(String, String)> = sqlx::query_as(
                "SELECT path, kind FROM sf3_nodes
                 WHERE workspace_id = $1 AND trash_id IS NULL AND path = ANY($2) FOR SHARE",
            )
            .bind(self.id)
            .bind(&paths)
            .fetch_all(&mut **tx)
            .await?;
            if let Some((path, _)) = existing.iter().find(|(_, kind)| kind != "dir") {
                return Err(Error::NotADirectory(self.shown_str(path)));
            }
            let have: HashSet<&str> = existing.iter().map(|(p, _)| p.as_str()).collect();
            let missing: Vec<&VPath> = wanted.iter().filter(|p| !have.contains(p.as_str())).collect();
            if missing.is_empty() {
                return Ok(());
            }
            db::charge_nodes(tx, self.id, missing.len() as i64).await?;
            let ids: Vec<Uuid> = missing.iter().map(|_| Uuid::now_v7()).collect();
            let mpaths: Vec<String> = missing.iter().map(|p| p.to_string()).collect();
            let depths: Vec<i32> = missing.iter().map(|p| p.depth()).collect();
            let created = sqlx::query(
                "INSERT INTO sf3_nodes (id, workspace_id, path, depth, kind, created_by, updated_by)
                 SELECT id, $1, path, depth, 'dir', $5, $5 FROM unnest($2::uuid[], $3::text[], $4::int[])
                     AS t(id, path, depth)
                 ON CONFLICT (workspace_id, path) WHERE trash_id IS NULL DO NOTHING",
            )
            .bind(self.id)
            .bind(&ids)
            .bind(&mpaths)
            .bind(&depths)
            .bind(self.access.actor.as_deref())
            .execute(&mut **tx)
            .await?
            .rows_affected() as i64;
            // Someone else created some of them first: give the count back and look again,
            // since what they created might be a file.
            db::charge_nodes(tx, self.id, created - missing.len() as i64).await?;
            if created == missing.len() as i64 {
                return Ok(());
            }
        }
        Err(Error::InvalidInput("the folders kept changing; try again".into()))
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Lock {
    None,
    Share,
    Update,
}

pub(crate) async fn insert_version(
    tx: &mut Tx,
    ws: Uuid,
    node: Uuid,
    version: i32,
    blob: Uuid,
    size: i64,
    actor: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO sf3_versions (node_id, version, workspace_id, blob_id, size, created_by)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(node)
    .bind(version)
    .bind(ws)
    .bind(blob)
    .bind(size)
    .bind(actor)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn usage_of(tx: &mut Tx, ws: Uuid) -> Result<Usage> {
    let (used, nodes, qb, qn): (i64, i64, Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT used_bytes, node_count, quota_bytes, quota_nodes FROM sf3_workspaces WHERE id = $1")
            .bind(ws)
            .fetch_one(&mut **tx)
            .await?;
    Ok(Usage {
        used_bytes: used.max(0) as u64,
        node_count: nodes.max(0) as u64,
        quota_bytes: qb.map(|q| q as u64),
        quota_nodes: qn.map(|q| q as u64),
    })
}

pub(crate) fn guess_mime(abs: &VPath) -> String {
    mime_guess::from_path(abs.name().unwrap_or_default()).first_raw().unwrap_or("application/octet-stream").to_string()
}
