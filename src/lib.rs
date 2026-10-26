//! # sysfile3
//!
//! A multi-tenant virtual file system for AI agents and the apps around them: Postgres holds the
//! tree, any S3-compatible store (Cloudflare R2, AWS S3, MinIO, SeaweedFS) holds the bytes.
//!
//! ```no_run
//! # async fn demo(pool: sqlx::PgPool, store: std::sync::Arc<dyn object_store::ObjectStore>) -> sysfile3::Result<()> {
//! use sysfile3::{Access, Edit, Sysfile3};
//!
//! let fs = Sysfile3::builder(pool, store).build();
//! fs.migrate().await?;
//!
//! // Every operation goes through a handle bound to one workspace.
//! let ws = fs.workspace("my-app", "user-42", Access::full().actor("user-42")).await?;
//! ws.write("/notes/todo.md", "- ship it\n", Default::default()).await?;
//! ws.edit("/notes/todo.md", &[Edit::new("ship it", "ship it today")], Default::default()).await?;
//! let text = ws.read_text("/notes/todo.md", 1, None).await?;
//! # Ok(()) }
//! ```
//!
//! See the README for the design: isolation, quotas, versions, trash, shares and caching.

mod admin;
mod config;
mod db;
mod edit;
mod error;
mod model;
mod path;
mod search;
mod share;
mod store;
mod tree;
mod upload;
mod workspace;

use std::sync::Arc;

use moka::future::Cache;
use object_store::ObjectStore;
use object_store::signer::Signer;
use sqlx::PgPool;
use uuid::Uuid;

pub use crate::config::Config;
pub use crate::edit::Edit;
pub use crate::error::{Error, Quota, Result};
pub use crate::model::*;
pub use crate::path::{MAX_DEPTH, MAX_NAME_BYTES, MAX_PATH_BYTES, VPath};
pub use crate::search::GrepOptions;
pub use crate::workspace::{EditOutcome, Expect, FileEdits, ListOptions, Workspace, WriteOptions};

use crate::store::Blobs;

/// What a handle may do and where. Build one per request from what your app has already
/// authenticated: the user's own files get `Access::full()`, an agent session might get
/// `Access::full().within("/agents/a1")`, a viewer `Access::read_only()`.
#[derive(Debug, Clone)]
pub struct Access {
    pub(crate) read: bool,
    pub(crate) write: bool,
    pub(crate) share: bool,
    pub(crate) root: VPath,
    pub(crate) actor: Option<String>,
}

impl Access {
    /// Read, write and share everything in the workspace.
    pub fn full() -> Self {
        Access { read: true, write: true, share: true, root: VPath::root(), actor: None }
    }

    /// Read everything, change nothing.
    pub fn read_only() -> Self {
        Access { read: true, write: false, share: false, root: VPath::root(), actor: None }
    }

    /// Without the right to create public links.
    pub fn no_share(mut self) -> Self {
        self.share = false;
        self
    }

    /// Confine the handle to a folder: its `/` is that folder, and nothing outside can be named.
    pub fn within(mut self, folder: &str) -> Result<Self> {
        self.root = self.root.join(&VPath::parse(folder)?);
        Ok(self)
    }

    /// Who is acting, recorded on every change (`created_by`, `updated_by`, `trashed_by`).
    pub fn actor(mut self, id: impl Into<String>) -> Self {
        self.actor = Some(id.into());
        self
    }

    pub fn can_read(&self) -> bool {
        self.read
    }

    pub fn can_write(&self) -> bool {
        self.write
    }

    pub fn can_share(&self) -> bool {
        self.share
    }

    pub fn root(&self) -> &VPath {
        &self.root
    }
}

pub(crate) struct Inner {
    pub pool: PgPool,
    pub blobs: Blobs,
    pub cfg: Config,
    /// (app, external id) to workspace id. The mapping never changes while the workspace exists.
    pub ids: Cache<(String, String), Uuid>,
}

/// The file system: a Postgres pool, an object store and the limits. Cheap to clone; share one
/// per process.
#[derive(Clone)]
pub struct Sysfile3 {
    pub(crate) inner: Arc<Inner>,
}

pub struct Builder {
    pool: PgPool,
    store: Arc<dyn ObjectStore>,
    signer: Option<Arc<dyn Signer>>,
    cfg: Config,
}

impl Builder {
    pub fn config(mut self, cfg: Config) -> Self {
        self.cfg = cfg;
        self
    }

    /// Enables signed upload and download URLs. For S3 and R2, pass the same `AmazonS3` you
    /// passed as the store.
    pub fn signer(mut self, signer: Arc<dyn Signer>) -> Self {
        self.signer = Some(signer);
        self
    }

    pub fn build(self) -> Sysfile3 {
        let blobs = Blobs::new(self.store, self.signer, &self.cfg);
        Sysfile3 {
            inner: Arc::new(Inner {
                pool: self.pool,
                blobs,
                cfg: self.cfg,
                ids: Cache::builder().max_capacity(100_000).build(),
            }),
        }
    }
}

impl Sysfile3 {
    pub fn builder(pool: PgPool, store: Arc<dyn ObjectStore>) -> Builder {
        Builder { pool, store, signer: None, cfg: Config::default() }
    }

    pub fn config(&self) -> &Config {
        &self.inner.cfg
    }

    /// Create or update the tables. Run once at startup; concurrent runs are safe.
    pub async fn migrate(&self) -> Result<()> {
        db::migrate(&self.inner.pool).await
    }

    /// A handle on one workspace, created on first use with the default quotas.
    ///
    /// `app` separates products sharing one database; `external_id` is your user, team or
    /// project id. Neither is ever shown to a handle's caller.
    pub async fn workspace(&self, app: &str, external_id: &str, access: Access) -> Result<Workspace> {
        let id = self.workspace_id(app, external_id).await?;
        Ok(Workspace { inner: self.inner.clone(), id, access })
    }

    pub(crate) async fn workspace_id(&self, app: &str, external_id: &str) -> Result<Uuid> {
        check_id("app", app)?;
        check_id("external id", external_id)?;
        let key = (app.to_string(), external_id.to_string());
        if let Some(id) = self.inner.ids.get(&key).await {
            return Ok(id);
        }
        let mut tx = db::begin(&self.inner.pool, db::Scope::All).await?;
        let cfg = &self.inner.cfg;
        sqlx::query(
            "INSERT INTO sf3_workspaces (id, app, external_id, quota_bytes, quota_nodes)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (app, external_id) DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(app)
        .bind(external_id)
        .bind(cfg.default_quota_bytes.map(|q| q as i64))
        .bind(cfg.default_quota_nodes.map(|q| q as i64))
        .execute(&mut *tx)
        .await?;
        let id: Uuid = sqlx::query_scalar("SELECT id FROM sf3_workspaces WHERE app = $1 AND external_id = $2")
            .bind(app)
            .bind(external_id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        self.inner.ids.insert(key, id).await;
        Ok(id)
    }
}

fn check_id(what: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
        return Err(Error::InvalidInput(format!("{what} must be 1 to 200 printable characters")));
    }
    Ok(())
}
