//! Every operation, its edge cases, and the guarantees around them.

mod common;

use std::time::Duration;

use bytes::Bytes;
use common::{fixture, fixture_with};
use futures::{StreamExt, TryStreamExt, stream};
use sysfile3::{Access, Config, Edit, Error, Expect, FileEdits, GrepOptions, ListOptions, Quota, WriteOptions};

fn new() -> WriteOptions {
    WriteOptions { expect: Expect::Absent, mime: None }
}

#[tokio::test]
async fn write_read_stat_and_auto_parents() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    let e = ws.write("/a/b/c.txt", "hello", Default::default()).await.unwrap();
    assert_eq!((e.path.as_str(), e.size, e.version), ("/a/b/c.txt", 5, 1));
    assert_eq!(e.mime.as_deref(), Some("text/plain"));
    assert_eq!(e.created_by.as_deref(), Some("u1"));
    assert_eq!(ws.read("a/b/c.txt").await.unwrap(), "hello");
    assert!(ws.stat("/a/b").await.unwrap().is_dir());
    assert!(ws.stat("/").await.unwrap().is_dir());
    assert!(matches!(ws.stat("/nope").await, Err(Error::NotFound(p)) if p == "/nope"));
    assert!(ws.exists("/a").await.unwrap());
    // 3 nodes + the file; empty files are fine.
    ws.write("/empty", Bytes::new(), Default::default()).await.unwrap();
    assert_eq!(ws.read("/empty").await.unwrap().len(), 0);
    assert_eq!(ws.usage().await.unwrap().node_count, 4);
}

#[tokio::test]
async fn kinds_collide_correctly() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    ws.write("/f", "x", Default::default()).await.unwrap();
    ws.mkdir("/d", false).await.unwrap();
    assert!(matches!(ws.write("/f/child", "x", Default::default()).await, Err(Error::NotADirectory(p)) if p == "/f"));
    assert!(matches!(ws.write("/d", "x", Default::default()).await, Err(Error::IsADirectory(_))));
    assert!(matches!(ws.write("/", "x", Default::default()).await, Err(Error::IsADirectory(_))));
    assert!(matches!(ws.read("/d").await, Err(Error::IsADirectory(_))));
    assert!(matches!(ws.list("/f", Default::default()).await, Err(Error::NotADirectory(_))));
    assert!(matches!(ws.mkdir("/f", true).await, Err(Error::NotADirectory(_))));
    assert!(matches!(ws.mkdir("/d", false).await, Err(Error::AlreadyExists(_))));
    assert!(ws.mkdir("/d", true).await.unwrap().is_dir());
    assert!(matches!(ws.mkdir("/x/y", false).await, Err(Error::NotFound(p)) if p == "/x"));
    ws.mkdir("/x/y/z", true).await.unwrap();
    assert!(ws.stat("/x/y").await.unwrap().is_dir());
}

#[tokio::test]
async fn preconditions_and_versions() {
    let f = fixture_with(Config { max_versions: 3, ..Config::default() }, 8).await;
    let ws = f.ws("u1").await;
    ws.write("/n.md", "v1", new()).await.unwrap();
    assert!(matches!(ws.write("/n.md", "x", new()).await, Err(Error::AlreadyExists(_))));
    let opts = |v| WriteOptions { expect: Expect::Version(v), mime: None };
    assert!(matches!(ws.write("/n.md", "x", opts(7)).await, Err(Error::Conflict { expected: 7, actual: 1, .. })));
    assert!(matches!(ws.write("/missing", "x", opts(1)).await, Err(Error::NotFound(_))));
    for (i, body) in ["v2", "v3", "v4", "v5"].iter().enumerate() {
        let e = ws.write("/n.md", *body, opts(i as i32 + 1)).await.unwrap();
        assert_eq!(e.version, i as i32 + 2);
    }
    let versions: Vec<i32> = ws.versions("/n.md").await.unwrap().iter().map(|v| v.version).collect();
    assert_eq!(versions, [5, 4, 3], "only max_versions are kept");
    assert_eq!(ws.read_version("/n.md", 3).await.unwrap(), "v3");
    assert!(matches!(ws.read_version("/n.md", 1).await, Err(Error::NotFound(_))));
    // Pruned versions free their bytes and objects: 3 versions of 2 bytes remain.
    assert_eq!(ws.usage().await.unwrap().used_bytes, 6);
    assert_eq!(f.objects().await, 3);
    let restored = ws.restore_version("/n.md", 3).await.unwrap();
    assert_eq!((restored.version, ws.read("/n.md").await.unwrap()), (6, Bytes::from("v3")));
    // Restoring shares the old blob: no new object, no new bytes. Versions 6 (= 3's bytes), 5
    // and 4 remain; version 3's row was pruned but its blob lives on in version 6.
    assert_eq!(f.objects().await, 3);
    assert_eq!(ws.usage().await.unwrap().used_bytes, 6);
    let versions: Vec<i32> = ws.versions("/n.md").await.unwrap().iter().map(|v| v.version).collect();
    assert_eq!(versions, [6, 5, 4]);
}

