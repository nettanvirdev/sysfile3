# Design

## Two stores, one source of truth

Postgres decides what exists; object storage only holds bytes. A file is a row in `sf3_nodes`
pointing at a row in `sf3_blobs`, whose bytes live at `{prefix}/{workspace}/{blob}`.

**Objects are immutable.** Every write uploads to a fresh blob id first, then commits the rows
that point at it. Readers never see a half-written file, a cached object can never be stale, and
a failed commit leaves at worst an object nothing points at (removed by `sweep_orphans`).

## The tree

`sf3_nodes.path` holds the whole normalized path, compared byte by byte (`COLLATE "C"`). A
folder's subtree is the range `[path || '/', path || '0')`, since `/` sorts immediately before
`0`. So:

- `stat` is one index lookup;
- `list` with any depth is one range scan with a depth filter;
- renaming a folder is one `UPDATE` that rewrites the prefix of every path in the range;
- deleting a folder marks every row in the range with one trash id.

A unique index on `(workspace_id, path) WHERE trash_id IS NULL` makes two live nodes at one path
impossible, while any number of trashed ones may share it.

**Invariant:** every live node's parent is a live folder. Writes create missing parents and lock
existing ones `FOR SHARE` until they commit, so a concurrent delete or move of a parent waits for
the write instead of stranding the new file under a trashed folder.

## Versions and garbage collection

Each change inserts a row in `sf3_versions`. The newest `max_versions` are kept; older rows are
deleted in the same transaction. A blob is garbage once no node and no version points at it:
`collect_garbage` deletes exactly those among the blobs a transaction stopped using, refunds
their bytes, and their objects are deleted after commit. Copies and restored versions point at
existing blobs, so foreign keys keep a shared blob alive as long as anything uses it.

## Quotas

`sf3_workspaces` holds `used_bytes` and `node_count`. A transaction adds up its changes and
applies them in one statement just before commit:

```sql
UPDATE sf3_workspaces SET used_bytes = used_bytes + $2, node_count = node_count + $3
WHERE id = $1 AND ($2 <= 0 OR quota_bytes IS NULL OR used_bytes + $2 <= quota_bytes) AND ...
```

Concurrent writers serialize only on that statement, so the limit can never be crossed, and the
row lock is held for one round trip. A write that would cross it fails and its uploaded object is
deleted.

What counts: every blob once (so kept versions and the trash count, and a copy is free until it
diverges), and every node row, trashed or not.

## Isolation

Every transaction starts with `set_config('sysfile3.workspace', <id>, true)`. Policies on every
table allow only rows of that workspace (or all rows for `*`, which only maintenance and link
lookup use). Tables use `FORCE ROW LEVEL SECURITY`, so even the owning role is filtered. Queries
also name the workspace explicitly; the policies are the second lock.

## Concurrency

- **Writes** lock the target node `FOR UPDATE`; preconditions (`Expect`) are checked under that
  lock. Two creators of one new path race on the unique index; the loser retries and finds the
  file there.
- **Edits** read, compute and upload without holding locks, then commit pinned to the version
  they read; on a conflict, unpinned edits recompute against the new contents (three tries).
- **Moves and deletes** lock the node and, through the range update, its subtree.

## Shares

`sf3_shares` points at a node id, not a path, so a link follows its file through renames. The
token is 244 random bits; only its SHA-256 is stored. Opening a link yields a read-only handle
whose root is the shared node's current path.

## Direct uploads

`begin_upload` reserves a blob id and signs a PUT URL for it, recording the reservation in
`sf3_uploads`. `finish_upload` deletes the reservation first (so a ticket can be used once),
checks the object's size against the reservation, and commits like any write.
