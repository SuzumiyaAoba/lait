# 開発

[ドキュメント目次に戻る](./README.md)

## 前提

Rust stable を使用します。`Cargo.toml` の `rust-version` は 1.88 で、
`rust-toolchain.toml` により `rustfmt` と `clippy` も自動で有効になります。

## Rust の検証

リポジトリのルートで、次のコマンドを実行します。

```sh
cargo check --locked
cargo test --locked
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

`makers`（cargo-make）を使う場合は、対応するタスクを実行できます。

```sh
makers check
makers test
makers fmt-check
makers clippy
makers build
```

macOS arm64 で `ld: library not found for -liconv` が発生する場合は、`makers` の cargo
ラッパーが Apple Clang と Xcode SDK を設定して対応します。

## ドキュメントサイト

日本語ドキュメントの正本は `docs/usage/ja/` です。文書を変更したら、サイト側の生成コピーを
同期してから型チェックを実行します。

```sh
cd website
pnpm sync-docs
pnpm types:check
```

`pnpm build` は同期と Astro のチェック・ビルドをまとめて実行します。

GitHub Actions でも Rust の検証と `pnpm build` を実行します。
