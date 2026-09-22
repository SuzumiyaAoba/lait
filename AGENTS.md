# Repository Guidelines

## Project Structure

This repository is primarily a Rust 2024 CLI (Rust 1.88, stable). `src/main.rs` is the module root; feature modules live in `src/`, with workflow parsing and validation under `src/workflow/`. Rust integration tests are in `tests/`, and `tests/support/` provides temporary-file fixtures and mock OpenAI-compatible servers. Japanese user documentation is in `docs/usage/ja/` — this is the single source of truth; edit it, never the generated copy. The `website/` directory is a Blume TypeScript site whose doc pages are generated from `docs/usage/ja/` by `website/scripts/sync-docs.mjs` (run `pnpm sync-docs` after editing docs, or just `pnpm build`/`pnpm dev`, which run it automatically); follow its existing `website/AGENTS.md` instructions for any other work there. Configuration and development metadata are in `Cargo.toml`, `Makefile.toml`, `rust-toolchain.toml`, `lait.config.yml`, and `scripts/`.

## Build, Test, and Development Commands

Run these from the repository root:

```sh
cargo run -- --help
cargo check --locked
cargo test --locked
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

`makers run|check|test|fmt-check|clippy|build` provides the corresponding cargo-make tasks; its wrapper configures Apple Clang and the macOS SDK when needed. For the documentation site, use `cd website && pnpm dev`, `pnpm build`, `pnpm preview`, or `pnpm types:check`.

A bare `cargo`/`makers` on `PATH` may not resolve to a toolchain with the `clippy` component (for example, a Nix-profile `cargo` outside the flake's dev shell) — that `cargo clippy` fails with "no such command" is a symptom of that, not of clippy being globally unavailable. This repository's `flake.nix` provides a dev shell built from `rust-toolchain.toml`, whose `components = ["rustfmt", "clippy"]` include it: `nix develop --command makers clippy` (or `nix develop`, then `makers clippy` inside the shell) runs it. Prefer that whenever a bare `cargo clippy`/`makers clippy` reports the command missing, before concluding clippy cannot be checked at all — CI's `lint` job always installs an explicit `clippy` component (`dtolnay/rust-toolchain@stable`) independently of this.

## Coding Style and Naming

Use standard `rustfmt` formatting (four-space indentation) and keep Clippy warning-free. Rust modules, functions, variables, and test names use `snake_case`; types and enums use `PascalCase`; constants use `UPPER_SNAKE_CASE`. Keep TypeScript/TSX consistent with neighboring files and run the site type check after site changes.

## Testing Guidelines

Unit tests are colocated in `#[cfg(test)]` modules; behavior-level coverage belongs in `tests/*.rs`. Name tests descriptively, such as `rejects_invalid_schema`. Prefer `tests/support` mock servers and temporary fixtures over real network calls or shared files. No explicit coverage threshold is configured. Once a module's inline `mod tests { ... }` body grows to roughly 400 lines, externalize it into a sibling `<module>/tests.rs` file (`#[cfg(test)] mod tests;` in the parent, mirroring how a `<module>/` directory already groups a module with its submodules) — see `src/async_io/tests.rs`, `src/schema/tests.rs`, `src/cli/tests.rs`, `src/lint/tests.rs`, `src/config/tests.rs`, and `src/jq/tests.rs` for precedent.

## Refactoring Conventions

When splitting a module `<name>.rs` into a `<name>/` directory, decide the shape mechanically: if, after moving code out, the parent file still holds anything besides `mod` declarations and `pub(crate) use` re-exports (a type definition, a function, an `impl` block, a constant), keep `<name>.rs` as a facade alongside `<name>/` (as `src/config.rs`+`src/config/` does); if nothing but `mod`/`use` remains, collapse to `<name>/mod.rs` (as `src/async_io/mod.rs` does — four `mod` declarations plus `use`/`pub(crate) use` re-exports and nothing else). This describes the existing 13-vs-5 split rather than introducing a new one (facade: `app`, `app/doctor`, `cli`, `config`, `engine`, `jq`, `lint`, `mcp`, `process`, `response`, `schema`, `workflow/exec`, `jsonl/unix_relative`; collapsed: `async_io`, `jsonl`, `workflow`, `workflow/model`, `workflow/tests`) — don't reclassify modules that predate this rule. Note that `jsonl/mod.rs` and `workflow/mod.rs` hold functions/structs, not only `mod`/`use` — the shape this rule assigns to the facade form — precisely because they predate it (`workflow/model/mod.rs` similarly holds three `type` aliases beyond its re-exports, for the same reason).

