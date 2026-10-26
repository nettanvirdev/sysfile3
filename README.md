# sysfile3

**A multi-tenant virtual file system for AI agents and the apps around them, in Rust.**

Postgres holds the tree. Any S3-compatible store (Cloudflare R2, AWS S3, MinIO, SeaweedFS, Ceph)
holds the bytes. Every user, team or project gets a workspace that nothing else can see into,
and agents get the file tools they expect (`read`, `write`, `edit`, `ls`, `glob`, `grep`, `mv`,
`cp`, `rm`) without ever touching a real disk.

<p align="center">
  <img src="https://raw.githubusercontent.com/nettanvirdev/sysfile3/main/docs/assets/architecture.svg" alt="Your app, AI agents and public links reach files through sysfile3, which keeps the tree in Postgres and the bytes in S3-compatible storage; large files go straight between clients and storage through signed URLs." width="900">
</p>

```rust
use sysfile3::{Access, Edit, Expect, Sysfile3};

let fs = Sysfile3::builder(pool, store).build();
fs.migrate().await?;

// Every call goes through a handle bound to one workspace.
let ws = fs.workspace("my-app", "user-42", Access::full().actor("user-42")).await?;
ws.write("/memory/user.md", "# User\nTimezone: (unknown)\n", Default::default()).await?;
ws.edit("/memory/user.md", &[Edit::new("(unknown)", "Asia/Dhaka")], Expect::Any).await?;

// An agent session can be confined to one folder: its "/" is that folder.
let agent = fs.workspace("my-app", "user-42", Access::full().within("/agents/research")?).await?;
agent.write("/out/report.pdf", pdf_bytes, Default::default()).await?;

// Share a file or folder read-only, with an optional expiry.
let link = ws.share("/sites/launch", None).await?;
let public = fs.open_share(&link.token).await?; // read-only, confined to /sites/launch
```

## Contents

