# Repository Guidelines

## Project Structure

This repository is primarily a Rust 2024 CLI (Rust 1.88, stable). `src/main.rs` is the module root; feature modules live in `src/`, with workflow parsing and validation under `src/workflow/`. Rust integration tests are in `tests/`, and `tests/support/` provides temporary-file fixtures and mock OpenAI-compatible servers. Japanese user documentation is in `docs/usage/ja/` — this is the single source of truth; edit it, never the generated copy. The `website/` directory is an Astro/Starlight TypeScript site whose doc pages are generated from `docs/usage/ja/` by `website/scripts/sync-docs.mjs` (run `pnpm sync-docs` after editing docs, or just `pnpm build`/`pnpm dev`, which run it automatically); follow its existing `website/AGENTS.md` instructions for any other work there. Configuration and development metadata are in `Cargo.toml`, `Makefile.toml`, `rust-toolchain.toml`, `lait.config.yml`, and `scripts/`.

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

## Coding Style and Naming

Use standard `rustfmt` formatting (four-space indentation) and keep Clippy warning-free. Rust modules, functions, variables, and test names use `snake_case`; types and enums use `PascalCase`; constants use `UPPER_SNAKE_CASE`. Keep TypeScript/TSX consistent with neighboring files and run the site type check after site changes.

## Testing Guidelines

Unit tests are colocated in `#[cfg(test)]` modules; behavior-level coverage belongs in `tests/*.rs`. Name tests descriptively, such as `rejects_invalid_schema`. Prefer `tests/support` mock servers and temporary fixtures over real network calls or shared files. No explicit coverage threshold is configured.

## Commits and Pull Requests

Use the history’s Conventional Commit-style prefixes (`feat:`, `fix:`, `refactor:`, `docs:`, `build:`, `chore:`); use `feat!:` for breaking changes and append PR references like `(#34)` when applicable. PRs should explain purpose and behavior, list verification commands, link issues with `Closes #N`, and include tests and documentation updates for user-visible changes. Screenshots are only needed when they clarify a website/UI change.

## Security and Configuration

Keep API keys in environment variables or an untracked `.env`; the root `.gitignore` already ignores `.env`/`.env.*`, but double-check before committing config files that embed secrets directly. `lait.config.yml`'s top-level `base_url`/`api_key`, `models[].provider.*`, and `mcp_servers[]` (`command`/`args`/`env`/`cwd`/`url`/`headers`) support `${VAR_NAME}` expansion — other fields (prompt templates, `default.system`, workflow `prompt:`/`system_prompt:`) do not. `--no-env`/`--no-config` can disable local loading. Treat `mcp_servers` entries as trusted code: they may launch child processes or connect to remote URLs, and credentials must not be hard-coded.
