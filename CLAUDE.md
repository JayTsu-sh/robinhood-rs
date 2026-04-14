# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

robinhood-rs is a Rust rewrite of the [Robinhood Policy Engine](https://github.com/cea-hpc/robinhood) for Lustre filesystems. Instead of config-file-based policies, it uses a RESTful API backed by a database (via [scheduler-rs](../scheduler-rs)) for policy management. The project reuses Lustre changelog infrastructure concepts from [rust-terrasync](../rust-terrasync) but reimplements them idiomatically in Rust.

## Build & Test

```bash
# Build everything (requires lustre-client package for liblustreapi headers)
cargo build --workspace

# Run unit tests only (no Lustre mount needed)
cargo test --workspace --lib

# Run a single crate's tests
cargo test -p lustre-api --lib
cargo test -p lustre-changelog --lib

# Integration tests require a live Lustre mount at /lustre (testfs on 192.168.50.247)
RBH_INTEGRATION=1 cargo test -p lustre-api --test integration -- --test-threads=1
RBH_INTEGRATION=1 RBH_TEST_CHANGELOG_USER=cl3 cargo test -p lustre-changelog --test integration -- --test-threads=1

# Full gate check (must pass before every commit)
cargo check --workspace --all-targets
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --lib
```

## Architecture

### Workspace layout

The root `Cargo.toml` is a hybrid workspace + package. `cargo run` at the root launches the `robinhood` binary (a thin shim over `rbh-daemon::run()`).

**Implemented crates (Phase 1):**

- **`lustre-api`** — Sync FFI wrapper around `liblustreapi`. Bindgen generates types/constants from Lustre headers; function declarations are hand-written in `src/sys.rs`. All `LustreApi` methods are sync (blocking FFI); async callers use `tokio::task::spawn_blocking`. Links against `-llustreapi` at build time. MDT discovery uses `llapi_get_obd_count` + `llapi_obd_statfs` (not subprocess). Changelog user registration uses `lctl` subprocess (no liblustreapi equivalent — only works on MDS host).

- **`lustre-changelog`** — Async domain layer. `ChangelogListener::spawn()` runs a blocking recv loop on `spawn_blocking`, emits `EventBatch` through bounded mpsc, accepts `EventAck` back for watermark-tracked `clear_changelog`. Implements per-FID dedup (three robinhood-C rules: IGNORE_CANCEL, IGNORE_MASK for data-change collapse) and time/size-windowed batching.

**Skeleton crates (stubs for later phases):**
`rbh-observability`, `rbh-entry-store`, `rbh-predicate`, `rbh-fs-scan`, `rbh-policy`, `rbh-actions`, `rbh-api`, `rbh-daemon`, `rbh-cli`

### Key design constraints

- **Lustre 2.12+ assumed.** No runtime feature detection for LU-543/LU-1331. Changelog records use the post-LU-1331 single-record rename format.
- **Policies live in the database, not config files.** Triggers are `TriggerSpec` JSON fields inside policy REST bodies. scheduler-rs's `job-loader` feature must never be enabled.
- **No auth.** Trusted microservice — no authn/authz/tokens/rate-limiting in rbh-api.
- **Persistence boundary:** scheduler-rs owns `rbh_scheduler` DB; robinhood-rs owns `rbh_entries` DB. No cross-DB foreign keys.
- **Error handling:** `thiserror` in library crates, `anyhow` only in binary `main()` functions.
- **Observability:** every public async fn gets `#[tracing::instrument]` with correlation fields. Library crates depend on `tracing` only; subscriber setup lives in `rbh-observability`.

### Data flow (changelog ingestion)

```
Lustre MDT → liblustreapi (blocking FFI) → ChangelogListener (spawn_blocking thread)
  → parse (RecView → ChangelogEvent enum) → dedup (per-FID, 3 rules) → batcher
  → bounded mpsc → async consumer → EventAck back → clear_changelog + cursor commit
```

### Related projects (sibling directories)

- `/root/lustre/robinhood` — Original C codebase being rewritten. Key reference files for semantics.
- `/root/rust/rust-terrasync` — Has Lustre changelog FFI code. Referenced but NOT directly depended on; code was lifted with edits per terrasync_copy_audit decisions.
- `/root/rust/scheduler-rs` — Policy scheduling backbone. Used as a path dependency (`../scheduler-rs`). Can be modified upstream when needed.

## Code Correctness Rules (from review)

These rules were derived from code review findings and apply to all future code in this repo:

1. **Never `Handle::block_on()` inside `spawn_blocking`.** It panics on `current_thread` runtimes (used by `#[tokio::test]`). Use `Handle::spawn` (fire-and-forget) or send work back to the async side via a channel.

2. **Error detection must be structural, not string-based.** Match on typed error variants (e.g. `LustreApiError::Ffi { errno, .. }` with `errno == libc::EINTR`), never on error message substrings like `msg.contains("Interrupted")`.

3. **Shutdown/cleanup paths must not silently discard data.** Never `let _ = tx.send(batch)` during shutdown. Log a warning with the number of dropped events, or propagate the error. Changelog events represent filesystem truth — silent loss is a correctness bug.

4. **Flush buffered state before reconnecting after an error.** When reopening a changelog stream after a recv error, flush the batcher first. Otherwise the replay from the reopened stream produces duplicates for structural events (Create, Rename, Unlink) that IGNORE_MASK doesn't cover.

5. **Dedup cancel rules must check for intervening events.** IGNORE_CANCEL (Create+Unlink cancellation) must verify no Rename or Hardlink sits between the pair in the per-FID queue. `retain` should remove exactly one partner, not all matches. Check `last_link` on Unlink before cancelling.

6. **Run `rust-reviewer` agent before committing.** The Phase 1b review caught 2 CRITICAL + 5 HIGH issues that 49 passing unit tests did not surface. Gate: no CRITICAL or HIGH findings from review before commit.

## Design Memory

Detailed architectural decisions are persisted in `.claude/projects/-root-rust-robinhood-rs/memory/`. Key files:
- `robinhood_c_changelog_reader_semantics.md` — watermark tracking, dedup rules, rename stitching from robinhood-C
- `rust_design_style.md` — Rust idiom rules (enum-of-variants not discriminant+union, no stringly-typed params, etc.)
- `keep_discard_from_robinhood.md` — which C patterns to preserve vs discard
- `phase_verification_rule.md` — every phase must pass build+unit+integration+smoke gates
- `pre_phase_design_rule.md` — every phase starts with a design pass; never guess at unclear points
