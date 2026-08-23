# lait

Lightweight AI Tool (lait) は、YAML で定義したハーネス、Agent Loop、Flow を CLI から実行・制御するためのツールです。

## 必要な環境

- Rust stable
- `rustfmt` と `clippy`（`rust-toolchain.toml` により自動的に指定されます）

## 使い方

リポジトリのルートで次のコマンドを実行します。

```sh
cargo run
# Hello, World!
```

CLI のヘルプとバージョンは以下で確認できます。

```sh
cargo run -- --help
cargo run -- --version
```

## 開発

テスト、フォーマット、Lint、リリースビルドは次のコマンドで実行できます。

```sh
cargo test --locked
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo build --release --locked
```

GitHub Actions でも同じチェックを実行します。