#[tokio::test]
async fn edits() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    ws.write("/code.rs", "fn a() {}\nfn b() {}\n", Default::default()).await.unwrap();
    let out = ws.edit("/code.rs", &[Edit::new("fn b", "fn c")], Expect::Any).await.unwrap();
    assert_eq!((out.replacements, out.entry.version), (1, 2));
    assert!(matches!(
        ws.edit("/code.rs", &[Edit::new("fn", "f")], Expect::Any).await,
        Err(Error::EditAmbiguous { count: 2, .. })
    ));
    assert!(matches!(ws.edit("/code.rs", &[Edit::new("zzz", "y")], Expect::Any).await, Err(Error::EditNotFound(_))));
    assert!(matches!(
        ws.edit("/code.rs", &[Edit::new("fn a", "x")], Expect::Version(1)).await,
        Err(Error::Conflict { .. })
    ));
    ws.edit("/code.rs", &[Edit::all("fn", "pub fn")], Expect::Version(2)).await.unwrap();
    assert_eq!(ws.read("/code.rs").await.unwrap(), "pub fn a() {}\npub fn c() {}\n");
    // A failed edit leaves no stray object behind.
    let before = f.objects().await;
    let _ = ws.edit("/code.rs", &[Edit::new("nope", "x")], Expect::Any).await;
    assert_eq!(f.objects().await, before);
    // Binary files are not text.
    ws.write("/img.png", vec![0x89, b'P', b'N', b'G', 0, 1, 2], Default::default()).await.unwrap();
    assert!(matches!(ws.edit("/img.png", &[Edit::new("PNG", "x")], Expect::Any).await, Err(Error::NotText(_))));
    assert!(matches!(ws.read_text("/img.png", 1, None).await, Err(Error::NotText(_))));
}

#[tokio::test]
async fn edit_many_is_all_or_nothing() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    ws.write("/a.txt", "alpha", Default::default()).await.unwrap();
    ws.write("/b.txt", "beta", Default::default()).await.unwrap();
    let both = |b_old: &str| {
        vec![
            FileEdits { path: "/a.txt".into(), edits: vec![Edit::new("alpha", "ALPHA")], expect: Expect::Any },
            FileEdits { path: "/b.txt".into(), edits: vec![Edit::new(b_old, "BETA")], expect: Expect::Any },
        ]
    };
    assert!(ws.edit_many(both("missing")).await.is_err());
    assert_eq!(ws.read("/a.txt").await.unwrap(), "alpha", "nothing applied");
    let out = ws.edit_many(both("beta")).await.unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(ws.read("/a.txt").await.unwrap(), "ALPHA");
    assert_eq!(ws.read("/b.txt").await.unwrap(), "BETA");
    let dup = vec![
        FileEdits { path: "/a.txt".into(), edits: vec![Edit::new("A", "a")], expect: Expect::Any },
        FileEdits { path: "a.txt".into(), edits: vec![Edit::new("L", "l")], expect: Expect::Any },
    ];
    assert!(matches!(ws.edit_many(dup).await, Err(Error::InvalidInput(_))));
}

#[tokio::test]
async fn concurrent_edits_all_land() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    let lines: String = (0..20).map(|i| format!("line {i}: todo\n")).collect();
    ws.write("/log.md", lines, Default::default()).await.unwrap();
    // 20 agents edit different lines of one file at once; each retries on conflict.
    let results: Vec<_> = stream::iter(0..20)
        .map(|i| {
            let ws = ws.clone();
            async move {
                ws.edit("/log.md", &[Edit::new(format!("line {i}: todo"), format!("line {i}: done"))], Expect::Any)
                    .await
            }
        })
        .buffer_unordered(20)
        .collect()
        .await;
    let ok = results.iter().filter(|r| r.is_ok()).count();
    let text = String::from_utf8(ws.read("/log.md").await.unwrap().to_vec()).unwrap();
    let done = text.matches(": done").count();
    // Every edit that reported success is in the file, and nothing was lost to a race.
    assert_eq!(done, ok);
    assert!(ok >= 1);
    for r in results.iter().filter_map(|r| r.as_ref().err()) {
        assert!(matches!(r, Error::Conflict { .. }), "only conflicts may fail: {r}");
    }
}

#[tokio::test]
async fn concurrent_creates_of_one_path_make_one_file() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    let results: Vec<_> = stream::iter(0..16)
        .map(|i| {
            let ws = ws.clone();
            async move { ws.write("/deep/same/file.txt", format!("writer {i}"), Default::default()).await }
        })
        .buffer_unordered(16)
        .collect()
        .await;
    assert!(results.iter().all(|r| r.is_ok()), "{results:?}");
    let e = ws.stat("/deep/same/file.txt").await.unwrap();
    assert_eq!(e.version, 16, "every write became one version of one file");
    assert_eq!(ws.usage().await.unwrap().node_count, 3);
}

