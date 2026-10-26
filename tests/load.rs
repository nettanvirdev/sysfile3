//! Throughput and latency of every tool under concurrency. Ignored by default; run with
//!
//! ```sh
//! cargo test --release --test load -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Numbers depend on the machine and on Postgres; the point is the shape (what scales, what the
//! cache buys) and that nothing fails under load.

mod common;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::fixture_full;
use futures::{StreamExt, stream};
use object_store::throttle::ThrottleConfig;
use sysfile3::{Access, Config, Edit, Expect, GrepOptions, ListOptions, Workspace};

const USERS: usize = 200;
const CONCURRENCY: usize = 64;

struct Row {
    name: String,
    ops: usize,
    secs: f64,
    p50: Duration,
    p99: Duration,
    errors: usize,
}

async fn bench<F, Fut>(name: &str, ops: usize, conc: usize, f: F) -> Row
where
    F: Fn(usize) -> Fut,
    Fut: Future<Output = sysfile3::Result<()>>,
{
    let errors = AtomicUsize::new(0);
    let start = Instant::now();
    let mut lat: Vec<Duration> = stream::iter(0..ops)
        .map(|i| {
            let fut = f(i);
            let errors = &errors;
            async move {
                let t = Instant::now();
                if let Err(e) = fut.await
                    && errors.fetch_add(1, Ordering::Relaxed) < 3
                {
                    eprintln!("{name}: {e}");
                }
                t.elapsed()
            }
        })
        .buffer_unordered(conc)
        .collect()
        .await;
    let secs = start.elapsed().as_secs_f64();
    lat.sort();
    let pick = |q: f64| lat[((lat.len() as f64 * q) as usize).min(lat.len() - 1)];
    let row = Row { name: name.into(), ops, secs, p50: pick(0.50), p99: pick(0.99), errors: errors.into_inner() };
    print_row(&row);
    row
}

fn header(title: &str) {
    println!("\n{title}");
    println!("{:<34} {:>7} {:>10} {:>9} {:>9} {:>6}", "tool", "ops", "ops/s", "p50 ms", "p99 ms", "errors");
}

fn print_row(r: &Row) {
    println!(
        "{:<34} {:>7} {:>10.0} {:>9.2} {:>9.2} {:>6}",
        r.name,
        r.ops,
        r.ops as f64 / r.secs,
        r.p50.as_secs_f64() * 1e3,
        r.p99.as_secs_f64() * 1e3,
        r.errors
    );
}