A split's new file should keep external call sites referring to items the same way they did before (`pub(crate) use` re-export), give new items the narrowest visibility that works (`pub(super)` over `pub(crate)`), and carry a `//!` doc comment explaining why the split happened. When a name would collide with an existing module (e.g. a `workflow` submodule holding workflow-specific lint rules, next to the crate's own `workflow` module), suffix it and say why in the doc comment (`lint/workflow_lint.rs` is the precedent). Splitting a type's `impl` block across files without moving the type itself is an established technique (`impl RequestSettings` spans `src/engine.rs` and `src/engine/settings.rs`) — prefer it over introducing a new type solely to relocate methods.

The following are deliberate design choices, not oversights — don't "fix" them without a specific reason tied to new evidence:
- `engine` and `workflow` depend on each other (subagent/workflow nesting is genuinely mutually recursive); this cycle is not a defect.
- `engine/stream.rs` flushes stdout on every streamed chunk — intentional for perceived latency during interactive output; file output already buffers instead.
- `serde_json`'s `preserve_order` feature is relied on for output stability elsewhere in the crate, even though a specific cache key computation doesn't need it.
- `async_io`'s per-operation OS thread (rather than `tokio::spawn_blocking`) exists so a blocking read can be cancelled without leaking a task that outlives its owner; don't route more call sites through it than actually need cancellable FIFO-aware reads.
- `tests/*.rs`'s 39 files plus `tests/workflow/` stay as 40 separate integration test binaries rather than being grouped into fewer themed ones. Measured after a `cargo clean && cargo build --tests --locked --timings` (following the P8-2 `[profile.dev.package."*"] opt-level = 2` change): the 40 `lait`-crate test targets' wall-clock tail (their earliest start to their latest finish, since they compile in parallel once the shared `lait` rlib is ready) was ~15s against a ~197s total build, and their summed per-unit durations were ~8% of the summed duration of every unit in the build — both well under the 40% gate this decision was conditioned on. Caveats for a future re-measurement: `--timings` reports per-unit rustc totals, not an isolated link phase, so this is an estimate, not a measured link-time share; and the ~8% denominator was inflated by `opt-level = 2` raising dependency-compile cost, so a re-measure under a different profile will not reproduce this number and needs its own run.
- `lait lint <DIR>`'s recursive directory walk skips dot-prefixed *files* as well as dot-prefixed directories (`file_walk::DirWalker`, shared with `lait test`). Before P10 this was only true of `lait test`; `lint`'s own walker used to skip only dot-*directories*, so a `.hidden.yml` inside a linted directory was still checked. `b4746af`'s `DirWalker` unification silently widened `lint` to match `test` without a commit-message callout, and P10-4 is what confirmed and documented it as the intended behavior (see `src/lint/targets.rs`'s dot-file test and `docs/usage/ja/lint.md`'s directory-walk paragraph) rather than reverting it — an explicitly named file argument is never affected either way.

Performance changes should be justified with a measurement, not intuition: build a baseline in a separate `git worktree` at the pre-change commit, build both with `cargo build --release --locked`, and compare with `/usr/bin/time -l` (median of at least 3 runs). Keep benchmark fixtures (YAML workflows, mock servers) out of the commit — construct them under `/tmp` for the measurement and discard them.

## Commits and Pull Requests

Use the history’s Conventional Commit-style prefixes (`feat:`, `fix:`, `refactor:`, `docs:`, `build:`, `chore:`); use `feat!:` for breaking changes and append PR references like `(#34)` when applicable. PRs should explain purpose and behavior, list verification commands, link issues with `Closes #N`, and include tests and documentation updates for user-visible changes. Screenshots are only needed when they clarify a website/UI change.

## Security and Configuration

Keep API keys in environment variables or an untracked `.env`; the root `.gitignore` already ignores `.env`/`.env.*`, but double-check before committing config files that embed secrets directly. `lait.config.yml`'s top-level `base_url`/`api_key`, `models[].provider.*`, and `mcp_servers[]` (`command`/`args`/`env`/`cwd`/`url`/`headers`) support `${VAR_NAME}` expansion — other fields (prompt templates, `default.system`, workflow `prompt:`/`system_prompt:`) do not. `--no-env`/`--no-config` can disable local loading. Treat `mcp_servers` entries as trusted code: they may launch child processes or connect to remote URLs, and credentials must not be hard-coded.