#[tokio::test]
async fn read_text_and_ranges() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    ws.write("/t.txt", "one\r\ntwo\nthree\n", Default::default()).await.unwrap();
    let w = ws.read_text("/t.txt", 2, Some(5)).await.unwrap();
    assert_eq!((w.lines, w.first_line, w.total_lines), (vec!["two".to_string(), "three".into()], 2, 3));
    assert!(ws.read_text("/t.txt", 99, None).await.unwrap().lines.is_empty());
    assert_eq!(ws.read_range("/t.txt", 5, 3).await.unwrap(), "two");
    assert_eq!(ws.read_range("/t.txt", 13, 100).await.unwrap(), "e\n");
    assert!(ws.read_range("/t.txt", 100, 5).await.unwrap().is_empty());
    let (entry, body) = ws.open("/t.txt").await.unwrap();
    let all: Vec<Bytes> = body.try_collect().await.unwrap();
    assert_eq!(all.concat().len() as u64, entry.size);
}

#[tokio::test]
async fn listing_depth_and_pages() {
    let f = fixture_with(Config { list_limit: 3, ..Config::default() }, 8).await;
    let ws = f.ws("u1").await;
    for p in ["/a/1", "/a/2", "/a/sub/3", "/b", "/c", "/d"] {
        ws.write(p, "x", Default::default()).await.unwrap();
    }
    let top = ws.list("/", ListOptions { limit: Some(10), ..Default::default() }).await.unwrap();
    let names: Vec<_> = top.entries.iter().map(|e| e.path.clone()).collect();
    assert_eq!(names, ["/a", "/b", "/c"], "capped at list_limit");
    let next = ws.list("/", ListOptions { after: top.next, ..Default::default() }).await.unwrap();
    assert_eq!(next.entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["/d"]);
    assert!(next.next.is_none());
    let deep = ws.list("/a", ListOptions { depth: 3, ..Default::default() }).await.unwrap();
    assert_eq!(deep.entries.len(), 3, "/a/1, /a/2, /a/sub (page of 3)");
    let all = ws.list("/a", ListOptions { depth: 3, after: deep.next, ..Default::default() }).await.unwrap();
    assert_eq!(all.entries[0].path, "/a/sub/3");
}

#[tokio::test]
async fn rename_moves_whole_subtrees() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    ws.write("/docs/a.txt", "a", Default::default()).await.unwrap();
    ws.write("/docs/deep/b.txt", "b", Default::default()).await.unwrap();
    ws.write("/docs-old/keep.txt", "k", Default::default()).await.unwrap();
    ws.rename("/docs", "/archive/2026/docs", false).await.unwrap();
    assert_eq!(ws.read("/archive/2026/docs/deep/b.txt").await.unwrap(), "b");
    assert!(!ws.exists("/docs/a.txt").await.unwrap());
    assert_eq!(ws.read("/docs-old/keep.txt").await.unwrap(), "k", "a sibling with the same prefix is untouched");
    assert!(matches!(ws.rename("/archive", "/archive/x", false).await, Err(Error::InvalidInput(_))));
    assert!(matches!(ws.rename("/", "/x", false).await, Err(Error::PermissionDenied(_))));
    // Rename onto an existing file needs overwrite; the replaced file goes to the trash.
    ws.write("/x.txt", "x", Default::default()).await.unwrap();
    ws.write("/y.txt", "y", Default::default()).await.unwrap();
    assert!(matches!(ws.rename("/x.txt", "/y.txt", false).await, Err(Error::AlreadyExists(_))));
    let moved = ws.rename("/x.txt", "/y.html", false).await.unwrap();
    assert_eq!(moved.mime.as_deref(), Some("text/html"), "type follows the new name");
    ws.rename("/y.html", "/y.txt", true).await.unwrap();
    assert_eq!(ws.read("/y.txt").await.unwrap(), "x");
    assert_eq!(ws.list_trash().await.unwrap().len(), 1);
    // Folder onto file and file onto folder are refused even with overwrite.
    assert!(matches!(ws.rename("/archive", "/y.txt", true).await, Err(Error::NotADirectory(_))));
    assert!(matches!(ws.rename("/y.txt", "/archive", true).await, Err(Error::IsADirectory(_))));
    // Case-only rename works (paths are case-sensitive).
    ws.rename("/y.txt", "/Y.txt", false).await.unwrap();
    assert!(ws.exists("/Y.txt").await.unwrap());
}