- [What it is for](#what-it-is-for)
- [Features](#features)
- [Getting started](#getting-started)
- [Concepts](#concepts)
- [Operations](#operations)
- [Security model](#security-model)
- [Performance](#performance)
- [Running the tests](#running-the-tests)
- [Roadmap](#roadmap)

## What it is for

The durable files of an assistant platform:

- an agent's memory and identity files (`identity.md`, `user.md`, `heartbeat.md`);
- the user's library: uploads, photos, PDFs, audio, video, documents;
- what agents produce and keep: reports, generated images, exported sites;
- public, read-only sharing of any of it.

It is **not** a scratch disk for running code. Compilers, package managers and editors that
hammer thousands of small files belong in a sandbox (a container or VM) with a real disk. Do the
work there, then move the results into sysfile3, where they are versioned, searchable, shareable
and counted against the user's quota.

## Features

| | |
|---|---|
| **Multi-tenant** | Workspaces per `(app, external id)`. Row-level security in Postgres hides every other workspace's rows from each transaction, as a second lock behind the API. |
| **Virtual paths** | Normalized (NFC Unicode, collapsed slashes), case-sensitive, no `..`, no control characters. Nothing to escape into. |
| **Scoped handles** | Full, read-only or no-share access, optionally confined to a folder (`Access::within`). Errors never reveal paths outside the handle. |
| **Agent file tools** | `read`, `read_text` (line windows), `read_range`, `open` (stream), `write`, `write_stream`, `edit` (exact replace, CRLF aware), `edit_many` (atomic across files), `list` (depth, pages), `stat`, `mkdir`, `rename`, `copy`, `delete`, `glob`, `grep`. |
| **Optimistic concurrency** | Every file has a version. `Expect::Version(n)` makes a write fail with `Conflict` if someone changed the file; `Expect::Absent` only creates. Unpinned edits retry on the new contents. |
| **Versions** | Every change is kept, up to `max_versions` per file. Read or restore any kept version. |
| **Trash** | Deletes are recoverable until purged. Restoring recreates missing parent folders. |
| **Quotas** | Bytes and file count per workspace, enforced atomically under concurrency. Versions and trash count; a copy shares bytes and costs nothing until it changes. |
| **Shares** | Read-only links to a file or folder, with expiry and revocation. Only the token's SHA-256 is stored. Links follow moves and pause while trashed. |
| **Large files** | Streamed multipart writes, ranged and streamed reads, and signed direct upload/download URLs, so big files never pass through your servers. |
| **Search** | `glob` (`**/*.pdf`) and `grep` (linear-time regex, include filters, binary detection). |
| **Caching** | Small file contents are cached in memory. Contents are immutable by design, so the cache is never stale and never needs invalidating. |
| **Storage usage** | Billed usage with quota helpers (MiB, GiB, remaining, percent), the size of any folder or project, a breakdown into current files, old versions and trash, storage by type (images, videos, audio, documents, text, archives), the largest files, and totals across an app. |
| **Maintenance** | Purge old trash, expire unfinished uploads, sweep orphaned objects, delete a whole workspace, usage reports for billing. |

## Getting started

```toml
[dependencies]
sysfile3 = "0.1"
object_store = { version = "0.14", features = ["aws"] }
sqlx = { version = "0.9", default-features = false, features = ["runtime-tokio", "postgres"] }
```

### Cloudflare R2 (or any S3-compatible store)

```rust
use std::sync::Arc;
use object_store::aws::AmazonS3Builder;
use sqlx::postgres::PgPoolOptions;
use sysfile3::{Config, Sysfile3};

let pool = PgPoolOptions::new().max_connections(32).connect(&database_url).await?;
let s3 = Arc::new(
    AmazonS3Builder::new()
        .with_endpoint(format!("https://{account_id}.r2.cloudflarestorage.com"))
        .with_region("auto")
        .with_bucket_name("my-files")
        .with_access_key_id(key_id)
        .with_secret_access_key(secret)
        .build()?,
);
let fs = Sysfile3::builder(pool, s3.clone())
    .signer(s3) // enables begin_upload / download_url
    .config(Config { default_quota_bytes: Some(5 << 30), ..Config::default() })
    .build();
fs.migrate().await?;
```

Moving from R2 to your own S3 later is a configuration change plus a copy of the bucket: object
keys are `{prefix}/{workspace}/{blob}` and never depend on file names.

### Postgres role

Connect as an ordinary role that owns the tables (it runs the migrations), **not** a superuser
or a role with `BYPASSRLS`: those skip row-level security. Tested on Postgres 17.

## Concepts

### Workspaces and access

`Sysfile3::workspace(app, external_id, access)` returns a `Workspace` handle, creating the
workspace on first use with the default quotas. `app` lets several products share a database;
`external_id` is your user, team or project id. sysfile3 never sees accounts or passwords: your
app authenticates people and decides what each handle may do.

| Access | Meaning |
|---|---|
| `Access::full()` | read, write, share, the whole workspace |
| `Access::read_only()` | read only |
| `.no_share()` | everything except creating public links |
| `.within("/agents/a1")` | confined to a folder; it becomes the handle's `/` |
| `.actor("agent:main")` | recorded as `created_by`, `updated_by`, `trashed_by` |

### Writes, versions and conflicts

`write` creates missing folders. Each change bumps the file's `version`; the last
`Config::max_versions` (default 20) are kept and older ones are deleted with their bytes.

```rust
let cur = ws.read_text("/notes.md", 1, None).await?;
ws.edit("/notes.md", &[Edit::new("draft", "final")], Expect::Version(cur.version)).await?;
// Err(Error::Conflict { .. }) if anyone changed it since it was read
```

### Trash

`delete` moves a file or folder (with everything in it) to the trash; the path is free again at
once. `list_trash`, `restore`, `purge` and `empty_trash` work on trash items. Schedule
`purge_trash_older_than(30 days)` to empty it for good.

### Quotas and billing

sysfile3 enforces limits and measures usage; prices, plans and invoices stay in your app.

| sysfile3 | your app |
|---|---|
| `set_quota(app, id, bytes, files)`; writes over it fail with `QuotaExceeded` | decides each plan's limits, sets them on signup and upgrade |
| `usage(...)`, `usage_report(app, after, limit)` | turns usage into money |

### Storage usage

A workspace is whatever you make it: a user, a team or a project. A folder inside one can be a
project too. Every level has its numbers:

```rust
// The workspace: what quotas and billing use.
let u = ws.usage().await?;                     // or fs.usage(app, id) from the server side
println!("{:.1} MiB used, {:?}% of plan", u.used_mib(), u.used_percent());
u.remaining_bytes();                           // Some(bytes left) or None when unlimited

// A folder or project, like `du`: files, folders and bytes as they are now.
let p = ws.disk_usage("/projects/launch").await?;

// Where the space goes, for a storage screen.
let b = ws.breakdown().await?;
// b.file_bytes, b.version_bytes, b.trash_bytes,
// b.by_type: [{ category: "videos", files: 3, bytes: .. }, { category: "images", .. }, ..]

// What to clean up first.
let biggest = ws.largest_files("/", 10).await?;

// Everything an app stores, and one row per workspace for billing.
let total = fs.app_usage("my-app").await?;
let page = fs.usage_report("my-app", None, 1000).await?;
```

`usage` counts every stored byte once: kept versions and the trash count, and a copy shares its
original's bytes until it changes. `disk_usage` and `breakdown` count file by file, so a copy
counts again there. A confined handle's `breakdown` covers only its own folder.

### Shares

```rust
let link = ws.share("/report.pdf", Some(Duration::from_secs(7 * 86_400))).await?;
// Serve it on your own domain:
let view = fs.open_share(&token_from_url).await?; // Error::ShareNotFound if revoked, expired, trashed
let entry = view.stat("/").await?;               // the shared file itself
let bytes = view.read("/").await?;
```

A folder link exposes everything inside it, read-only (for static sites: `view.read("/css/app.css")`).

### Large files

| Size | Write | Read |
|---|---|---|
| up to `max_inline_bytes` (64 MiB) | `write` | `read` |
| up to `max_file_bytes` (5 GiB) | `write_stream`, or `begin_upload` → client PUT → `finish_upload` | `open` (stream), `read_range`, `download_url` |

### Errors

`Error::code()` gives a stable string for HTTP and MCP layers (`not_found`, `conflict`,
`quota_exceeded`, `edit_ambiguous`, …) and `Error::is_client_error()` separates bad requests from
backend failures.

## Operations

Run these from a scheduler; all are safe to run while the service is busy.

| Call | Suggested schedule |
|---|---|
| `purge_trash_older_than(Duration::from_secs(30 * 86_400))` | daily |
| `expire_uploads()` | hourly |
| `sweep_orphans(Duration::from_secs(86_400))` | daily: deletes objects nothing points at (crashes between upload and commit, failed deletes) |
| `purge_workspace(app, id)` | on account deletion |

## Security model

- **Two locks on isolation.** Every query names its workspace, and every transaction also sets
  `sysfile3.workspace`, which Postgres row-level security (forced, so the table owner obeys it
  too) uses to hide and refuse every other workspace's rows.
- **No path escapes.** Paths are names in a database, normalized and validated; `..` is
  refused, and a confined handle joins its root to an already-normalized path.
- **Least privilege per handle.** Read, write and share are separate; confinement hides
  everything outside the root, including in error messages.
- **Links are secrets.** 244 random bits, stored only as SHA-256, revocable, expiring.
- **Immutable contents.** Objects are written once under fresh ids and never rewritten, so a
  half-finished write can never corrupt a file someone is reading.
- **Bounded work.** Sizes, listing pages, glob and grep scans, matches, copies and regex size
  all have limits (`Config`).

Report vulnerabilities privately: see [SECURITY.md](SECURITY.md).

## Performance

`cargo test --release --test load -- --ignored --nocapture --test-threads=1`: 200 workspaces,
64 concurrent callers, Postgres 17 in Docker Desktop on a Windows laptop, in-memory object store.
Numbers on a Linux server next to its database are higher (here every database round trip pays
for Docker's network); what matters is the shape.

<p align="center">
  <img src="https://raw.githubusercontent.com/nettanvirdev/sysfile3/main/docs/assets/throughput.svg" alt="Operations per second by tool, from 6,127 for stat down to 231 for 4 MiB streamed writes, and small reads against a 40 ms store going from 1,024 per second without the cache to 5,735 with it." width="900">
</p>

| Tool | ops/s | p50 ms | p99 ms |
|---|---:|---:|---:|
| write 1 KiB (new file) | 1,775 | 34 | 107 |
| write 1 KiB (overwrite, versioned) | 1,668 | 36 | 76 |
| write 256 KiB | 1,361 | 43 | 71 |
| stat | 6,127 | 9.5 | 16 |
| read 1 KiB (cached) | 6,070 | 9.6 | 16 |
| read_text | 5,869 | 9.9 | 16 |
| list | 4,485 | 13 | 19 |
| edit | 1,676 | 37 | 54 |
| mkdir -p | 2,847 | 22 | 29 |
| rename | 2,533 | 24 | 40 |
| copy (folder of 10 files) | 793 | 79 | 91 |
| delete + restore | 1,278 | 48 | 75 |
| glob | 4,325 | 13 | 18 |
| grep | 2,789 | 22 | 30 |
| share + open link + read | 1,733 | 36 | 49 |
| versions | 2,958 | 20 | 30 |
| usage | 6,089 | 9.7 | 17 |
| disk_usage (project folder) | 4,636 | 13 | 19 |
| breakdown (whole workspace) | 660 | 93 | 166 |
| write_stream 4 MiB | 231 | 137 | 144 |

**Size does not slow a workspace down.** In a workspace of 20,000 files, `stat` and `list` stay
within a few milliseconds of an empty one, and renaming a 100-file folder is one statement.

**The cache matters against a remote store.** With 40 ms object reads (like R2 or S3), small
reads go from about 1,000/s uncached to about 5,700/s with the cache warm.

**One limit to know.** Writes to the *same* workspace update its usage counters in one short
step at commit, which is what keeps quotas exact under concurrency. One workspace hammered by 64
writers at once managed about 135 writes per second in this setup; writes to different
workspaces do not wait on each other.

## Running the tests

The tests need a Postgres superuser to create a database per test (owned by an ordinary role, so
row-level security is really in force):

```sh
docker run --rm -d --name sf3-test-pg -e POSTGRES_PASSWORD=sf3 -p 55432:5432 \
  postgres:17-alpine -c max_connections=400
cargo test                                   # unit, functional and end-to-end tests
cargo test --release --test load -- --ignored --nocapture --test-threads=1
docker stop sf3-test-pg
```

`SYSFILE3_TEST_DB` overrides the database URL (default
`postgres://postgres:sf3@127.0.0.1:55432/postgres`).

- `tests/fs.rs`: every operation and its edge cases: isolation, races, quotas under
  concurrency, trash, versions, shares, uploads, Unicode names, row-level security.
- `tests/e2e.rs`: one user's life on an assistant platform with 20+ file types, byte-for-byte checks.
- `tests/load.rs`: throughput and latency of every tool.

## Roadmap

- `sysfile3d`: a server exposing the same operations over HTTP and as MCP tools, with scoped,
  short-lived tokens for agents.
- A shared Redis tier for the content cache and per-workspace rate limits.
- A full-text index so `grep` scales past `search_scan_limit` files.
- Sync to and from sandbox disks (copy in at session start, write back at the end).

## License

MIT. See [LICENSE](LICENSE).
