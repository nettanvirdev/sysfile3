# Changelog

## 0.1.0

First release.

- Workspaces per `(app, external id)` with Postgres row-level security.
- Scoped handles: full, read-only, no-share, confined to a folder, with an actor.
- File tools: `stat`, `list`, `read`, `read_text`, `read_range`, `open`, `write`, `write_stream`,
  `edit`, `edit_many`, `mkdir`, `rename`, `copy`, `delete`, `glob`, `grep`.
- Versions with a retention limit, restore of any kept version.
- Trash with restore and purge.
- Quotas on bytes and files, exact under concurrency.
- Read-only share links with expiry and revocation.
- Signed direct uploads and downloads.
- In-memory cache of small file contents.
- Maintenance: trash purging, upload expiry, orphan sweeping, workspace deletion, usage reports.