#[tokio::test]
async fn copy_shares_bytes_until_changed() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    ws.write("/src/big.bin", vec![7u8; 10_000], Default::default()).await.unwrap();
    ws.write("/src/n/note.md", "note", Default::default()).await.unwrap();
    let used = ws.usage().await.unwrap().used_bytes;
    ws.copy("/src", "/dst", false).await.unwrap();
    let after = ws.usage().await.unwrap();
    assert_eq!(after.used_bytes, used, "copies cost no bytes");
    assert_eq!(after.node_count, 8);
    assert_eq!(ws.read("/dst/n/note.md").await.unwrap(), "note");
    assert_eq!(ws.stat("/dst/big.bin").await.unwrap().version, 1);
    // Deleting and purging the original leaves the copy intact.
    let t = ws.delete("/src", true).await.unwrap();
    ws.purge(t.id).await.unwrap();
    assert_eq!(ws.read("/dst/big.bin").await.unwrap().len(), 10_000);
    assert_eq!(ws.usage().await.unwrap().used_bytes, used);
    assert!(matches!(ws.copy("/dst", "/dst/inner", false).await, Err(Error::InvalidInput(_))));
    assert!(matches!(ws.copy("/dst", "/dst", true).await, Err(Error::InvalidInput(_))));
}

#[tokio::test]
async fn copy_limit() {
    let f = fixture_with(Config { max_copy_nodes: 3, ..Config::default() }, 8).await;
    let ws = f.ws("u1").await;
    for i in 0..3 {
        ws.write(&format!("/d/{i}"), "x", Default::default()).await.unwrap();
    }
    assert!(matches!(ws.copy("/d", "/e", false).await, Err(Error::InvalidInput(_))));
    assert!(!ws.exists("/e").await.unwrap());
}

#[tokio::test]
async fn trash_restore_and_purge() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    ws.write("/p/a.txt", "aaaa", Default::default()).await.unwrap();
    ws.write("/p/q/b.txt", "bb", Default::default()).await.unwrap();
    assert!(matches!(ws.delete("/p", false).await, Err(Error::NotEmpty(_))));
    assert!(matches!(ws.delete("/", true).await, Err(Error::PermissionDenied(_))));
    let t = ws.delete("/p", true).await.unwrap();
    assert_eq!((t.path.as_str(), t.size), ("/p", 6));
    assert!(!ws.exists("/p/q/b.txt").await.unwrap());
    // The path is free again while the old one sits in the trash.
    ws.write("/p/a.txt", "new", Default::default()).await.unwrap();
    assert!(matches!(ws.restore(t.id).await, Err(Error::AlreadyExists(_))));
    let t2 = ws.delete("/p", true).await.unwrap();
    ws.restore(t.id).await.unwrap();
    assert_eq!(ws.read("/p/q/b.txt").await.unwrap(), "bb");
    assert_eq!(ws.list_trash().await.unwrap().len(), 1);
    // Purging frees bytes, nodes and objects.
    let before = ws.usage().await.unwrap();
    ws.purge(t2.id).await.unwrap();
    let after = ws.usage().await.unwrap();
    assert_eq!(before.used_bytes - after.used_bytes, 3);
    assert_eq!(before.node_count - after.node_count, 2);
    assert!(matches!(ws.purge(t2.id).await, Err(Error::NotFound(_))));
    // Restoring re-creates folders above that were deleted meanwhile.
    let tf = ws.delete("/p/q/b.txt", false).await.unwrap();
    ws.delete("/p", true).await.unwrap();
    ws.restore(tf.id).await.unwrap();
    assert_eq!(ws.read("/p/q/b.txt").await.unwrap(), "bb");
    assert_eq!(ws.empty_trash().await.unwrap(), 1);
    assert!(ws.list_trash().await.unwrap().is_empty());
}

#[tokio::test]
async fn quotas_hold_under_concurrency() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    f.fs.set_quota("test", "u1", Some(100), Some(1000)).await.unwrap();
    let results: Vec<_> = stream::iter(0..30)
        .map(|i| {
            let ws = ws.clone();
            async move { ws.write(&format!("/f{i}"), vec![1u8; 10], Default::default()).await }
        })
        .buffer_unordered(30)
        .collect()
        .await;
    let ok = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(ok, 10, "exactly 100 bytes fit");
    assert!(results.iter().filter_map(|r| r.as_ref().err()).all(|e| matches!(e, Error::QuotaExceeded(Quota::Bytes))));
    assert_eq!(ws.usage().await.unwrap().used_bytes, 100);
    assert_eq!(f.objects().await, 10, "refused writes left nothing in storage");
    // Node quota.
    f.fs.set_quota("test", "u1", None, Some(11)).await.unwrap();
    ws.mkdir("/one-more", false).await.unwrap();
    assert!(matches!(ws.mkdir("/too-many", false).await, Err(Error::QuotaExceeded(Quota::Nodes))));
    assert!(matches!(ws.write("/x/y", "z", Default::default()).await, Err(Error::QuotaExceeded(Quota::Nodes))));
    // Raising the limit (a plan upgrade) takes effect at once.
    f.fs.set_quota("test", "u1", None, None).await.unwrap();
    ws.write("/x/y", "z", Default::default()).await.unwrap();
}

