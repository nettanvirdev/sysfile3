//! One user's life on an assistant platform, end to end: an agent keeping its memory files,
//! a library of real-looking files of every common type, work done in a sandbox and moved in,
//! a public site, search, quotas and a plan upgrade, mistakes undone, nightly maintenance, and
//! finally the account being deleted. Every file is read back and compared byte for byte.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use bytes::Bytes;
use common::fixture_with;
use futures::{StreamExt, TryStreamExt, stream};
use sha2::{Digest, Sha256};
use sysfile3::{Access, Config, Edit, Error, Expect, GrepOptions, ListOptions, Quota, WriteOptions};

const MIB: usize = 1024 * 1024;

/// Deterministic noise, so files look like compressed media and never accidentally like text.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn with_header(header: &[u8], body_len: usize, trailer: &[u8], seed: u64) -> Vec<u8> {
    let mut v = header.to_vec();
    v.extend(noise(body_len, seed));
    v.extend_from_slice(trailer);
    v
}

fn sha(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

/// The library a user uploads: name, bytes, expected content type.
fn library() -> Vec<(&'static str, Vec<u8>, &'static str)> {
    let pdf = {
        let mut v = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n1 0 obj << /Type /Catalog >> endobj\n".to_vec();
        v.extend(noise(300 * 1024, 3));
        v.extend_from_slice(b"\n%%EOF\n");
        v
    };
    vec![
        (
            "/library/photos/beach.jpg",
            with_header(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F'], 220 * 1024, &[0xFF, 0xD9], 1),
            "image/jpeg",
        ),
        (
            "/library/photos/logo.png",
            with_header(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR", 48 * 1024, b"IEND\xaeB`\x82", 2),
            "image/png",
        ),
        ("/library/photos/cat.webp", with_header(b"RIFF\0\0\0\0WEBPVP8 ", 90 * 1024, b"", 21), "image/webp"),
        ("/library/docs/contract.pdf", pdf, "application/pdf"),
        ("/library/docs/রিপোর্ট ২০২৬.pdf", with_header(b"%PDF-1.4\n", 40 * 1024, b"%%EOF", 4), "application/pdf"),
        (
            "/library/docs/letter.docx",
            with_header(b"PK\x03\x04\x14\0\x06\0", 30 * 1024, b"", 5),
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ),
        (
            "/library/docs/budget.xlsx",
            with_header(b"PK\x03\x04\x14\0\x06\0", 25 * 1024, b"", 6),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ),
        ("/library/audio/voice-note.mp3", with_header(b"ID3\x04\0\0\0\0\0\x0f", 600 * 1024, b"", 7), "audio/mpeg"),
        ("/library/data/sales.csv", b"date,amount\n2026-01-01,120.50\n2026-01-02,98.10\n".repeat(2000), "text/csv"),
        ("/library/data/config.json", br#"{"theme":"dark","lang":"bn","notify":true}"#.to_vec(), "application/json"),
        (
            "/library/data/backup.zip",
            with_header(b"PK\x03\x04\x0a\0\0\0", 180 * 1024, b"PK\x05\x06", 8),
            "application/zip",
        ),
        (
            "/library/web/icon.svg",
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8"/></svg>"#
                .to_vec(),
            "image/svg+xml",
        ),
        (
            "/library/notes/ideas.txt",
            "Ideas\n- TODO: weekly reel\n- a café with ünïcödé ✓\n".as_bytes().to_vec(),
            "text/plain",
        ),
    ]
}

#[tokio::test]
async fn a_users_life() {
    // A free plan: 50 MiB and 2,000 files for every new user.
    let cfg = Config {
        default_quota_bytes: Some(50 * MIB as u64),
        default_quota_nodes: Some(2_000),
        max_versions: 20,
        ..Config::default()
    };
    let f = fixture_with(cfg, 32).await;
    let app = "assistant";
    let user = "user-1001";
    let me = f.fs.workspace(app, user, Access::full().actor(user)).await.unwrap();
    let agent = f.fs.workspace(app, user, Access::full().actor("agent:main")).await.unwrap();
    let mut expected: BTreeMap<String, String> = BTreeMap::new();

    // 1. The agent sets up its memory.
    for (path, body) in [
        ("/identity.md", "# Identity\nName: Juno\nRole: personal assistant\nTone: warm, brief\n"),
        ("/user.md", "# User\nName: (unknown)\nTimezone: (unknown)\nPreferences:\n- (none yet)\n"),
        ("/heartbeat.md", "# Heartbeat\nlast_run: never\ncount: 0\n"),
    ] {
        agent.write(path, body, WriteOptions { expect: Expect::Absent, mime: None }).await.unwrap();
    }
    assert_eq!(me.stat("/identity.md").await.unwrap().created_by.as_deref(), Some("agent:main"));

    // 2. Fifty heartbeats, each a precise edit pinned to the version it read.
    for i in 1..=50 {
        let cur = agent.read_text("/heartbeat.md", 1, None).await.unwrap();
        let count_line = cur.lines.iter().find(|l| l.starts_with("count:")).unwrap().clone();
        let last_line = cur.lines.iter().find(|l| l.starts_with("last_run:")).unwrap().clone();
        agent
            .edit_many(vec![sysfile3::FileEdits {
                path: "/heartbeat.md".into(),
                edits: vec![
                    Edit::new(count_line, format!("count: {i}")),
                    Edit::new(last_line, format!("last_run: tick-{i}")),
                ],
                expect: Expect::Version(cur.version),
            }])
            .await
            .unwrap();
    }
    let hb = agent.read_text("/heartbeat.md", 1, None).await.unwrap();
    assert_eq!(hb.version, 51);
    assert!(hb.lines.contains(&"count: 50".to_string()));
    assert_eq!(agent.versions("/heartbeat.md").await.unwrap().len(), 20, "history capped");

    // 3. The user and the agent update user.md at the same moment, different lines.
    let name = [Edit::new("Name: (unknown)", "Name: Rahim")];
    let zone = [Edit::new("Timezone: (unknown)", "Timezone: Asia/Dhaka")];
    let (a, b) = tokio::join!(me.edit("/user.md", &name, Expect::Any), agent.edit("/user.md", &zone, Expect::Any));
    a.unwrap();
    b.unwrap();
    let user_md = String::from_utf8(me.read("/user.md").await.unwrap().to_vec()).unwrap();
    assert!(user_md.contains("Name: Rahim") && user_md.contains("Timezone: Asia/Dhaka"), "neither edit lost");

    // 4. The user's library: every common file type, several at once.
    let files = library();
    let results: Vec<_> = stream::iter(files.iter().cloned())
        .map(|(path, bytes, _)| {
            let me = me.clone();
            async move { me.write(path, bytes, Default::default()).await }
        })
        .buffer_unordered(8)
        .collect()
        .await;
    assert!(results.iter().all(|r| r.is_ok()), "{results:?}");
    for (path, bytes, mime) in &files {
        let e = me.stat(path).await.unwrap();
        assert_eq!(e.mime.as_deref(), Some(*mime), "{path}");
        assert_eq!(e.size, bytes.len() as u64, "{path}");
        expected.insert(path.to_string(), sha(bytes));
    }

    // A phone video goes up through a direct upload (client to storage), a screen recording
    // streams through the server.
    let video = with_header(b"\0\0\0\x20ftypisom\0\0\x02\0", 3 * MIB, b"", 9);
    let t = me.begin_upload("/library/video/trip.mp4", video.len() as u64, Default::default()).await.unwrap();
    f.client_put(&t.url, video.clone()).await;
    let e = me.finish_upload(t.id, Expect::Absent).await.unwrap();
    assert_eq!(e.mime.as_deref(), Some("video/mp4"));
    expected.insert("/library/video/trip.mp4".into(), sha(&video));
    let rec = with_header(b"\x1aE\xdf\xa3", 9 * MIB, b"", 10);
    let chunks: Vec<std::io::Result<Bytes>> = rec.chunks(256 * 1024).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
    let e = me.write_stream("/library/video/screen.webm", stream::iter(chunks), Default::default()).await.unwrap();
    assert_eq!((e.size, e.mime.as_deref()), (rec.len() as u64, Some("video/webm")));
    expected.insert("/library/video/screen.webm".into(), sha(&rec));

    // 5. A sandbox computer builds a report; the result is moved into the workspace.
    let research =
        f.fs.workspace(app, user, Access::full().within("/agents/research").unwrap().actor("agent:research"))
            .await
            .unwrap();
    let report = with_header(b"%PDF-1.7\n", 120 * 1024, b"%%EOF", 11);
    research.write("/out/market-report.pdf", report.clone(), Default::default()).await.unwrap();
    research.write("/out/notes.md", "TODO: double-check the Q3 numbers\n", Default::default()).await.unwrap();
    assert!(matches!(research.read("/identity.md").await, Err(Error::NotFound(_))), "confined");
    me.copy("/agents/research/out/market-report.pdf", "/library/reports/market-report.pdf", false).await.unwrap();
    expected.insert("/library/reports/market-report.pdf".into(), sha(&report));
    expected.insert("/agents/research/out/market-report.pdf".into(), sha(&report));
    expected.insert("/agents/research/out/notes.md".into(), sha(b"TODO: double-check the Q3 numbers\n"));

    // 6. A static site, published with a public link.
    let site = [
        (
            "/sites/launch/index.html",
            "<!doctype html><title>Launch</title><link rel=stylesheet href=css/site.css><img src=img/hero.png>"
                .as_bytes()
                .to_vec(),
        ),
        ("/sites/launch/css/site.css", b"body{font-family:system-ui}".to_vec()),
        ("/sites/launch/img/hero.png", with_header(b"\x89PNG\r\n\x1a\n", 64 * 1024, b"", 12)),
    ];
    for (path, bytes) in &site {
        me.write(path, bytes.clone(), Default::default()).await.unwrap();
        expected.insert(path.to_string(), sha(bytes));
    }
    let link = me.share("/sites/launch", None).await.unwrap();
    let public = f.fs.open_share(&link.token).await.unwrap();
    assert_eq!(public.stat("/index.html").await.unwrap().mime.as_deref(), Some("text/html"));
    assert_eq!(sha(&public.read("/img/hero.png").await.unwrap()), sha(&site[2].1));
    assert!(public.read("/../user.md").await.is_err());
    assert!(matches!(
        public.write("/index.html", "defaced", Default::default()).await,
        Err(Error::PermissionDenied(_))
    ));
    let pdf_link = me.share("/library/docs/contract.pdf", Some(Duration::from_secs(86_400))).await.unwrap();
    assert_eq!(
        sha(&f.fs.open_share(&pdf_link.token).await.unwrap().read("/").await.unwrap()),
        expected["/library/docs/contract.pdf"]
    );

    // 7. Search.
    let pdfs = me.glob("**/*.pdf", "/").await.unwrap();
    let names: Vec<_> = pdfs.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        names,
        [
            "/agents/research/out/market-report.pdf",
            "/library/docs/contract.pdf",
            "/library/docs/রিপোর্ট ২০২৬.pdf",
            "/library/reports/market-report.pdf",
        ]
    );
    let todos = me.grep(GrepOptions { ignore_case: true, ..GrepOptions::new("todo") }).await.unwrap();
    let hits: Vec<_> = todos.matches.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(hits, ["/agents/research/out/notes.md", "/library/notes/ideas.txt"]);
    assert!(todos.files_skipped >= 10, "binary files are skipped, not searched");
    let listing = me.list("/library", ListOptions { depth: 2, ..Default::default() }).await.unwrap();
    assert!(listing.entries.len() > 15);

    // 8. The free plan fills up; an upgrade lifts the limit at once.
    let usage = me.usage().await.unwrap();
    assert!(usage.used_bytes < 50 * MIB as u64);
    let film = vec![7u8; 40 * MIB];
    let film_stream = || {
        stream::iter(film.chunks(MIB).map(|c| Ok::<_, std::io::Error>(Bytes::copy_from_slice(c))).collect::<Vec<_>>())
    };
    let refused = me.write_stream("/library/video/film.mov", film_stream(), Default::default()).await;
    assert!(matches!(refused, Err(Error::QuotaExceeded(Quota::Bytes))));
    assert_eq!(me.usage().await.unwrap().used_bytes, usage.used_bytes, "a refused upload costs nothing");
    f.fs.set_quota(app, user, Some(1024 * MIB as u64), Some(100_000)).await.unwrap();
    me.write_stream("/library/video/film.mov", film_stream(), Default::default()).await.unwrap();
    expected.insert("/library/video/film.mov".into(), sha(&film));

    // 9. A mistake and its undo: the whole library deleted, then restored.
    let before = me.usage().await.unwrap();
    let t = me.delete("/library", true).await.unwrap();
    assert!(!me.exists("/library/docs/contract.pdf").await.unwrap());
    assert!(matches!(f.fs.open_share(&pdf_link.token).await, Err(Error::ShareNotFound)), "links pause while trashed");
    assert_eq!(me.usage().await.unwrap(), before, "the trash still counts");
    me.restore(t.id).await.unwrap();
    assert!(f.fs.open_share(&pdf_link.token).await.is_ok());
    // An old note version is brought back.
    me.write("/library/notes/ideas.txt", "overwritten by accident\n", Default::default()).await.unwrap();
    me.restore_version("/library/notes/ideas.txt", 1).await.unwrap();

    // 10. Someone else on the platform sees none of it.
    let stranger = f.fs.workspace(app, "user-2002", Access::full()).await.unwrap();
    assert!(stranger.list("/", Default::default()).await.unwrap().entries.is_empty());
    assert!(matches!(stranger.restore(t.id).await, Err(Error::NotFound(_))));
    assert!(matches!(stranger.revoke_share(link.info.id).await, Err(Error::ShareNotFound)));

    // 11. Nightly maintenance: trash older than 30 days, stale uploads, orphan objects.
    let junk = me.write("/tmp/junk.bin", noise(10_000, 13), Default::default()).await.unwrap();
    let junk_trash = me.delete(&junk.path, false).await.unwrap();
    assert_eq!(f.fs.purge_trash_older_than(Duration::from_secs(30 * 86_400)).await.unwrap(), 0);
    assert_eq!(
        f.fs.purge_trash_older_than(Duration::ZERO).await.unwrap(),
        1,
        "the junk (overwrites are versions, not trash)"
    );
    assert!(me.list_trash().await.unwrap().iter().all(|i| i.id != junk_trash.id));
    f.fs.expire_uploads().await.unwrap();
    assert_eq!(f.fs.sweep_orphans(Duration::ZERO).await.unwrap(), 0, "nothing is orphaned");

    // Every file reads back exactly, through every read path.
    for (path, digest) in &expected {
        let e = me.stat(path).await.unwrap();
        let (_, body) = me.open(path).await.unwrap();
        let streamed: Vec<Bytes> = body.try_collect().await.unwrap();
        assert_eq!(&sha(&streamed.concat()), digest, "{path} (stream)");
        if e.size <= 64 * MIB as u64 {
            assert_eq!(&sha(&me.read(path).await.unwrap()), digest, "{path} (read)");
        }
        assert!(me.download_url(path).await.unwrap().starts_with("https://"));
    }
    assert_eq!(me.read("/library/notes/ideas.txt").await.unwrap(), library()[12].1);

    // Billing sees both users.
    let report = f.fs.usage_report(app, None, 100).await.unwrap();
    assert_eq!(report.len(), 2);
    assert!(report[0].1.used_bytes > 50 * MIB as u64);

    // 12. The user deletes their account: rows and objects are gone, the stranger is untouched.
    let objects_before = f.objects().await;
    let deleted = f.fs.purge_workspace(app, user).await.unwrap();
    assert!(deleted > 20);
    assert_eq!(f.objects().await, objects_before - deleted);
    assert!(matches!(f.fs.open_share(&link.token).await, Err(Error::ShareNotFound)));
    let reborn = f.fs.workspace(app, user, Access::full()).await.unwrap();
    assert!(reborn.list("/", Default::default()).await.unwrap().entries.is_empty());
}
