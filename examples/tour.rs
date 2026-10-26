//! A tour of every sysfile3 feature, top to bottom. It is the code the guide (docs/guide.md)
//! walks through, so each step there is something that really runs.
//!
//! It needs a Postgres it may create tables in; file contents stay in memory.
//!
//! ```sh
//! docker run --rm -d --name sf3-demo -e POSTGRES_PASSWORD=demo -p 55433:5432 postgres:17-alpine
//! DATABASE_URL=postgres://postgres:demo@127.0.0.1:55433/postgres cargo run --example tour
//! docker stop sf3-demo
//! ```

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::{TryStreamExt, stream};
use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use object_store::memory::InMemory;
use sqlx::postgres::PgPoolOptions;
use sysfile3::{Access, Config, Edit, Error, Expect, FileEdits, GrepOptions, ListOptions, Sysfile3, WriteOptions};

fn step(title: &str) {
    println!("\n== {title}");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---- 1. Set up --------------------------------------------------------------------------
    step("1. Set up");
    let url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://postgres:demo@127.0.0.1:55433/postgres".into());
    let pool = PgPoolOptions::new().max_connections(16).connect(&url).await?;

    // In production this is your R2 / S3 bucket (see the guide). Here: memory.
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    // A signer turns paths into signed URLs. It signs locally, so fake credentials are fine
    // for the tour; in production pass the same AmazonS3 you use as the store.
    let signer = Arc::new(
        AmazonS3Builder::new()
            .with_endpoint("https://ACCOUNT.r2.cloudflarestorage.com")
            .with_region("auto")
            .with_bucket_name("demo")
            .with_access_key_id("demo")
            .with_secret_access_key("demo")
            .build()?,
    );
    let config = Config { default_quota_bytes: Some(50 * 1024 * 1024), ..Config::default() };
    let fs = Sysfile3::builder(pool, store).signer(signer).config(config).build();
    fs.migrate().await?;
    println!("tables ready");

    // ---- 2. Workspaces and access -----------------------------------------------------------
    step("2. Workspaces and access");
    let user = format!("tour-{}", uuid::Uuid::new_v4().simple());
    let me = fs.workspace("tour", &user, Access::full().actor(&user)).await?;
    let agent = fs.workspace("tour", &user, Access::full().actor("agent:main")).await?;
    let researcher =
        fs.workspace("tour", &user, Access::full().within("/agents/research")?.actor("agent:research")).await?;
    let viewer = fs.workspace("tour", &user, Access::read_only()).await?;
    println!("workspace {} opened four ways", me.id());

    // ---- 3. Writing ---------------------------------------------------------------------------
    step("3. Writing");
    let e = agent.write("/identity.md", "# Identity\nName: Juno\n", Default::default()).await?;
    println!("{} v{} {} bytes, {:?}, by {:?}", e.path, e.version, e.size, e.mime, e.created_by);
    let only_new = WriteOptions { expect: Expect::Absent, mime: None };
    match agent.write("/identity.md", "again", only_new).await {
        Err(Error::AlreadyExists(p)) => println!("Expect::Absent refused to overwrite {p}"),
        other => panic!("{other:?}"),
    }
    me.write("/library/report.pdf", b"%PDF-1.7 ...".to_vec(), Default::default()).await?;
    me.write(
        "/library/data.bin",
        vec![0u8; 1024],
        WriteOptions { mime: Some("application/x-custom".into()), ..Default::default() },
    )
    .await?;

    // Big files: stream them in.
    let chunks = (0..4).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![7u8; 256 * 1024])));
    let e = me.write_stream("/library/video.mp4", stream::iter(chunks), Default::default()).await?;
    println!("streamed {} ({} bytes)", e.path, e.size);

    // Or let the client upload straight to storage with a signed URL.
    let ticket = me.begin_upload("/library/photo.jpg", 2_000_000, Default::default()).await?;
    println!("client PUTs to {}…", &ticket.url[..60]);
    // ...the client uploads, then you call me.finish_upload(ticket.id, Expect::Any)
    me.cancel_upload(ticket.id).await?;

    // ---- 4. Reading ---------------------------------------------------------------------------
    step("4. Reading");
    println!("stat: {:?}", me.stat("/identity.md").await?.version);
    println!("exists /nope: {}", me.exists("/nope").await?);
    println!("read: {:?}", String::from_utf8(me.read("/identity.md").await?.to_vec())?);
    let w = me.read_text("/identity.md", 2, Some(10)).await?;
    println!("read_text from line {}: {:?} of {} lines", w.first_line, w.lines, w.total_lines);
    println!("read_range: {:?}", me.read_range("/library/report.pdf", 0, 8).await?);
    let (entry, body) = me.open("/library/video.mp4").await?;
    let chunks: Vec<Bytes> = body.try_collect().await?;
    println!("open: streamed {} of {} bytes", chunks.iter().map(|c| c.len()).sum::<usize>(), entry.size);
    println!("download_url: {}…", &me.download_url("/library/report.pdf").await?[..60]);

    let page = me.list("/", ListOptions { depth: 2, limit: Some(3), after: None }).await?;
    println!("list page 1: {:?}", page.entries.iter().map(|e| &e.path).collect::<Vec<_>>());
    if let Some(next) = page.next {
        let page2 = me.list("/", ListOptions { depth: 2, limit: Some(3), after: Some(next) }).await?;
        println!("list page 2: {:?}", page2.entries.iter().map(|e| &e.path).collect::<Vec<_>>());
    }

    // ---- 5. Editing ---------------------------------------------------------------------------
    step("5. Editing");
    agent.write("/user.md", "# User\nName: (unknown)\nTimezone: (unknown)\n", Default::default()).await?;
    let out = agent.edit("/user.md", &[Edit::new("Name: (unknown)", "Name: Rahim")], Expect::Any).await?;
    println!("edit: {} replacement, now v{}", out.replacements, out.entry.version);
    let cur = agent.read_text("/user.md", 1, None).await?;
    agent.edit("/user.md", &[Edit::new("(unknown)", "Asia/Dhaka")], Expect::Version(cur.version)).await?;
    match agent.edit("/user.md", &[Edit::new("Asia", "Europe")], Expect::Version(cur.version)).await {
        Err(Error::Conflict { expected, actual, .. }) => println!("pinned edit refused: v{expected} is now v{actual}"),
        other => panic!("{other:?}"),
    }
    // Several files, all or nothing: if one edit fails, none is applied.
    let both = agent
        .edit_many(vec![
            FileEdits { path: "/user.md".into(), edits: vec![Edit::new("Rahim", "Rahim K.")], expect: Expect::Any },
            FileEdits { path: "/identity.md".into(), edits: vec![Edit::all("Juno", "Juno AI")], expect: Expect::Any },
        ])
        .await?;
    println!("edit_many changed {} files together", both.len());

    // ---- 6. Organising ------------------------------------------------------------------------
    step("6. Organising");
    me.mkdir("/projects/launch/assets", true).await?;
    me.rename("/library/report.pdf", "/projects/launch/report.pdf", false).await?;
    me.copy("/projects/launch", "/projects/launch-v2", false).await?;
    println!("copied a folder; bytes are shared, not duplicated");
    let t = me.delete("/projects/launch-v2", true).await?;
    println!("deleted {} into the trash ({})", t.path, t.id);

    // ---- 7. Trash -----------------------------------------------------------------------------
    step("7. Trash");
    println!("trash: {:?}", me.list_trash().await?.iter().map(|i| &i.path).collect::<Vec<_>>());
    me.restore(t.id).await?;
    println!("restored /projects/launch-v2");
    let t = me.delete("/projects/launch-v2", true).await?;
    me.purge(t.id).await?;
    println!("purged it for good; empty_trash() purges everything");
    me.empty_trash().await?;

    // ---- 8. Versions --------------------------------------------------------------------------
    step("8. Versions");
    for v in me.versions("/user.md").await? {
        println!("v{} {} bytes by {:?}", v.version, v.size, v.created_by);
    }
    println!("v1 was: {:?}", String::from_utf8(me.read_version("/user.md", 1).await?.to_vec())?);
    let e = me.restore_version("/user.md", 1).await?;
    println!("restored v1 as v{}", e.version);

    // ---- 9. Search ----------------------------------------------------------------------------
    step("9. Search");
    me.write("/notes/a.md", "TODO: call the bank\nbuy milk\n", Default::default()).await?;
    me.write("/notes/b.txt", "todo later\n", Default::default()).await?;
    let g = me.glob("**/*.md", "/").await?;
    println!("glob **/*.md: {:?}", g.entries.iter().map(|e| &e.path).collect::<Vec<_>>());
    let r =
        me.grep(GrepOptions { ignore_case: true, include: Some("*.md".into()), ..GrepOptions::new("todo") }).await?;
    for m in &r.matches {
        println!("grep: {}:{}: {}", m.path, m.line, m.text);
    }

    // ---- 10. Confined agents and read-only viewers ----------------------------------------------
    step("10. Confined and read-only handles");
    researcher.write("/out/findings.md", "found it\n", Default::default()).await?;
    println!(
        "the researcher wrote /out/findings.md, which is {:?} to the owner",
        me.stat("/agents/research/out/findings.md").await?.path
    );
    println!("the researcher cannot see /identity.md: {}", researcher.read("/identity.md").await.unwrap_err());
    println!("the viewer cannot write: {}", viewer.write("/x", "y", Default::default()).await.unwrap_err());

    // ---- 11. Sharing ----------------------------------------------------------------------------
    step("11. Sharing");
    me.write("/site/index.html", "<h1>Launch</h1>", Default::default()).await?;
    let link = me.share("/site", Some(Duration::from_secs(7 * 86_400))).await?;
    println!("share token (show once, put it in your URL): {}…", &link.token[..16]);
    let public = fs.open_share(&link.token).await?;
    println!("anyone with the link reads: {:?}", String::from_utf8(public.read("/index.html").await?.to_vec())?);
    println!("live links: {}", me.shares().await?.len());
    me.revoke_share(link.info.id).await?;
    println!("after revoke: {}", fs.open_share(&link.token).await.err().unwrap());

    // ---- 12. Storage and quotas ---------------------------------------------------------------
    step("12. Storage and quotas");
    let u = me.usage().await?;
    println!(
        "used {:.2} MiB of the plan ({:.3}%), {} files and folders",
        u.used_mib(),
        u.used_percent().unwrap_or(0.0),
        u.node_count
    );
    println!("project size: {:?}", me.disk_usage("/projects/launch").await?);
    let b = me.breakdown().await?;
    println!("files {} B, old versions {} B, trash {} B", b.file_bytes, b.version_bytes, b.trash_bytes);
    for t in &b.by_type {
        println!("  {:<10} {:>3} files {:>9} B", t.category, t.files, t.bytes);
    }
    println!("largest: {:?}", me.largest_files("/", 3).await?.iter().map(|e| (&e.path, e.size)).collect::<Vec<_>>());
    let u = fs.set_quota("tour", &user, Some(1 << 30), Some(100_000)).await?;
    println!("upgraded: {:?} bytes left", u.remaining_bytes());
    println!("whole app: {:?}", fs.app_usage("tour").await?);
    println!("billing page: {} workspaces", fs.usage_report("tour", None, 100).await?.len());

    // ---- 13. Errors -------------------------------------------------------------------------
    step("13. Errors");
    for err in [
        me.read("/missing").await.unwrap_err(),
        me.write("/a/../b", "x", Default::default()).await.unwrap_err(),
        viewer.delete("/notes", true).await.unwrap_err(),
    ] {
        println!("{:<18} client error: {:<5} {err}", err.code(), err.is_client_error());
    }

    // ---- 14. Maintenance --------------------------------------------------------------------
    step("14. Maintenance");
    println!("purged trash items: {}", fs.purge_trash_older_than(Duration::from_secs(30 * 86_400)).await?);
    println!("expired uploads: {}", fs.expire_uploads().await?);
    println!("orphaned objects: {}", fs.sweep_orphans(Duration::from_secs(86_400)).await?);
    println!("deleted the workspace: {} objects", fs.purge_workspace("tour", &user).await?);
    Ok(())
}