async fn users(f: &common::Fixture) -> Arc<Vec<Workspace>> {
    let mut v = Vec::with_capacity(USERS);
    for i in 0..USERS {
        v.push(f.fs.workspace("load", &format!("user-{i}"), Access::full().actor("load")).await.unwrap());
    }
    Arc::new(v)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn every_tool_under_load() {
    let cfg = Config { max_versions: 5, ..Config::default() };
    let f = fixture_full(cfg, 80, None).await;
    let ws = users(&f).await;
    let small = Bytes::from(vec![b'x'; 1024]);
    let medium = Bytes::from(vec![b'y'; 256 * 1024]);
    let text: String = (0..200).map(|i| format!("line {i}: the quick brown fox TODO {}\n", i % 7)).collect();
    let mut rows = Vec::new();

    header(&format!(
        "sysfile3 under load: {USERS} workspaces, {CONCURRENCY} concurrent callers, in-memory object store"
    ));
    let w = ws.clone();
    rows.push(bench("write 1 KiB (new file)", 5000, CONCURRENCY, |i| {
        let (w, b) = (w.clone(), small.clone());
        async move { w[i % USERS].write(&format!("/docs/d{}/f{i}.txt", i % 25), b, Default::default()).await.map(drop) }
    }).await);
    let w = ws.clone();
    rows.push(bench("write 1 KiB (overwrite, versioned)", 5000, CONCURRENCY, |i| {
        let (w, b) = (w.clone(), small.clone());
        async move { w[i % USERS].write(&format!("/docs/d{}/f{i}.txt", i % 25), b, Default::default()).await.map(drop) }
    }).await);
    let w = ws.clone();
    rows.push(
        bench("write 256 KiB", 1000, CONCURRENCY, |i| {
            let (w, b) = (w.clone(), medium.clone());
            async move { w[i % USERS].write(&format!("/media/m{i}.bin"), b, Default::default()).await.map(drop) }
        })
        .await,
    );
    for (i, u) in ws.iter().enumerate() {
        u.write("/notes/journal.md", text.clone(), Default::default()).await.unwrap();
        u.write(&format!("/edit/e{i}.md"), "status: draft\n", Default::default()).await.unwrap();
    }
    let w = ws.clone();
    rows.push(
        bench("stat", 10_000, CONCURRENCY, |i| {
            let w = w.clone();
            async move { w[i % USERS].stat(&format!("/docs/d{}/f{}.txt", i % 25, i % 5000)).await.map(drop) }
        })
        .await,
    );
    let w = ws.clone();
    rows.push(
        bench("read 1 KiB (cached)", 10_000, CONCURRENCY, |i| {
            let w = w.clone();
            async move { w[i % USERS].read(&format!("/docs/d{}/f{}.txt", i % 25, i % 5000)).await.map(drop) }
        })
        .await,
    );
    let w = ws.clone();
    rows.push(
        bench("read 256 KiB", 2000, CONCURRENCY, |i| {
            let w = w.clone();
            async move { w[i % USERS].read(&format!("/media/m{}.bin", i % 1000)).await.map(drop) }
        })
        .await,
    );
    let w = ws.clone();
    rows.push(
        bench("read_text (200 lines)", 5000, CONCURRENCY, |i| {
            let w = w.clone();
            async move { w[i % USERS].read_text("/notes/journal.md", 10, Some(50)).await.map(drop) }
        })
        .await,
    );
    let w = ws.clone();
    rows.push(
        bench("list (folder of ~1 per user)", 5000, CONCURRENCY, |i| {
            let w = w.clone();
            async move { w[i % USERS].list("/docs", ListOptions { depth: 2, ..Default::default() }).await.map(drop) }
        })
        .await,
    );
    let w = ws.clone();
    rows.push(
        bench("edit", 2000, CONCURRENCY, |i| {
            let w = w.clone();
            let n = i / USERS;
            async move {
                let old = if n == 0 { "status: draft".to_string() } else { format!("status: {}", n - 1) };
                w[i % USERS]
                    .edit(&format!("/edit/e{}.md", i % USERS), &[Edit::new(old, format!("status: {n}"))], Expect::Any)
                    .await
                    .map(drop)
            }
        })
        .await,
    );
    let w = ws.clone();
    rows.push(
        bench("mkdir -p", 3000, CONCURRENCY, |i| {
            let w = w.clone();
            async move { w[i % USERS].mkdir(&format!("/tree/a{}/b{i}", i % 10), true).await.map(drop) }
        })
        .await,
    );
    let w = ws.clone();
    rows.push(
        bench("rename (file)", 2000, CONCURRENCY, |i| {
            let w = w.clone();
            async move {
                w[i % USERS]
                    .rename(&format!("/docs/d{}/f{i}.txt", i % 25), &format!("/moved/f{i}.txt"), false)
                    .await
                    .map(drop)
            }
        })
        .await,
    );
    for u in ws.iter() {
        for k in 0..10 {
            u.write(&format!("/kit/part{k}.txt"), "part", Default::default()).await.unwrap();
        }
    }
    let w = ws.clone();
    rows.push(
        bench("copy (folder of 10 files)", 1000, CONCURRENCY, |i| {
            let w = w.clone();
            async move { w[i % USERS].copy("/kit", &format!("/kits/k{i}"), false).await.map(drop) }
        })
        .await,
    );
    let w = ws.clone();
    rows.push(
        bench("delete + restore", 1000, CONCURRENCY, |i| {
            let w = w.clone();
            async move {
                let u = &w[i % USERS];
                let t = u.delete(&format!("/kits/k{i}"), true).await?;
                u.restore(t.id).await.map(drop)
            }
        })
        .await,
    );
    let w = ws.clone();
    rows.push(
        bench("glob **/*.txt", 2000, CONCURRENCY, |i| {
            let w = w.clone();
            async move { w[i % USERS].glob("**/*.txt", "/docs").await.map(drop) }
        })
        .await,
    );
    let w = ws.clone();
    rows.push(
        bench("grep (notes)", 2000, CONCURRENCY, |i| {
            let w = w.clone();
            async move {
                w[i % USERS].grep(GrepOptions { under: "/notes".into(), ..GrepOptions::new("TODO 3") }).await.map(drop)
            }
        })
        .await,
    );
    let w = ws.clone();
    let fs = f.fs.clone();
    rows.push(
        bench("share + open link + read", 2000, CONCURRENCY, |i| {
            let (w, fs) = (w.clone(), fs.clone());
            async move {
                let s = w[i % USERS].share("/notes/journal.md", None).await?;
                fs.open_share(&s.token).await?.read("/").await.map(drop)
            }
        })
        .await,
    );
    let w = ws.clone();
    rows.push(
        bench("versions", 3000, CONCURRENCY, |i| {
            let w = w.clone();
            async move { w[i % USERS].versions(&format!("/docs/d{}/f{}.txt", i % 25, 2000 + i % 3000)).await.map(drop) }
        })
        .await,
    );
    let w = ws.clone();
    rows.push(
        bench("usage", 5000, CONCURRENCY, |i| {
            let w = w.clone();
            async move { w[i % USERS].usage().await.map(drop) }
        })
        .await,
    );
    let w = ws.clone();
    rows.push(
        bench("disk_usage (project folder)", 5000, CONCURRENCY, |i| {
            let w = w.clone();
            async move { w[i % USERS].disk_usage("/docs").await.map(drop) }
        })
        .await,
    );
    let w = ws.clone();
    rows.push(
        bench("breakdown (whole workspace)", 3000, CONCURRENCY, |i| {
            let w = w.clone();
            async move { w[i % USERS].breakdown().await.map(drop) }
        })
        .await,
    );
    let w = ws.clone();
    let chunk = Bytes::from(vec![9u8; 256 * 1024]);
    rows.push(
        bench("write_stream 4 MiB", 200, 32, |i| {
            let (w, c) = (w.clone(), chunk.clone());
            async move {
                let body = stream::iter((0..16).map(move |_| Ok::<_, std::io::Error>(c.clone())));
                w[i % USERS].write_stream(&format!("/video/v{i}.mp4"), body, Default::default()).await.map(drop)
            }
        })
        .await,
    );

    let failed: usize = rows.iter().map(|r| r.errors).sum();
    assert_eq!(failed, 0, "no operation may fail under load");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn a_big_workspace_stays_fast() {
    let f = fixture_full(Config::default(), 80, None).await;
    let big = f.fs.workspace("load", "big", Access::full()).await.unwrap();
    let small = f.fs.workspace("load", "small", Access::full()).await.unwrap();
    small.write("/a/b/file.txt", "x", Default::default()).await.unwrap();
    let files = 20_000;
    header(&format!("one workspace with {files} files across 200 folders, next to a tiny one"));
    let b = big.clone();
    bench("fill: write 200 B", files, CONCURRENCY, |i| {
        let b = b.clone();
        async move {
            b.write(&format!("/f{}/s{}/file{i}.txt", i % 200, i % 7), vec![b'z'; 200], Default::default())
                .await
                .map(drop)
        }
    })
    .await;
    for (name, ws) in [("big", &big), ("small", &small)] {
        let w = ws.clone();
        let path = if name == "big" { "/f17/s2/file1017.txt" } else { "/a/b/file.txt" };
        bench(&format!("stat ({name})"), 5000, CONCURRENCY, |_| {
            let w = w.clone();
            async move { w.stat(path).await.map(drop) }
        })
        .await;
        let w = ws.clone();
        let dir = if name == "big" { "/f17" } else { "/a" };
        bench(&format!("list one folder ({name})"), 3000, CONCURRENCY, |_| {
            let w = w.clone();
            async move { w.list(dir, ListOptions { depth: 2, ..Default::default() }).await.map(drop) }
        })
        .await;
    }
    let w = big.clone();
    bench("glob in one folder (big)", 1000, CONCURRENCY, |_| {
        let w = w.clone();
        async move { w.glob("f17/s3/*.txt", "/").await.map(drop) }
    })
    .await;
    let w = big.clone();
    bench("rename a 100-file folder (big)", 200, 16, |i| {
        let w = w.clone();
        async move { w.rename(&format!("/f{}", i % 200), &format!("/r{i}"), false).await.map(drop) }
    })
    .await;
    assert_eq!(big.usage().await.unwrap().node_count as usize, files + 200 + 200 * 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn what_the_cache_buys_against_a_remote_store() {
    // Roughly what a request to R2 or S3 costs from a nearby server.
    let throttle = ThrottleConfig {
        wait_get_per_call: Duration::from_millis(40),
        wait_put_per_call: Duration::from_millis(60),
        wait_delete_per_call: Duration::from_millis(20),
        wait_list_per_call: Duration::from_millis(50),
        ..ThrottleConfig::default()
    };
    let f = fixture_full(Config::default(), 80, Some(throttle)).await;
    let ws = users(&f).await;
    header("with 40 ms reads and 60 ms writes, like a remote object store");
    let w = ws.clone();
    bench("write 4 KiB", 2000, CONCURRENCY, |i| {
        let w = w.clone();
        async move { w[i % USERS].write(&format!("/m/{i}.md"), vec![b'm'; 4096], Default::default()).await.map(drop) }
    })
    .await;
    let cold = fixture_full(
        Config { cache_bytes: 0, ..Config::default() },
        80,
        Some(ThrottleConfig { wait_get_per_call: Duration::from_millis(40), ..ThrottleConfig::default() }),
    )
    .await;
    let cold_ws = cold.fs.workspace("load", "u", Access::full()).await.unwrap();
    for i in 0..100 {
        cold_ws.write(&format!("/m/{i}.md"), vec![b'm'; 4096], Default::default()).await.unwrap();
    }
    bench("read 4 KiB, no cache", 2000, CONCURRENCY, |i| {
        let w = cold_ws.clone();
        async move { w.read(&format!("/m/{}.md", i % 100)).await.map(drop) }
    })
    .await;
    let w = ws.clone();
    bench("read 4 KiB, cache warm", 2000, CONCURRENCY, |i| {
        let w = w.clone();
        async move { w[i % USERS].read(&format!("/m/{}.md", i % 2000 / USERS * USERS + i % USERS)).await.map(drop) }
    })
    .await;
}
