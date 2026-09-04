# lait

Lightweight AI Tool (lait) は、YAML で定義したハーネス、Agent Loop、Flow を CLI から実行・制御するためのツールです。

`lait` は `async-openai` を使って OpenAI Compatible API に接続し、単体のプロンプトをチャット補完として送信する CLI です。LM Studio のローカルサーバーを利用した動作確認や、LLM API 接続のサンプルとして使えます。

## インストール

### バイナリ（GitHub Releases）

[Releases](https://github.com/SuzumiyaAoba/lait/releases) から各プラットフォーム（macOS arm64/x86_64、Linux gnu/musl、Windows）のアーカイブをダウンロードし、`lait`（Windows は `lait.exe`）をパスの通った場所に置いてください。`v*` タグの push で自動的にビルド・添付されます。

### cargo binstall

```sh
cargo binstall lait
```

（crates.io への publish 後に利用できます。Releases のアーカイブをそのまま利用するため Rust のビルドは不要です。）

### Homebrew

```sh
brew install SuzumiyaAoba/tap/lait
```

（tap の formula はリリース時に自動更新されます。）

### ソースからビルド

```sh
cargo install --git https://github.com/SuzumiyaAoba/lait
```

インストール後、シェル補完と man ページは `lait` 自身で生成できます。

```sh
lait completions zsh > ~/.zfunc/_lait   # bash / zsh / fish / powershell / elvish
lait man --dir ~/.local/share/man/man1  # lait.1, lait-run.1, ...
```

## クイックスタート

- 開発には Rust stable と `rustfmt`/`clippy`（`rust-toolchain.toml` により自動的に指定されます）が必要です。

```sh
cargo run -- --model <MODEL_ID_OR_ALIAS> "プロンプト"
```

`makers`（cargo-make）を使う場合は次のとおりです。

```sh
makers run -- --model <MODEL_ID> "プロンプト"
```

## ドキュメント

詳細な使い方の正本は [日本語利用ガイド](./docs/usage/ja/README.md) です。目的別の入口は次のとおりです。

### まず読む

- [はじめに](./docs/usage/ja/getting-started.md) — インストール、最短手順、主要な CLI オプション。
- [トラブルシュート](./docs/usage/ja/troubleshooting.md) — `lait doctor` による一括診断、接続・認証・モデル・ログの確認。

### 設定・自動化

- [設定ファイル](./docs/usage/ja/config.md) — モデル alias、`.env`、ツールやレジストリの登録。
- [ワークフロー（workflow.yml）](./docs/usage/ja/workflow.md) — 複数ステップの自動化、分岐、ループ、並列実行。
- [エージェント Markdown ファイル（agent.md）](./docs/usage/ja/agent.md) — agent の定義と実行。
- [JSON Schema でエディタ補完（lait schema）](./docs/usage/ja/schema.md) — workflow/config/agent の JSON Schema と yaml-language-server 連携。

### ツール・運用

- [MCP サーバーのツールを使う](./docs/usage/ja/mcp.md) — 外部ツールをモデルから呼び出す方法。
- [カスタムシェルツールを使う](./docs/usage/ja/tools.md) — ローカルコマンドをツールとして公開する方法。
- [出力例](./docs/usage/ja/output.md) — 通常出力、ストリーミング、JSON、Markdown 表示。

全ページの一覧は [日本語利用ガイドの目次](./docs/usage/ja/README.md) を参照してください。

## ライセンス

[MIT](./LICENSE)
