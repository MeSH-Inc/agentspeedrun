# agentspeedrun

Rust workspace: `speedrun-core` (domain, no I/O), `speedrund` (daemon), `speedrun-cli` (`speedrun` binary). Read `docs/ARCHITECTURE.md` before changing event semantics.

- Before committing: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`.
- After changing anything in `speedrun-core/src/api.rs` or `event.rs`, regenerate the schema with `cargo run -q --bin speedrun -- schema > schema/protocol.schema.json`.
- Events are immutable once written. Changing an existing payload's meaning needs a `SCHEMA_VERSION` bump, and `RunView::fold` must keep reading old events.
- Hooks (`speedrun hook …`) and `speedrun ingest` must always exit 0 and stay fast.
- Host adapters (`plugins/opencode`, `plugins/pi`) stay thin: logic belongs in the CLI. After changing one, run `cargo build && (cd plugins/<host> && npx tsc --noEmit && bun test)`. The binary embeds them for `speedrun setup`, so rebuild before testing setup.
- Built-in categories are versioned. Change an existing category by adding a new version, never by editing a published one.
