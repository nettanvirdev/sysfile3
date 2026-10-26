//! Test harness: a fresh database per test, owned by an ordinary (non-superuser) role so
//! row-level security is really enforced, and an in-memory object store.
//!
//! Point `SYSFILE3_TEST_DB` at a Postgres superuser URL, for example a throwaway container:
//!
//! ```sh
//! docker run --rm -d --name sf3-test-pg -e POSTGRES_PASSWORD=sf3 -p 55432:5432 postgres:17-alpine
//! ```
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::signer::{Method, Signer, Url};
use object_store::throttle::{ThrottleConfig, ThrottledStore};
use object_store::{ObjectStore, ObjectStoreExt};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use sysfile3::{Access, Config, Sysfile3, Workspace};
use uuid::Uuid;

pub const DEFAULT_ADMIN_URL: &str = "postgres://postgres:sf3@127.0.0.1:55432/postgres";

pub fn admin_url() -> String {
    std::env::var("SYSFILE3_TEST_DB").unwrap_or_else(|_| DEFAULT_ADMIN_URL.into())
}

pub struct Fixture {
    pub fs: Sysfile3,
    pub store: Arc<InMemory>,
    /// Connected as the same non-superuser role sysfile3 uses.
    pub pool: PgPool,
}

/// Signs nothing: returns a recognisable URL so tests can follow the upload flow.
#[derive(Debug)]
pub struct FakeSigner;

#[async_trait]
impl Signer for FakeSigner {
    async fn signed_url(&self, method: Method, path: &Path, expires_in: Duration) -> object_store::Result<Url> {
        Ok(Url::parse(&format!("https://signed.test/{path}?method={method}&ttl={}", expires_in.as_secs())).unwrap())
    }
}

pub async fn fixture() -> Fixture {
    fixture_with(Config::default(), 16).await
}

pub async fn fixture_with(cfg: Config, connections: u32) -> Fixture {
    fixture_full(cfg, connections, None).await
}

/// With `throttle`, every storage call waits like a remote object store would (R2, S3).
pub async fn fixture_full(cfg: Config, connections: u32, throttle: Option<ThrottleConfig>) -> Fixture {
    let admin = admin_url();
    let mut conn = PgConnection::connect(&admin)
        .await
        .unwrap_or_else(|e| panic!("cannot reach the test database at {admin} ({e}); see tests/common/mod.rs"));
    sqlx::raw_sql(
        "DO $$ BEGIN
             CREATE ROLE sf3_app LOGIN PASSWORD 'sf3_app';
         EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
         END $$",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    let db = format!("sf3_{}", Uuid::new_v4().simple());
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {db} OWNER sf3_app"))).execute(&mut conn).await.unwrap();
    let mut url = url_of(&admin);
    url.set_username("sf3_app").unwrap();
    url.set_password(Some("sf3_app")).unwrap();
    url.set_path(&db);
    let pool = PgPoolOptions::new().max_connections(connections).connect(url.as_str()).await.unwrap();
    let store = Arc::new(InMemory::new());
    let backend: Arc<dyn ObjectStore> = match throttle {
        Some(t) => Arc::new(ThrottledStore::new(store.clone() as Arc<dyn ObjectStore>, t)),
        None => store.clone(),
    };
    let fs = Sysfile3::builder(pool.clone(), backend).signer(Arc::new(FakeSigner)).config(cfg).build();
    fs.migrate().await.unwrap();
    Fixture { fs, store, pool }
}

fn url_of(s: &str) -> object_store::signer::Url {
    object_store::signer::Url::parse(s).unwrap()
}

impl Fixture {
    pub async fn ws(&self, user: &str) -> Workspace {
        self.fs.workspace("test", user, Access::full().actor(user)).await.unwrap()
    }

    /// Number of objects in the store.
    pub async fn objects(&self) -> usize {
        use futures::TryStreamExt;
        let all: Vec<_> = self.store.list(None).try_collect().await.unwrap();
        all.len()
    }

    /// Simulate a client uploading to a signed URL from `FakeSigner`.
    pub async fn client_put(&self, url: &str, bytes: Vec<u8>) {
        let path = url.trim_start_matches("https://signed.test/").split('?').next().unwrap();
        self.store.put(&Path::from(path), bytes.into()).await.unwrap();
    }
}