#[tokio::test]
async fn default_quota_and_size_limits() {
    let cfg = Config { default_quota_bytes: Some(50), max_inline_bytes: 40, ..Config::default() };
    let f = fixture_with(cfg, 8).await;
    let ws = f.ws("new-user").await;
    assert_eq!(ws.usage().await.unwrap().quota_bytes, Some(50));
    assert!(matches!(
        ws.write("/big", vec![0u8; 41], Default::default()).await,
        Err(Error::TooLarge { limit: 40, .. })
    ));
    ws.write("/ok", vec![0u8; 40], Default::default()).await.unwrap();
    assert!(matches!(ws.write("/more", vec![0u8; 20], Default::default()).await, Err(Error::QuotaExceeded(_))));
}

#[tokio::test]
async fn workspaces_are_isolated() {
    let f = fixture().await;
    let a = f.ws("alice").await;
    let b = f.ws("bob").await;
    a.write("/secret.txt", "alice's", Default::default()).await.unwrap();
    b.write("/secret.txt", "bob's", Default::default()).await.unwrap();
    assert_eq!(a.read("/secret.txt").await.unwrap(), "alice's");
    assert_eq!(b.read("/secret.txt").await.unwrap(), "bob's");
    // The same external id in another app is another workspace.
    let other = f.fs.workspace("other-app", "alice", Access::full()).await.unwrap();
    assert!(!other.exists("/secret.txt").await.unwrap());
    // Bob cannot use Alice's trash or share ids.
    let t = a.delete("/secret.txt", false).await.unwrap();
    assert!(matches!(b.restore(t.id).await, Err(Error::NotFound(_))));
    assert!(matches!(b.purge(t.id).await, Err(Error::NotFound(_))));
    a.restore(t.id).await.unwrap();
    let s = a.share("/secret.txt", None).await.unwrap();
    assert!(matches!(b.revoke_share(s.info.id).await, Err(Error::ShareNotFound)));
}

