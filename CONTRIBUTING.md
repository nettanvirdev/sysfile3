# Contributing

Thanks for helping. Small, focused pull requests are easiest to review.

## Before you open a pull request

```sh
docker run --rm -d --name sf3-test-pg -e POSTGRES_PASSWORD=sf3 -p 55432:5432 \
  postgres:17-alpine -c max_connections=400
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

- A bug fix comes with a test that fails without it.
- A new operation comes with tests for its edge cases (permissions, confinement, quotas,
  concurrency) in `tests/fs.rs`.
- Schema changes go in a new file under `migrations/` and a new entry in `MIGRATIONS`
  (`src/db.rs`); never edit a released migration.
- Keep every query scoped to its workspace, and inside a transaction from `db::begin`, so
  row-level security applies.

## Design notes

See [docs/design.md](docs/design.md) for how the tree, blobs, quotas, trash and garbage
collection fit together.
