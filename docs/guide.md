# The sysfile3 guide

Everything sysfile3 does, in the order you need it, with code you can copy. Every snippet here
comes from [`examples/tour.rs`](../examples/tour.rs), which runs top to bottom against a real
Postgres:

```sh
docker run --rm -d --name sf3-demo -e POSTGRES_PASSWORD=demo -p 55433:5432 postgres:17-alpine
DATABASE_URL=postgres://postgres:demo@127.0.0.1:55433/postgres cargo run --example tour
docker stop sf3-demo
```

## Contents

1. [How it works, in one minute](#1-how-it-works-in-one-minute)
2. [Install and set up](#2-install-and-set-up)
3. [Workspaces and access](#3-workspaces-and-access)
4. [Paths](#4-paths)
5. [Writing files](#5-writing-files)
6. [Reading files](#6-reading-files)
7. [Editing text](#7-editing-text)
8. [Folders, moving and copying](#8-folders-moving-and-copying)
9. [The trash](#9-the-trash)
10. [Versions](#10-versions)
11. [Search](#11-search)
12. [Agents: confined handles](#12-agents-confined-handles)
13. [Public links](#13-public-links)
14. [Storage, quotas and billing](#14-storage-quotas-and-billing)
15. [Errors](#15-errors)
16. [Maintenance jobs](#16-maintenance-jobs)
17. [Configuration reference](#17-configuration-reference)
18. [Recipes](#18-recipes)
19. [Limits and FAQ](#19-limits-and-faq)

---

## 1. How it works, in one minute

![Architecture](assets/architecture.svg)

- **Postgres** stores the tree: every file and folder is a row with its path, size, type,
  version and owner. It is the source of truth.
- **Object storage** (Cloudflare R2, AWS S3, MinIO, …) stores the bytes. Each version of each
  file is one object, written once and never changed.
- **A workspace** is one private tree: a user's, a team's or a project's. You get a
  `Workspace` handle for it and call methods on the handle (`write`, `read`, `edit`, …).
- **Your app decides who may do what.** sysfile3 has no logins. You authenticate the person or
  agent, then open a handle with exactly the access they should have.

## 2. Install and set up

### 2.1 Add the crates

```toml
[dependencies]
sysfile3 = "0.2"
object_store = { version = "0.14", features = ["aws"] }
sqlx = { version = "0.9", default-features = false, features = ["runtime-tokio", "postgres"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

### 2.2 Postgres

Use a recent Postgres (tested on 17). Create a dedicated database and an **ordinary**
role that owns it. Do not connect as a superuser or a role with `BYPASSRLS`: they skip the
row-level security that keeps workspaces apart.

```sql
CREATE ROLE sysfile3 LOGIN PASSWORD 'change-me';
CREATE DATABASE sysfile3 OWNER sysfile3;
```

With PgBouncer, transaction pooling works: sysfile3 only sets per-transaction settings.

### 2.3 Object storage

**Cloudflare R2**

```rust
use std::sync::Arc;
use object_store::aws::AmazonS3Builder;

let r2 = Arc::new(
    AmazonS3Builder::new()
        .with_endpoint(format!("https://{account_id}.r2.cloudflarestorage.com"))
        .with_region("auto")
        .with_bucket_name("my-files")
        .with_access_key_id(access_key_id)
        .with_secret_access_key(secret_access_key)
        .build()?,
);
```

**AWS S3**: the same builder with `.with_region("eu-west-1")` and no endpoint, or
`AmazonS3Builder::from_env()` to read the standard `AWS_*` variables.

**MinIO or SeaweedFS (self-hosted)**: the same builder with
`.with_endpoint("http://minio:9000").with_allow_http(true)`.

**Tests**: `object_store::memory::InMemory::new()`.

### 2.4 Build it and create the tables

```rust
use sqlx::postgres::PgPoolOptions;
use sysfile3::{Config, Sysfile3};

let pool = PgPoolOptions::new().max_connections(32).connect(&database_url).await?;
let fs = Sysfile3::builder(pool, r2.clone())
    .signer(r2) // optional: enables signed upload and download URLs
    .config(Config { default_quota_bytes: Some(5 << 30), ..Config::default() })
    .build();
fs.migrate().await?; // safe to run on every start, from many processes at once
```

`Sysfile3` is cheap to clone. Make one at startup and share it.

## 3. Workspaces and access

```rust
use sysfile3::Access;

let me     = fs.workspace("my-app", "user-42", Access::full().actor("user-42")).await?;
let agent  = fs.workspace("my-app", "user-42", Access::full().actor("agent:main")).await?;
let viewer = fs.workspace("my-app", "user-42", Access::read_only()).await?;
```

- **`app`** separates products that share one database (`"chat"`, `"studio"`).
- **`external_id`** is your id for the owner: a user id, a team id or a project id. The
  workspace is created on first use, with the default quotas from `Config`.
- **`Access`** is what this handle may do:

| Builder | Allows |
|---|---|
| `Access::full()` | read, write, delete, share |
| `Access::read_only()` | read only |
| `.no_share()` | everything except making public links |
| `.within("/agents/research")?` | only that folder, which becomes the handle's `/` |
| `.actor("agent:main")` | records who acted, in `created_by`, `updated_by`, `trashed_by` |

**Users, teams and projects.** Pick the owner that should have its own storage limit:

| You want | `external_id` |
|---|---|
| a private drive per user | the user id |
| a shared drive per team | the team id (give each member a handle on it) |
| a separate drive per project | the project id |
| projects inside a team drive | the team id, and one folder per project |

**Open a handle per request.** Handles are cheap, and the workspace id is cached in memory, so
build one from the authenticated caller each time rather than storing them.

## 4. Paths

- Paths start at the handle's root: `/notes/todo.md`. A leading `/` is optional.
- `//`, trailing `/` and `.` are tidied away; `..` is refused.
- Names are case-sensitive and Unicode-normalized (NFC), so `é` typed two ways is one name.
- No control characters or backslashes; at most 255 bytes per name, 4096 per path, 64 levels.

```rust
use sysfile3::VPath;
assert_eq!(VPath::parse("notes//./todo.md")?.as_str(), "/notes/todo.md");
assert!(VPath::parse("/a/../b").is_err());
```

## 5. Writing files

### 5.1 `write`: up to 64 MiB in one call

```rust
let e = agent.write("/identity.md", "# Identity\nName: Juno\n", Default::default()).await?;
// e.path, e.version (1), e.size, e.mime ("text/markdown"), e.created_by ("agent:main")
```

Missing folders are created. Writing again replaces the contents and keeps the old version.

Options:

```rust
use sysfile3::{Expect, WriteOptions};

// Only create, never overwrite: Error::AlreadyExists if the path is taken.
agent.write("/identity.md", "again", WriteOptions { expect: Expect::Absent, mime: None }).await;

// Only replace the version you read: Error::Conflict if someone changed it.
agent.write("/notes.md", text, WriteOptions { expect: Expect::Version(3), mime: None }).await?;

// Set the content type yourself (otherwise it is guessed from the name).
me.write("/data.bin", bytes, WriteOptions { mime: Some("application/x-custom".into()), ..Default::default() }).await?;
```

### 5.2 `write_stream`: any size, never all in memory

```rust
use bytes::Bytes;
use futures::stream;

let chunks = (0..4).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![7u8; 256 * 1024])));
me.write_stream("/library/video.mp4", stream::iter(chunks), Default::default()).await?;
```

Any `Stream<Item = std::io::Result<Bytes>>` works: an HTTP request body, a file, a pipe. If the
stream fails or runs past the size limit or the quota, the upload is cancelled and nothing is
left behind.

### 5.3 Direct uploads: the client sends the bytes to storage

For big uploads from browsers and phones, skip your server entirely. Needs a signer.

```rust
// 1. Your server reserves the upload and gets a signed URL.
let ticket = me.begin_upload("/library/photo.jpg", size_in_bytes, Default::default()).await?;
// 2. Send ticket.url to the client. The client uploads the bytes:
//      curl -X PUT --upload-file photo.jpg "<ticket.url>"
// 3. The client tells your server it is done, and your server commits it:
let entry = me.finish_upload(ticket.id, Expect::Any).await?;
// Or abandon it: me.cancel_upload(ticket.id).await?;
```

The uploaded object must be exactly the reserved size; each ticket finishes once; unfinished
tickets expire after `Config::upload_ttl` and are cleaned up by `expire_uploads`.

## 6. Reading files

```rust
let entry  = me.stat("/identity.md").await?;            // size, type, version, who and when
let here   = me.exists("/nope").await?;                 // false
let bytes  = me.read("/identity.md").await?;            // whole file, up to 64 MiB
let window = me.read_text("/identity.md", 2, Some(10)).await?; // lines 2..11 of a text file
let head   = me.read_range("/library/report.pdf", 0, 8).await?; // bytes 0..8
let (entry, body) = me.open("/library/video.mp4").await?;      // a stream of chunks, any size
let url    = me.download_url("/library/report.pdf").await?;    // signed GET URL (needs a signer)
```

`read_text` returns `lines`, `first_line`, `total_lines` and the file's `version`, which you can
pass to an edit (see below).

**Listing folders**:

```rust
use sysfile3::ListOptions;

let page = me.list("/", ListOptions { depth: 2, limit: Some(100), after: None }).await?;
for e in &page.entries {
    println!("{} {}", if e.is_dir() { "dir " } else { "file" }, e.path);
}
if let Some(next) = page.next {
    let page2 = me.list("/", ListOptions { depth: 2, limit: Some(100), after: Some(next) }).await?;
}
```

Entries come in path order. `depth: 1` lists only the folder's own children.

## 7. Editing text

Agents edit files by replacing exact text, like a person using find-and-replace.

```rust
use sysfile3::{Edit, Expect};

// Replace one unique piece of text.
let out = agent.edit("/user.md", &[Edit::new("Name: (unknown)", "Name: Rahim")], Expect::Any).await?;
// out.replacements == 1, out.entry.version went up by one

// Replace every occurrence.
agent.edit("/user.md", &[Edit::all("(unknown)", "-")], Expect::Any).await?;
```

- The text must appear **exactly once** unless you use `Edit::all`; otherwise you get
  `EditNotFound` or `EditAmbiguous { count }`, so the agent can add more context and retry.
- Several edits in one call apply in order, each to the result of the previous one.
- Files with Windows line endings (`\r\n`) are handled when the edit uses `\n`.
- Binary files are refused (`NotText`), and so are files over `max_text_bytes` (8 MiB).

**Concurrent editors.** With `Expect::Any`, if someone changes the file between the read and
the save, the edit is re-applied to the new contents (up to three times), so two agents editing
different lines never lose each other's work. To insist on the exact version you looked at, pin
it:

```rust
let cur = agent.read_text("/user.md", 1, None).await?;
agent.edit("/user.md", &[Edit::new("Timezone: (unknown)", "Timezone: Asia/Dhaka")], Expect::Version(cur.version)).await?;
// Error::Conflict { expected, actual, .. } if it changed since
```

**Several files at once, all or nothing**:

```rust
use sysfile3::FileEdits;

agent.edit_many(vec![
    FileEdits { path: "/user.md".into(), edits: vec![Edit::new("Rahim", "Rahim K.")], expect: Expect::Any },
    FileEdits { path: "/identity.md".into(), edits: vec![Edit::all("Juno", "Juno AI")], expect: Expect::Any },
]).await?;
```

## 8. Folders, moving and copying

```rust
me.mkdir("/projects/launch/assets", true).await?;   // true: create parents, fine if it exists
me.mkdir("/projects/new", false).await?;            // false: parent must exist, path must be free

me.rename("/library/report.pdf", "/projects/launch/report.pdf", false).await?; // move or rename
me.copy("/projects/launch", "/projects/launch-v2", false).await?;              // file or folder
let item = me.delete("/projects/launch-v2", true).await?;                      // to the trash
```

- `rename` and `copy` take the **new path**, not a folder to drop into.
- The last argument is `overwrite`: without it, an existing target is `AlreadyExists`; with it,
  the target goes to the trash first (a folder can only replace an empty folder).
- Renaming a folder with thousands of files is one database statement.
- A copy shares its bytes with the original and costs no storage until one side changes.
- `delete` on a non-empty folder needs `recursive = true`. The root cannot be deleted or moved.

## 9. The trash

```rust
let items = me.list_trash().await?;        // newest first: id, path, kind, size, when, who
me.restore(item.id).await?;                // back where it was; recreates missing parent folders
me.purge(item.id).await?;                  // gone for good, storage freed
let n = me.empty_trash().await?;           // purge everything in the trash
```

Restoring fails with `AlreadyExists` if something new now lives at that path. Trash counts
against the quota until purged; schedule `purge_trash_older_than` (see Maintenance).

## 10. Versions

Every change to a file keeps the previous contents, up to `max_versions` (default 20).

```rust
for v in me.versions("/user.md").await? {          // newest first
    println!("v{} {} bytes by {:?} at {}", v.version, v.size, v.created_by, v.created_at);
}
let old = me.read_version("/user.md", 1).await?;
me.restore_version("/user.md", 1).await?;          // becomes a new version; history is kept
```

## 11. Search

```rust
use sysfile3::GrepOptions;

// glob: * stays inside a folder, ** crosses folders.
let pdfs  = me.glob("**/*.pdf", "/").await?;           // every PDF
let notes = me.glob("notes/2026/*.md", "/").await?;    // one folder

// grep: lines matching a regular expression in text files.
let hits = me.grep(GrepOptions {
    ignore_case: true,
    include: Some("*.md".into()),   // by name; use a path like "notes/**/*.md" to match paths
    under: "/notes".into(),         // a folder, or a single file
    ..GrepOptions::new("todo")
}).await?;
for m in &hits.matches {
    println!("{}:{}: {}", m.path, m.line, m.text);
}
```

Binary files and files over `max_text_bytes` are skipped (`files_skipped`). Both calls stop at
their limits and say so with `truncated`. The regex engine always runs in linear time.

## 12. Agents: confined handles

Give each agent only the folder it needs. Inside the handle, that folder is `/`; nothing
outside can be named, listed, searched or even mentioned in an error.

```rust
let researcher = fs.workspace("my-app", "user-42",
    Access::full().within("/agents/research")?.actor("agent:research")).await?;

researcher.write("/out/findings.md", "found it\n", Default::default()).await?;
// the owner sees it at /agents/research/out/findings.md
researcher.read("/identity.md").await;        // Err(NotFound("/identity.md"))
researcher.read("../../identity.md").await;   // Err(InvalidPath)
```

A read-only viewer: `Access::read_only()`. Its writes, deletes and shares fail with
`PermissionDenied`.

## 13. Public links

```rust
use std::time::Duration;

let link = me.share("/site", Some(Duration::from_secs(7 * 86_400))).await?; // or None: no expiry
// link.token is shown only now. Put it in your URL, for example https://files.example.com/s/<token>
let live = me.shares().await?;               // links that still work
me.revoke_share(link.info.id).await?;        // turn one off for good
```

**Serving links on your domain**: open the token, then read with the read-only handle you get.
The handle's `/` is the shared file or folder:

```rust
let view = fs.open_share(&token).await?;      // Err(ShareNotFound) if revoked, expired or trashed
let root = view.stat("/").await?;
if root.is_dir() {
    // a shared folder: serve /index.html, /css/app.css, ... relative to it
    let page = view.read("/index.html").await?;
} else {
    // a shared file: serve view.read("/") or redirect to view.download_url("/")
}
```

Links follow their file when it is moved or renamed, stop while it is in the trash, and are
deleted with it. Only the SHA-256 of a token is stored.

## 14. Storage, quotas and billing

### 14.1 How much is stored

| Question | Call |
|---|---|
| How much does this user/team/project use, against its plan? | `ws.usage()` or `fs.usage(app, id)` |
| How big is this folder or project? | `ws.disk_usage("/projects/launch")` |
| Where does the space go (files, old versions, trash, by type)? | `ws.breakdown()` |
| What should they delete first? | `ws.largest_files("/", 10)` |
| How much does the whole app store? | `fs.app_usage(app)` |
| Usage of every workspace, for billing | `fs.usage_report(app, after, limit)` |

```rust
let u = me.usage().await?;
println!("{:.1} MiB used", u.used_mib());               // also used_gib()
println!("{:?}% of the plan", u.used_percent());       // None when unlimited
println!("{:?} bytes left", u.remaining_bytes());      // also remaining_nodes()

let p = me.disk_usage("/projects/launch").await?;     // files, folders (inside), bytes

let b = me.breakdown().await?;
println!("files {}, old versions {}, trash {}", b.file_bytes, b.version_bytes, b.trash_bytes);
for t in &b.by_type {                                  // largest first
    println!("{}: {} files, {} bytes", t.category, t.files, t.bytes);
    // categories: images, videos, audio, documents, text, archives, other
}
```

**Two ways of counting.** `usage` is what quotas and billing use: every stored byte once, so
kept versions and the trash count, and a copy is free until it changes. `disk_usage` and
`breakdown` count file by file, so a copy counts again there.

### 14.2 Quotas

```rust
// On signup, upgrade or downgrade. None = unlimited.
fs.set_quota("my-app", "user-42", Some(10 << 30), Some(100_000)).await?;
```

- Writes that would go over fail with `QuotaExceeded(Quota::Bytes)` or
  `QuotaExceeded(Quota::Nodes)`, and nothing is stored.
- The check is exact even with many writers at once.
- Lowering a quota below current usage deletes nothing; new writes are refused until usage
  drops (by deleting and purging).
- New workspaces start with `Config::default_quota_bytes` / `default_quota_nodes`.

### 14.3 Billing

sysfile3 measures; your app charges. Page through `usage_report` on a schedule and turn bytes
into money however your plans work:

```rust
let mut after: Option<String> = None;
loop {
    let page = fs.usage_report("my-app", after.as_deref(), 1000).await?;
    let Some((last, _)) = page.last() else { break };
    after = Some(last.clone());
    for (external_id, usage) in &page {
        // record usage.used_bytes for external_id
    }
}
```

## 15. Errors

Every error has a stable `code()` for APIs and agent tools, and `is_client_error()` tells
caller mistakes from backend failures.

| `code()` | Meaning | Suggested HTTP |
|---|---|---|
| `not_found` | no such file, folder, version, trash item or upload | 404 |
| `already_exists` | the path is taken (`Expect::Absent`, `rename`/`copy` without overwrite) | 409 |
| `not_a_directory` / `is_a_directory` | wrong kind for the operation | 409 |
| `not_empty` | deleting a non-empty folder without `recursive` | 409 |
| `conflict` | the file changed since the version you named | 412 |
| `quota_exceeded` | over the workspace's byte or file limit | 507 |
| `too_large` | over a size limit (`max_inline_bytes`, `max_text_bytes`, `max_file_bytes`) | 413 |
| `not_text` | a text operation on a binary file | 415 |
| `edit_not_found` / `edit_ambiguous` | the text to replace is missing / appears more than once | 422 |
| `invalid_path` / `invalid_input` | a bad path or argument | 400 |
| `permission_denied` | the handle may not do this | 403 |
| `share_not_found` | the link does not exist, expired or was revoked | 404 |
| `unsupported` | e.g. signed URLs without a signer | 501 |
| `database` / `storage` | Postgres or object storage failed | 503 |
| `io` | reading the upload stream failed | 400 |

## 16. Maintenance jobs

Run these on a schedule (any scheduler; here with tokio). All are safe while the service is
busy and from several instances at once.

```rust
let fs2 = fs.clone();
tokio::spawn(async move {
    let mut every_hour = tokio::time::interval(std::time::Duration::from_secs(3600));
    loop {
        every_hour.tick().await;
        let _ = fs2.expire_uploads().await;                                        // unfinished direct uploads
        let _ = fs2.purge_trash_older_than(std::time::Duration::from_secs(30 * 86_400)).await; // 30-day trash
        let _ = fs2.sweep_orphans(std::time::Duration::from_secs(86_400)).await;   // objects nothing points at
    }
});
```

When an account is deleted: `fs.purge_workspace(app, id).await?` removes every row and object
of that workspace. The same id can be opened again later as a fresh, empty workspace.

## 17. Configuration reference

`Config::default()` suits a multi-user service. Change any field:

```rust
let cfg = Config { max_versions: 50, default_quota_bytes: Some(5 << 30), ..Config::default() };
```

| Field | Default | What it controls |
|---|---|---|
| `key_prefix` | `"sysfile3"` | object keys are `{key_prefix}/{workspace}/{blob}` |
| `max_file_bytes` | 5 GiB | largest file, however it arrives |
| `max_inline_bytes` | 64 MiB | largest `write` / `read` in one piece |
| `max_text_bytes` | 8 MiB | largest file for `read_text`, `edit`, `grep` |
| `max_versions` | 20 | versions kept per file, the current one included |
| `default_quota_bytes` | `None` (unlimited) | byte quota for new workspaces |
| `default_quota_nodes` | `None` (unlimited) | file + folder quota for new workspaces |
| `list_limit` | 1000 | most entries per `list` page |
| `search_scan_limit` | 5000 | most files `glob` and `grep` look at per call |
| `grep_max_matches` | 500 | most lines `grep` returns |
| `max_copy_nodes` | 10,000 | most files and folders one `copy` creates |
| `cache_bytes` | 256 MiB | memory for the content cache (0 turns it off) |
| `cache_item_bytes` | 256 KiB | files larger than this are never cached |
| `cache_idle` | 1 hour | a cached file is dropped after going unread this long |
| `upload_ttl` | 1 hour | how long a direct-upload ticket stays valid |
| `download_ttl` | 15 minutes | how long a `download_url` stays valid |

## 18. Recipes

### An agent's memory files

```rust
let agent = fs.workspace("my-app", &user_id, Access::full().within("/memory")?.actor("agent:main")).await?;
// First run: create without clobbering.
let only_new = WriteOptions { expect: Expect::Absent, mime: None };
for (path, body) in [("/identity.md", IDENTITY), ("/user.md", USER), ("/heartbeat.md", HEARTBEAT)] {
    match agent.write(path, body, only_new.clone()).await {
        Ok(_) | Err(sysfile3::Error::AlreadyExists(_)) => {}
        Err(e) => return Err(e.into()),
    }
}
// Every heartbeat: read, change, save pinned to what was read.
let cur = agent.read_text("/heartbeat.md", 1, None).await?;
let old = cur.lines.iter().find(|l| l.starts_with("last_run:")).unwrap();
agent.edit("/heartbeat.md", &[Edit::new(old.clone(), format!("last_run: {now}"))], Expect::Version(cur.version)).await?;
```

### Giving an agent file tools

Map each tool to one call on a handle opened for that agent:

| Tool | Call |
|---|---|
| `read(path, offset?, limit?)` | `read_text(path, offset, limit)`; `read` for binary |
| `write(path, content)` | `write(path, content, Default::default())` |
| `edit(path, old, new, replace_all?)` | `edit(path, &[Edit { old, new, replace_all }], Expect::Any)` |
| `ls(path, depth?)` | `list(path, ListOptions { depth, .. })` |
| `glob(pattern, path?)` | `glob(pattern, path)` |
| `grep(pattern, path?, include?, ignore_case?)` | `grep(GrepOptions { .. })` |
| `mkdir(path)` | `mkdir(path, true)` |
| `mv(from, to)` / `cp(from, to)` | `rename(from, to, false)` / `copy(from, to, false)` |
| `rm(path, recursive?)` | `delete(path, recursive)` (recoverable from the trash) |

Return `err.to_string()` to the model: the messages say what went wrong and what to do next
(for example, "appears 3 times; include more context or replace all").

### Results from a sandbox

Run code in a container or VM with a real disk; when it finishes, move what should be kept:

```rust
for (path, bytes) in sandbox_outputs {
    me.write(&format!("/projects/launch/out/{path}"), bytes, Default::default()).await?;
}
```

### Publishing a static site

```rust
me.write("/site/index.html", html, Default::default()).await?;
me.write("/site/css/app.css", css, Default::default()).await?;
let link = me.share("/site", None).await?; // serve it as in section 13
```

### Plan upgrade and account deletion

```rust
fs.set_quota("my-app", &user_id, Some(100 << 30), None).await?; // upgrade: takes effect at once
fs.purge_workspace("my-app", &user_id).await?;                  // deletion: rows and objects
```

### Testing your own app

Use `InMemory` for storage and a throwaway `postgres:17-alpine` container; see
[`tests/common/mod.rs`](../tests/common/mod.rs) for a harness that creates a fresh database per
test, owned by an ordinary role so row-level security is really in force.

## 19. Limits and FAQ

**Is it a real disk I can mount?** No. It is a file system for apps and agents to call. For
code that needs a disk, use a sandbox and move the results in.

**How big can a workspace get?** Large: listing, `stat`, renames and deletes stay fast with tens
of thousands of files. `glob` and `grep` look at up to `search_scan_limit` files per call and
say `truncated` when there were more.

**How fast is it?** See the README's performance section. Writes to *different* workspaces
never wait on each other; many simultaneous writers on the *same* workspace briefly queue on
its usage counter at commit, which is what keeps quotas exact.

**Can I move from R2 to my own S3 later?** Yes: copy the bucket and point the builder at the new
store. Object keys never depend on file names.

**Is anything deleted automatically?** Only versions beyond `max_versions` and whatever your
maintenance jobs purge. Deleted files sit in the trash until you purge them.

**Where do I report a security problem?** Privately, as described in
[SECURITY.md](../SECURITY.md).
