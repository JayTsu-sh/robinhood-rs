# Integration tests

Two tiers of end-to-end coverage. Per-crate integration tests are under
`crates/<crate>/tests/integration.rs`; this directory holds the
top-level scripts that exercise the deployed binaries. See the
[Rev.8 cutover acceptance report](../docs/REV8-CUTOVER-ACCEPTANCE.md)
for the REST and live Lustre deployment checks.

## `e2e.sh` — no Lustre required

Run against a disposable MariaDB reachable at `127.0.0.1:3306` as root
without a password. The script creates a fresh schema, seeds the `lustre`
catalog scope via SQL, and checks REST, `rbh find`, `rbh report`,
`rbh undelete`, Rev.8 format rejection, metrics, graceful shutdown, and
nonzero exit when the scheduler loses access to its table.

```sh
cargo build --locked --release -p robinhood-rs -p rbh-cli
./tests/e2e.sh
```

The default schema is `rbh_e2e_<pid>` and the daemon listens on port 8088.
Use `RBH_E2E_DB` and `RBH_E2E_PORT` to override them. An existing schema
or a port already in use causes setup to fail before seeding. Set
`RBH_E2E_KEEP_DB=1` to keep the schema for inspection. The script prints
the daemon log path at startup.

The GitHub Actions `Compose smoke` check builds and tests the development
image for pull requests. Run the same check locally with Docker Compose
and BuildKit.

```sh
bash tests/e2e-compose.sh
```

The runner uses an isolated MariaDB container and its loopback interface.
The check publishes no host ports, resolves linked symbols at process
startup, and prints the daemon log on failure.

## `e2e_lustre.sh` — needs a real Lustre mount

Covers what `e2e.sh` deliberately skips: the changelog listener,
fs-scan, orphan sweep, dump/restore round-trip, policy format rejection,
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
