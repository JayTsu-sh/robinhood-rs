# Integration tests

Two tiers of end-to-end coverage. Per-crate integration tests are under
`crates/<crate>/tests/integration.rs`; this directory holds the
top-level scripts that exercise the deployed binaries.

## `e2e.sh` — no Lustre required

Run against a disposable MariaDB reachable at `127.0.0.1:3306` as root
without a password. The script creates a fresh schema, seeds the `lustre`
catalog scope via SQL, and checks REST, `rbh find`, `rbh report`,
`rbh undelete`, unsupported policy format rejection, metrics, and graceful
shutdown. It also checks uninitialized startup rejection, catalog persistence
across a restart, and nonzero exit on run queue failure.

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

Enable these tests with `RBH_INTEGRATION=1`. The entry-store suite defaults
to a disposable database named `rbh_entries_test` on local MariaDB. It imports
the baseline and recreates its test tables before each case. For another test
server, set `RBH_ENTRY_STORE_TEST_DATABASE_URL` to a dedicated `rbh_entries_test` database.

Other MariaDB-backed suites use databases provisioned before running the tests.
For the REST examples below, initialize each disposable database referenced by
`RBH_TEST_DATABASE_URL` with [schema.sql](../crates/rbh-entry-store/schema.sql).
For example:

```sh
mariadb --execute 'CREATE DATABASE rbh_preview_test'
mariadb rbh_preview_test < crates/rbh-entry-store/schema.sql
```

```sh
RBH_INTEGRATION=1 cargo test -p rbh-entry-store --test integration -- --test-threads=1
RBH_INTEGRATION=1 RBH_TEST_CHANGELOG_USER=cl3 \
    cargo test -p lustre-changelog --test integration -- --test-threads=1
RBH_INTEGRATION=1 cargo test -p lustre-api --test integration -- --test-threads=1
```

### Directory scope preview

Run the REST scenario with a fresh MariaDB database and a Lustre
mount with non-empty `hot` and `cold` pools. The scenario seeds indexed
namespace evidence in MariaDB and reads the mount and pool membership.

```sh
RBH_TEST_DATABASE_URL=mysql://root@127.0.0.1/rbh_preview_test \
  RBH_TEST_LUSTRE_MOUNT=/mnt/lustre \
  cargo test --locked -p rbh-api scoped_draft_preview_evaluates_all_links_without_creating_a_policy_or_run -- --ignored
```

The draft endpoint is `POST /api/policies/preview` and accepts a pool migration policy document. `spec.target.directoryScope` accepts a root
path such as `/team` and optional `excludedSubtrees` such as
`["/team/archive"]`. Paths are relative to the registered Lustre root and
begin with `/`. The response returns a null policy revision, the resolved
resource revisions, and FID counts and logical bytes. The scoped result
also includes the exact decimal text as `estimatedBytesExact`. Directory reason
counts cover objects that match the indexed pool and policy rules or are
pending a heat observation. The candidate estimate covers the catalog's
indexed files before execution budgets and live admission checks. Saved
policies use `POST /api/policies/{id}/preview` with their pinned resource
bindings. Preview also works with a read-only Lustre mount.

The namespace replay regression uses the same isolated database and needs
no live mount.

```sh
RBH_TEST_DATABASE_URL=mysql://root@127.0.0.1/rbh_preview_test \
  cargo test --locked -p rbh-daemon lustre_link_events_keep_all_surviving_paths_on_replay -- --ignored
```

### Console policy wizard

The MariaDB revision test issues two edits with the same `If-Match: "1"`.
One returns the saved revision and the other gets HTTP 409 with
`currentRevision`. The immutable definition is available at
`GET /api/policies/{id}/revisions/{revision}/definition`.

```sh
RBH_TEST_DATABASE_URL=mysql://root@127.0.0.1/rbh_wizard_test \
  cargo test --locked -p rbh-api conditional_policy_edit_creates_one_revision_and_preserves_the_original -- --ignored
```

The Playwright wizard test can also run against the compiled console and a
live, isolated daemon with a disposable database. Register a Lustre fixture
root with writable `hot` and `cold` pools. Index three files on `hot` under
`/team` with logical sizes of 100, 200, and 300 bytes and mtimes at least one
second in the past. Link the 300-byte file outside `/team`. Configure an
existing policy writer with `RBH_POLICY_WRITERS_FILE` and provide its fixture
token and registration ID to Playwright as `RBH_CONSOLE_WIZARD_TEST_TOKEN`
and `RBH_CONSOLE_WIZARD_TEST_FS`. Set `RBH_CONSOLE_TEST_URL` to the test
daemon and run `npm --prefix web run test:e2e`.

### Scoped pool migration

Use a disposable directory that inherits the hot pool layout to test
namespace changes after copying and before exchanging layouts.

```sh
RBH_TEST_LUSTRE_MOUNT=/mnt/lustre \
  RBH_TEST_MIGRATION_HOT_DIR=/mnt/lustre/.rbh-tiering-lab/hot \
  cargo test --locked -p rbh-policy directory_and_link_changes_during_copy -- --ignored
```

For the bundled console test, register the full Lustre mount with HSM,
stripe and OST capabilities in a daemon with a disposable database. Set
`RBH_HSM_PRINCIPAL_FILE` to a workload identity with service account
`tiering-daemon` and configure a test writer. Give Playwright a new empty
test directory beneath the mount in `RBH_CONSOLE_EXECUTION_TEST_ROOT`,
the mount in `RBH_CONSOLE_EXECUTION_TEST_MOUNT`, the registration ID in
`RBH_CONSOLE_EXECUTION_TEST_FS`, and the test writer token in
`RBH_CONSOLE_EXECUTION_TEST_TOKEN`. Run the test with
`npm --prefix web run test:e2e -- policy-execution.spec.ts` against that
daemon. The test creates fixtures, previews and saves a policy through the
six-step wizard, enables it, runs it, and downloads its execution report.
It also verifies periodic scheduling and uses `lfs` to check the placement
of migrated, excluded, and HSM-managed files.

The final check reads the live link namespace immediately before the swap.
Lustre group locking and the swap's data-version guard protect file I/O.
A rename on another client may still land between the namespace check and
the layout exchange. Providing atomic directory-scope enforcement would
require a server-side namespace guard across the exchange.

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
fixture imports the same schema as Compose into its disposable database.