#[tokio::test]
async fn row_level_security_hides_other_workspaces() {
    let f = fixture().await;
    let a = f.ws("alice").await;
    a.write("/x", "1", Default::default()).await.unwrap();
    // Outside a scoped transaction, the app's own role sees no rows at all.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sf3_nodes").fetch_one(&f.pool).await.unwrap();
    assert_eq!(n, 0);
    // Scoped to another workspace, still nothing.
    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('sysfile3.workspace', $1, true)")
        .bind(uuid::Uuid::new_v4().to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sf3_nodes").fetch_one(&mut *tx).await.unwrap();
    assert_eq!(n, 0);
    // Nor can a row be written into a workspace the transaction is not scoped to.
    let insert =
        sqlx::query("INSERT INTO sf3_nodes (id, workspace_id, path, depth, kind) VALUES ($1, $2, '/evil', 1, 'dir')")
            .bind(uuid::Uuid::new_v4())
            .bind(a.id())
            .execute(&mut *tx)
            .await;
    assert!(insert.is_err(), "row-level security must reject the insert");
}

#[tokio::test]
async fn confined_and_read_only_handles() {
    let f = fixture().await;
    let owner = f.ws("u1").await;
    owner.write("/private/diary.md", "secret", Default::default()).await.unwrap();
    let agent =
        f.fs.workspace("test", "u1", Access::full().within("/agents/a1").unwrap().actor("agent-a1")).await.unwrap();
    assert!(agent.list("/", Default::default()).await.unwrap().entries.is_empty(), "an unborn root is an empty folder");
    agent.write("/notes.md", "hi", Default::default()).await.unwrap();
    assert_eq!(owner.read("/agents/a1/notes.md").await.unwrap(), "hi");
    assert!(matches!(agent.read("../../private/diary.md").await, Err(Error::InvalidPath(_))));
    assert!(matches!(agent.read("/private/diary.md").await, Err(Error::NotFound(p)) if p == "/private/diary.md"));
    let g = agent.glob("**/*", "/").await.unwrap();
    assert_eq!(g.entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["/notes.md"]);
    assert!(matches!(agent.delete("/", true).await, Err(Error::PermissionDenied(_))));
    // The confined handle's trash only shows its own deletions.
    owner.delete("/private/diary.md", false).await.unwrap();
    assert!(agent.list_trash().await.unwrap().is_empty());
    let viewer = f.fs.workspace("test", "u1", Access::read_only()).await.unwrap();
    assert_eq!(viewer.read("/agents/a1/notes.md").await.unwrap(), "hi");
    assert!(matches!(viewer.write("/x", "y", Default::default()).await, Err(Error::PermissionDenied(_))));
    assert!(matches!(viewer.delete("/agents", true).await, Err(Error::PermissionDenied(_))));
    assert!(matches!(viewer.share("/agents", None).await, Err(Error::PermissionDenied(_))));
    let no_share = f.fs.workspace("test", "u1", Access::full().no_share()).await.unwrap();
    assert!(matches!(no_share.share("/agents", None).await, Err(Error::PermissionDenied(_))));
}

#[tokio::test]
async fn shares() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    ws.write("/site/index.html", "<h1>hi</h1>", Default::default()).await.unwrap();
    ws.write("/site/css/app.css", "body{}", Default::default()).await.unwrap();
    ws.write("/report.pdf", b"%PDF-1.7 ...".to_vec(), Default::default()).await.unwrap();
    ws.write("/private.txt", "no", Default::default()).await.unwrap();

    let file = ws.share("/report.pdf", None).await.unwrap();
    assert_eq!(file.token.len(), 64);
    let view = f.fs.open_share(&file.token).await.unwrap();
    let st = view.stat("/").await.unwrap();
    assert_eq!((st.path.as_str(), st.mime.as_deref()), ("/", Some("application/pdf")));
    assert_eq!(view.read("/").await.unwrap(), Bytes::from_static(b"%PDF-1.7 ..."));
    assert!(matches!(view.read("/private.txt").await, Err(Error::NotADirectory(_)) | Err(Error::NotFound(_))));
    assert!(matches!(view.write("/x", "y", Default::default()).await, Err(Error::PermissionDenied(_))));

    let site = ws.share("/site", Some(Duration::from_secs(3600))).await.unwrap();
    let view = f.fs.open_share(&site.token).await.unwrap();
    assert_eq!(view.read("/css/app.css").await.unwrap(), "body{}");
    assert!(matches!(view.read("/../private.txt").await, Err(Error::InvalidPath(_))));
    assert_eq!(ws.shares().await.unwrap().len(), 2);

    // The token is not stored, only its hash.
    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('sysfile3.workspace', '*', true)").execute(&mut *tx).await.unwrap();
    let leaked: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sf3_shares WHERE encode(token_hash, 'hex') = $1)")
            .bind(&site.token)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert!(!leaked);
    drop(tx);

    // Links follow moves, stop while trashed, and can be revoked.
    ws.rename("/site", "/published/site", false).await.unwrap();
    assert_eq!(f.fs.open_share(&site.token).await.unwrap().read("/index.html").await.unwrap(), "<h1>hi</h1>");
    let t = ws.delete("/published", true).await.unwrap();
    assert!(matches!(f.fs.open_share(&site.token).await, Err(Error::ShareNotFound)));
    ws.restore(t.id).await.unwrap();
    assert!(f.fs.open_share(&site.token).await.is_ok());
    ws.revoke_share(site.info.id).await.unwrap();
    assert!(matches!(f.fs.open_share(&site.token).await, Err(Error::ShareNotFound)));
    assert!(matches!(f.fs.open_share("not-a-token").await, Err(Error::ShareNotFound)));
    assert!(matches!(f.fs.open_share(&"0".repeat(64)).await, Err(Error::ShareNotFound)));
    assert!(matches!(ws.share("/", None).await, Err(Error::InvalidInput(_))));
    // Expired links stop working.
    let short = ws.share("/report.pdf", Some(Duration::from_millis(1))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(matches!(f.fs.open_share(&short.token).await, Err(Error::ShareNotFound)));
}

#[tokio::test]
async fn glob_and_grep() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    ws.write("/notes/2026/jan.md", "# January\nbuy milk\nTODO: call mom\n", Default::default()).await.unwrap();
    ws.write("/notes/2026/feb.md", "TODO: taxes\n", Default::default()).await.unwrap();
    ws.write("/notes/ideas.txt", "todo lowercase\n", Default::default()).await.unwrap();
    ws.write("/bin/blob.dat", vec![0u8, 1, 2, b'T', b'O', b'D', b'O'], Default::default()).await.unwrap();
    let paths = |g: sysfile3::GlobResult| g.entries.into_iter().map(|e| e.path).collect::<Vec<_>>();
    assert_eq!(paths(ws.glob("**/*.md", "/").await.unwrap()), ["/notes/2026/feb.md", "/notes/2026/jan.md"]);
    assert_eq!(paths(ws.glob("*.txt", "/notes").await.unwrap()), ["/notes/ideas.txt"]);
    assert!(paths(ws.glob("*.md", "/notes").await.unwrap()).is_empty(), "* does not cross folders");
    assert_eq!(paths(ws.glob("notes/2026/j*", "/").await.unwrap()), ["/notes/2026/jan.md"]);
    assert!(matches!(ws.glob("[", "/").await, Err(Error::InvalidInput(_))));

    let r = ws.grep(GrepOptions::new("TODO")).await.unwrap();
    let hits: Vec<_> = r.matches.iter().map(|m| (m.path.as_str(), m.line)).collect();
    assert_eq!(hits, [("/notes/2026/feb.md", 1), ("/notes/2026/jan.md", 3)]);
    assert_eq!(r.files_skipped, 1, "the binary file");
    let r = ws.grep(GrepOptions { ignore_case: true, ..GrepOptions::new("todo") }).await.unwrap();
    assert_eq!(r.matches.len(), 3);
    let r = ws
        .grep(GrepOptions { include: Some("*.txt".into()), ignore_case: true, ..GrepOptions::new("todo") })
        .await
        .unwrap();
    assert_eq!(r.matches.len(), 1);
    let r = ws.grep(GrepOptions { max_matches: Some(1), ..GrepOptions::new("TODO") }).await.unwrap();
    assert!(r.truncated && r.matches.len() == 1);
    let r = ws.grep(GrepOptions { under: "/notes/ideas.txt".into(), ..GrepOptions::new("todo") }).await.unwrap();
    assert_eq!(r.matches.len(), 1, "a single file can be searched");
    assert!(matches!(ws.grep(GrepOptions::new("(")).await, Err(Error::InvalidInput(_))));
}

