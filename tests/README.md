# Integration tests

Two tiers of end-to-end coverage. Per-crate integration tests are under
`crates/<crate>/tests/integration.rs`; this directory holds the
top-level scripts that exercise the deployed binaries.

## `e2e.sh` — no Lustre required

Spins up the daemon against a fresh MariaDB, seeds entries directly
via SQL, and validates the REST + CLI surface (`find`, `report`,
`undelete`, threshold fires, metrics). Use this as the fast smoke
test during normal development.

```sh
cargo build --release
./tests/e2e.sh
```

## `e2e_lustre.sh` — needs a real Lustre mount

Covers what `e2e.sh` deliberately skips: the changelog listener,
fs-scan, orphan sweep, dump/restore round-trip, dry-run policy,
stripe-distribution, and `rbh du`.

Prerequisites:

* Writable Lustre mount (default `/lustre`; override with `LUSTRE_MOUNT`)
* Pre-registered changelog user id on the MDS, passed via
  `RBH_CHANGELOG_USER` (e.g. `cl7`). See
  `.claude/memory/lustre_operator_setup.md` for the registration
  command on the MDS host.
* `lfs` on `PATH` (for `path2fid` inside assertions)

```sh
cargo build --release
RBH_CHANGELOG_USER=cl7 ./tests/e2e_lustre.sh
```

Each scenario prints a labeled heading, so a failure tells you which
stage of the pipeline broke.

## Per-crate integration tests

Opt-in via `RBH_INTEGRATION=1`; all require a local MariaDB reachable
as root:

```sh
RBH_INTEGRATION=1 cargo test -p rbh-entry-store --test integration -- --test-threads=1
RBH_INTEGRATION=1 RBH_TEST_CHANGELOG_USER=cl3 \
    cargo test -p lustre-changelog --test integration -- --test-threads=1
RBH_INTEGRATION=1 cargo test -p lustre-api --test integration -- --test-threads=1
```

### What's covered where

| Layer | File |
|---|---|
| Lustre FFI (path→FID, stripe, MDT enum) | `lustre-api/tests/integration.rs` |
| Changelog listener (CREAT / UNLNK / RENME / dedup / ack) | `lustre-changelog/tests/integration.rs` |
| DB (upsert, batch, remove, cursor, sweep_orphans, subtree_totals) | `rbh-entry-store/tests/integration.rs` |
| Full stack via HTTP + CLI | `tests/e2e.sh`, `tests/e2e_lustre.sh` |

## Adding scenarios

For full-stack scenarios: extend `e2e_lustre.sh`. The file is broken
into numbered scenarios; add a new one at the tail before the shutdown
step. Use `wait_for_entry` / `wait_for_absence` helpers — they already
encode the SETTLE budget and fail loudly.

For narrower coverage of one crate: add a `#[tokio::test]` function to
the relevant `crates/<crate>/tests/integration.rs`. The reset-per-test
pattern (drop tables, let migrations recreate) keeps tests isolated.
