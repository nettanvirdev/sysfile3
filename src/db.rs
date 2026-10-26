//! Transactions, migrations and the row types shared by the operations.

use std::ops::{Deref, DerefMut};

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::error::{Error, Quota, Result};

/// A transaction that also collects changes to its workspace's usage counters.
///
/// The counters live on the workspace row, which every write to that workspace touches. Taking
/// that row's lock early would make concurrent writers to one workspace queue for the whole of
/// each other's transactions, so the changes are added up as the transaction goes and applied in
/// a single statement right before commit: the lock is held for one round trip, and the quota
/// check is still exact (the statement refuses to cross a limit).
pub(crate) struct Tx {
    inner: Transaction<'static, Postgres>,
    usage: Option<(Uuid, i64, i64)>,
}

impl Deref for Tx {
    type Target = PgConnection;
    fn deref(&self) -> &PgConnection {
        &self.inner
    }
}

impl DerefMut for Tx {
    fn deref_mut(&mut self) -> &mut PgConnection {
        &mut self.inner
    }
}

impl Tx {
    fn add(&mut self, ws: Uuid, bytes: i64, nodes: i64) {
        let (id, b, n) = self.usage.get_or_insert((ws, 0, 0));
        assert_eq!(*id, ws, "one transaction changes one workspace's usage");
        *b += bytes;
        *n += nodes;
    }

    /// Apply the collected usage, refusing to cross a quota, then commit.
    pub async fn commit(mut self) -> Result<()> {
        if let Some((ws, bytes, nodes)) = self.usage.take()
            && (bytes != 0 || nodes != 0)
        {
            let done = sqlx::query(
                "UPDATE sf3_workspaces
                 SET used_bytes = used_bytes + $2, node_count = node_count + $3
                 WHERE id = $1
                   AND ($2 <= 0 OR quota_bytes IS NULL OR used_bytes + $2 <= quota_bytes)
                   AND ($3 <= 0 OR quota_nodes IS NULL OR node_count + $3 <= quota_nodes)",
            )
            .bind(ws)
            .bind(bytes)
            .bind(nodes)
            .execute(&mut *self.inner)
            .await?;
            if done.rows_affected() == 0 {
                let over_bytes: bool = sqlx::query_scalar(
                    "SELECT $2 > 0 AND quota_bytes IS NOT NULL AND used_bytes + $2 > quota_bytes
                     FROM sf3_workspaces WHERE id = $1",
                )
                .bind(ws)
                .bind(bytes)
                .fetch_one(&mut *self.inner)
                .await?;
                return Err(Error::QuotaExceeded(if over_bytes { Quota::Bytes } else { Quota::Nodes }));
            }
        }
        self.inner.commit().await?;
        Ok(())
    }
}

/// Which rows a transaction may see. Row-level security reads this from the session setting.
pub(crate) enum Scope {
    Workspace(Uuid),
    /// Maintenance across every workspace (purging, sweeping, share lookup).
    All,
}

pub(crate) async fn begin(pool: &PgPool, scope: Scope) -> Result<Tx> {
    let mut tx = Tx { inner: pool.begin().await?, usage: None };
    let value = match scope {
        Scope::Workspace(id) => id.to_string(),
        Scope::All => "*".to_string(),
    };
    sqlx::query("SELECT set_config('sysfile3.workspace', $1, true)").bind(value).execute(&mut *tx).await?;
    Ok(tx)
}

const MIGRATIONS: &[(i32, &str)] = &[(1, include_str!("../migrations/0001_init.sql"))];

/// Apply any migrations not applied yet. Safe to run from many processes at once: they queue on
/// an advisory lock and the later ones find nothing to do.
pub(crate) async fn migrate(pool: &PgPool) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(7311003)").execute(&mut *tx).await?;
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS sf3_migrations (
             version integer PRIMARY KEY,
             applied_at timestamptz NOT NULL DEFAULT now()
         )",
    )
    .execute(&mut *tx)
    .await?;
    let applied: Vec<i32> = sqlx::query_scalar("SELECT version FROM sf3_migrations").fetch_all(&mut *tx).await?;
    for (version, sql) in MIGRATIONS {
        if applied.contains(version) {
            continue;
        }
        sqlx::raw_sql(*sql).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO sf3_migrations (version) VALUES ($1)").bind(version).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The columns of `sf3_nodes` that make up a `NodeRow`, for `concat!` into queries.
macro_rules! node_cols {
    () => {
        "id, path, kind, blob_id, size, mime, version, created_at, created_by, updated_at, updated_by"
    };
}
pub(crate) use node_cols;

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct NodeRow {
    pub id: Uuid,
    pub path: String,
    pub kind: String,
    pub blob_id: Option<Uuid>,
    pub size: i64,
    pub mime: Option<String>,
    pub version: i32,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub updated_by: Option<String>,
}

impl NodeRow {
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
}

/// Add `delta` bytes to the workspace's usage when the transaction commits.
pub(crate) async fn charge_bytes(tx: &mut Tx, ws: Uuid, delta: i64) -> Result<()> {
    tx.add(ws, delta, 0);
    Ok(())
}

/// Add `delta` files or folders to the workspace's count when the transaction commits.
pub(crate) async fn charge_nodes(tx: &mut Tx, ws: Uuid, delta: i64) -> Result<()> {
    tx.add(ws, 0, delta);
    Ok(())
}

/// Record a new blob and charge its bytes.
pub(crate) async fn insert_blob(tx: &mut Tx, ws: Uuid, id: Uuid, size: u64, sha256: Option<&[u8]>) -> Result<()> {
    sqlx::query("INSERT INTO sf3_blobs (id, workspace_id, size, sha256) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(ws)
        .bind(size as i64)
        .bind(sha256)
        .execute(&mut **tx)
        .await?;
    charge_bytes(tx, ws, size as i64).await
}

/// Delete the blobs among `candidates` that nothing points at any more, refund their bytes, and
/// return them so their objects can be deleted once the transaction commits.
pub(crate) async fn collect_garbage(tx: &mut Tx, ws: Uuid, candidates: Vec<Uuid>) -> Result<Vec<(Uuid, Uuid)>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let freed: Vec<(Uuid, i64)> = sqlx::query_as(
        "DELETE FROM sf3_blobs b
         WHERE b.workspace_id = $1 AND b.id = ANY($2)
           AND NOT EXISTS (SELECT 1 FROM sf3_versions v WHERE v.blob_id = b.id)
           AND NOT EXISTS (SELECT 1 FROM sf3_nodes n WHERE n.blob_id = b.id)
         RETURNING b.id, b.size",
    )
    .bind(ws)
    .bind(&candidates)
    .fetch_all(&mut **tx)
    .await?;
    let bytes: i64 = freed.iter().map(|(_, size)| size).sum();
    charge_bytes(tx, ws, -bytes).await?;
    Ok(freed.into_iter().map(|(id, _)| (ws, id)).collect())
}