#[tokio::test]
async fn streaming_writes() {
    let cfg = Config { max_file_bytes: 20 * 1024 * 1024, ..Config::default() };
    let f = fixture_with(cfg, 8).await;
    let ws = f.ws("u1").await;
    let chunk = Bytes::from(vec![42u8; 1024 * 1024]);
    let body = stream::iter((0..12).map(|_| Ok::<_, std::io::Error>(chunk.clone())));
    let e = ws.write_stream("/video.mp4", body, Default::default()).await.unwrap();
    assert_eq!((e.size, e.mime.as_deref()), (12 * 1024 * 1024, Some("video/mp4")));
    assert_eq!(ws.read_range("/video.mp4", 5 * 1024 * 1024, 4).await.unwrap(), Bytes::from(vec![42u8; 4]));
    // Over the file limit: refused, and nothing left in storage.
    let before = f.objects().await;
    let body = stream::iter((0..21).map(|_| Ok::<_, std::io::Error>(chunk.clone())));
    assert!(matches!(ws.write_stream("/huge.bin", body, Default::default()).await, Err(Error::TooLarge { .. })));
    assert_eq!(f.objects().await, before);
    // A stream that fails midway leaves nothing either.
    let body = stream::iter(vec![Ok(chunk.clone()), Err(std::io::Error::other("client went away"))]);
    assert!(matches!(ws.write_stream("/broken.bin", body, Default::default()).await, Err(Error::Io(_))));
    assert_eq!(f.objects().await, before);
    // Over quota mid-stream is a quota error, not a size error.
    f.fs.set_quota("test", "u1", Some(13 * 1024 * 1024), None).await.unwrap();
    let body = stream::iter((0..2).map(|_| Ok::<_, std::io::Error>(chunk.clone())));
    assert!(matches!(
        ws.write_stream("/q.bin", body, Default::default()).await,
        Err(Error::QuotaExceeded(Quota::Bytes))
    ));
}

#[tokio::test]
async fn direct_uploads() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    let t = ws.begin_upload("/uploads/photo.jpg", 5, Default::default()).await.unwrap();
    assert!(t.url.starts_with("https://signed.test/") && t.url.contains("method=PUT"));
    // Nothing uploaded yet.
    assert!(matches!(ws.finish_upload(t.id, Expect::Any).await, Err(Error::InvalidInput(_))));
    // The ticket is single-use: finishing it again is not found.
    assert!(matches!(ws.finish_upload(t.id, Expect::Any).await, Err(Error::NotFound(_))));

    let t = ws.begin_upload("/uploads/photo.jpg", 5, Default::default()).await.unwrap();
    f.client_put(&t.url, b"12345".to_vec()).await;
    let e = ws.finish_upload(t.id, Expect::Any).await.unwrap();
    assert_eq!((e.size, e.mime.as_deref()), (5, Some("image/jpeg")));
    assert_eq!(ws.read("/uploads/photo.jpg").await.unwrap(), "12345");
    assert!(ws.download_url("/uploads/photo.jpg").await.unwrap().contains("method=GET"));

    // Wrong size is refused and the object deleted.
    let t = ws.begin_upload("/uploads/x.bin", 3, Default::default()).await.unwrap();
    f.client_put(&t.url, b"too long".to_vec()).await;
    let before = f.objects().await;
    assert!(matches!(ws.finish_upload(t.id, Expect::Any).await, Err(Error::InvalidInput(_))));
    assert_eq!(f.objects().await, before - 1);

    // Cancel deletes what was sent.
    let t = ws.begin_upload("/uploads/y.bin", 2, Default::default()).await.unwrap();
    f.client_put(&t.url, b"ab".to_vec()).await;
    ws.cancel_upload(t.id).await.unwrap();
    assert_eq!(f.objects().await, before - 1);

    // Reservations respect size limits, quotas and kinds.
    assert!(matches!(ws.begin_upload("/uploads", 1, Default::default()).await, Err(Error::IsADirectory(_))));
    assert!(matches!(
        ws.begin_upload("/uploads/photo.jpg", 1, WriteOptions { expect: Expect::Absent, mime: None }).await,
        Err(Error::AlreadyExists(_))
    ));
    f.fs.set_quota("test", "u1", Some(10), None).await.unwrap();
    assert!(matches!(ws.begin_upload("/u/z", 6, Default::default()).await, Err(Error::QuotaExceeded(_))));
    // Expired tickets are cleaned up.
    let cfg = Config { upload_ttl: Duration::from_millis(1), ..Config::default() };
    let f2 = fixture_with(cfg, 4).await;
    let ws2 = f2.ws("u").await;
    let t = ws2.begin_upload("/late.bin", 1, Default::default()).await.unwrap();
    f2.client_put(&t.url, b"x".to_vec()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(f2.fs.expire_uploads().await.unwrap(), 1);
    assert_eq!(f2.objects().await, 0);
}

