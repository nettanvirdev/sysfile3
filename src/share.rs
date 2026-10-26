//! Read-only links to a file or folder.
//!
//! A link is a random token (244 bits). sysfile3 stores only its SHA-256, so the token is shown
//! once, when the link is made. Opening a link gives a read-only handle confined to the shared
//! node, so the rest of the workspace cannot be named through it. A link follows its file when
//! it is moved or renamed, stops working while the file is in the trash, and is deleted with it.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::db::{self, Scope};
use crate::error::{Error, Result};
use crate::model::{Kind, NewShare, ShareInfo};
use crate::path::VPath;
use crate::workspace::{Lock, Workspace};
use crate::{Access, Sysfile3};

fn hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

impl Workspace {
    /// Make a read-only link to a file or folder, optionally expiring.
    pub async fn share(&self, path: &str, expires_in: Option<Duration>) -> Result<NewShare> {
        self.need_share()?;
        let abs = self.abs(path)?;
        let mut tx = self.tx().await?;
        let row = match Self::find(&mut tx, self.id, &abs, Lock::Share).await? {
            Some(row) => row,
            None if self.is_folder_root(&abs) => {
                return Err(Error::InvalidInput("share a file or folder inside it instead".into()));
            }
            None => return Err(Error::NotFound(self.shown(&abs))),
        };
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let expires_at = match expires_in {
            Some(d) => Some(
                Utc::now()
                    + chrono::Duration::from_std(d).map_err(|_| Error::InvalidInput("expiry too far away".into()))?,
            ),
            None => None,
        };
        let id = Uuid::now_v7();
        let (created_at,): (DateTime<Utc>,) = sqlx::query_as(
            "INSERT INTO sf3_shares (id, workspace_id, node_id, token_hash, created_by, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING created_at",
        )
        .bind(id)
        .bind(self.id)
        .bind(row.id)
        .bind(hash(&token))
        .bind(self.access.actor.as_deref())
        .bind(expires_at)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        let info = ShareInfo {
            id,
            path: self.shown(&abs),
            kind: Kind::from_db(&row.kind),
            created_at,
            created_by: self.access.actor.clone(),
            expires_at,
        };
        Ok(NewShare { info, token })
    }

    /// Live links to anything under this handle's root (not revoked, not expired, not trashed).
    pub async fn shares(&self) -> Result<Vec<ShareInfo>> {
        self.need_read()?;
        let root = &self.access.root;
        let (lo, hi) = root.subtree();
        let mut tx = self.tx().await?;
        let rows: Vec<(Uuid, String, String, DateTime<Utc>, Option<String>, Option<DateTime<Utc>>)> = sqlx::query_as(
            "SELECT s.id, n.path, n.kind, s.created_at, s.created_by, s.expires_at
                 FROM sf3_shares s JOIN sf3_nodes n ON n.id = s.node_id
                 WHERE s.workspace_id = $1 AND s.revoked_at IS NULL
                   AND (s.expires_at IS NULL OR s.expires_at > now()) AND n.trash_id IS NULL
                   AND (n.path = $2 OR (n.path >= $3 AND n.path < $4))
                 ORDER BY s.created_at DESC",
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
            .map(|(id, path, kind, created_at, created_by, expires_at)| ShareInfo {
                id,
                path: self.shown_str(&path),
                kind: Kind::from_db(&kind),
                created_at,
                created_by,
                expires_at,
            })
            .collect())
    }

    /// Turn a link off for good.
    pub async fn revoke_share(&self, share_id: Uuid) -> Result<()> {
        self.need_share()?;
        let mut tx = self.tx().await?;
        let path: Option<String> = sqlx::query_scalar(
            "SELECT n.path FROM sf3_shares s JOIN sf3_nodes n ON n.id = s.node_id
             WHERE s.workspace_id = $1 AND s.id = $2 AND s.revoked_at IS NULL FOR UPDATE OF s",
        )
        .bind(self.id)
        .bind(share_id)
        .fetch_optional(&mut *tx)
        .await?;
        if !path.is_some_and(|p| VPath::trusted(p).within(&self.access.root)) {
            return Err(Error::ShareNotFound);
        }
        sqlx::query("UPDATE sf3_shares SET revoked_at = now() WHERE id = $1").bind(share_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}

impl Sysfile3 {
    /// A read-only handle on whatever a link shares: `stat("/")` is the shared file or folder,
    /// and for a folder, `list` and `read` work inside it.
    pub async fn open_share(&self, token: &str) -> Result<Workspace> {
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::ShareNotFound);
        }
        let mut tx = db::begin(&self.inner.pool, Scope::All).await?;
        let found: Option<(Uuid, String)> = sqlx::query_as(
            "SELECT s.workspace_id, n.path FROM sf3_shares s JOIN sf3_nodes n ON n.id = s.node_id
             WHERE s.token_hash = $1 AND s.revoked_at IS NULL
               AND (s.expires_at IS NULL OR s.expires_at > now()) AND n.trash_id IS NULL",
        )
        .bind(hash(&token.to_ascii_lowercase()))
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let (id, path) = found.ok_or(Error::ShareNotFound)?;
        let access = Access { root: VPath::trusted(path), ..Access::read_only() };
        Ok(Workspace { inner: self.inner.clone(), id, access })
    }
}
