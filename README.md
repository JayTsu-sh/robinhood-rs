# robinhood-rs

A Rust rewrite of the [Robinhood Policy Engine](https://github.com/cea-hpc/robinhood) for Lustre
filesystems, with policies managed via a RESTful API backed by a database instead of configuration
files.

## Status

Early development — Phase 0 (workspace skeleton). Not yet functional.

## Build

```sh
cargo build --release
```

Requires `liblustreapi` from the `lustre-client` package at build and runtime (added in Phase 1).

## Run (not yet implemented)

```sh
cargo run --release                  # launches the daemon (Phase 13)
cargo run -p rbh-cli --bin rbh -- …  # CLI (Phase 14)
```

## Architecture

See the memory directory at `.claude/memory/` for design decisions, or the roadmap in
`docs/roadmap.md` (Phase 15).

## License

Apache-2.0. See [LICENSE](LICENSE).