#[tokio::test]
async fn maintenance() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    ws.write("/a", "aaa", Default::default()).await.unwrap();
    ws.write("/b", "bb", Default::default()).await.unwrap();
    ws.delete("/a", false).await.unwrap();
    assert_eq!(f.fs.purge_trash_older_than(Duration::from_secs(3600)).await.unwrap(), 0);
    assert_eq!(f.fs.purge_trash_older_than(Duration::ZERO).await.unwrap(), 1);
    assert_eq!(ws.usage().await.unwrap().used_bytes, 2);

    // Orphans: an object with a sysfile3-shaped key that nothing points at, and one that is not ours.
    use object_store::ObjectStoreExt;
    let orphan = format!("sysfile3/{}/{}", ws.id(), uuid::Uuid::now_v7());
    f.store.put(&orphan.as_str().into(), Bytes::from("x").into()).await.unwrap();
    f.store.put(&"other-app/file".into(), Bytes::from("keep").into()).await.unwrap();
    assert_eq!(f.fs.sweep_orphans(Duration::from_secs(3600)).await.unwrap(), 0, "too young");
    assert_eq!(f.fs.sweep_orphans(Duration::ZERO).await.unwrap(), 1);
    assert_eq!(ws.read("/b").await.unwrap(), "bb", "live files survive the sweep");
    assert_eq!(f.objects().await, 2);

    // Billing report.
    let other = f.ws("u2").await;
    other.write("/x", "1234", Default::default()).await.unwrap();
    let report = f.fs.usage_report("test", None, 10).await.unwrap();
    let bytes: Vec<_> = report.iter().map(|(id, u)| (id.as_str(), u.used_bytes)).collect();
    assert_eq!(bytes, [("u1", 2), ("u2", 4)]);
    assert_eq!(f.fs.usage_report("test", Some("u1"), 10).await.unwrap().len(), 1);

    // Account deletion removes rows and objects; the id can be reused afresh.
    assert_eq!(f.fs.purge_workspace("test", "u1").await.unwrap(), 1);
    assert_eq!(f.objects().await, 2, "u2's object and the foreign one remain");
    let fresh = f.ws("u1").await;
    assert!(!fresh.exists("/b").await.unwrap());
    assert_eq!(fresh.usage().await.unwrap().used_bytes, 0);
}

#[tokio::test]
async fn unicode_names_and_odd_paths() {
    let f = fixture().await;
    let ws = f.ws("u1").await;
    ws.write("/Café/naïve résumé.pdf", "x", Default::default()).await.unwrap();
    // The same name typed with combining accents is the same file.
    assert_eq!(ws.read("/Cafe\u{301}/nai\u{308}ve re\u{301}sume\u{301}.pdf").await.unwrap(), "x");
    for name in ["/日本語/ファイル.txt", "/emoji 🎉/party.md", "/space name/with spaces.txt", "/dots.../.hidden"]
    {
        ws.write(name, "ok", Default::default()).await.unwrap();
        assert_eq!(ws.read(name).await.unwrap(), "ok", "{name}");
    }
    assert!(ws.list("/", Default::default()).await.unwrap().entries.len() >= 5);
    assert!(matches!(ws.write("/a\\b", "x", Default::default()).await, Err(Error::InvalidPath(_))));
    assert!(matches!(ws.write("/a\tb", "x", Default::default()).await, Err(Error::InvalidPath(_))));
}

#[tokio::test]
async fn migrate_twice_is_harmless() {
    let f = fixture().await;
    f.fs.migrate().await.unwrap();
    let ws = f.ws("u").await;
    ws.write("/x", "y", Default::default()).await.unwrap();
}
