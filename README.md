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

詳細な使い方は [docs/usage/ja](./docs/usage/ja/README.md) にまとまっています。

- [はじめに](./docs/usage/ja/getting-started.md) — 必要な環境、LM Studio の準備、CLI 引数と環境変数、ビルドと実行、認証あり／なし
- [設定ファイル](./docs/usage/ja/config.md) — `lait.config.yml`、モデル定義と alias、設定値の優先順位、`${VAR_NAME}` による環境変数参照
- [ワークフロー（workflow.yml）](./docs/usage/ja/workflow.md) — `nodes:`（何をするか）と `steps:`（どう繋ぐか）の分離とノードの再利用、ワークフロー内でのモデル定義、JSON 出力と `jq` による加工、ファイルへの出力（`write_file`）、ステップ間の値の受け渡し（`{{ steps.<id> }}`/`$steps`）、エラー処理（`retry`/`timeout`/`on_error`、ワークフロー全体の既定値）、条件分岐（`when`/`switch`）、並列実行（`parallel`）、条件ループ（`loop`）、配列反復（`for_each`）、早期終了（`stop`/`break`）、サブワークフロー呼び出し（`workflow`）
- [エージェント Markdown ファイル（agent.md）](./docs/usage/ja/agent.md) — frontmatter によるエージェント定義、ワークフローからの利用
- [ワークフロー／エージェントファイルの静的チェック（lint）](./docs/usage/ja/lint.md) — `lait lint`、未使用ノードや jq/テンプレート構文エラー、`mcp`/`skills`/`agent`/`workflow` 参照や `schema_name` の事前検出
- [MCP サーバーのツールを使う](./docs/usage/ja/mcp.md) — `mcp_servers:` の登録、チャット／agent／workflow での `mcp:` 指定、ツール名の修飾、`structured_output`/`--stream` との関係、`max_tool_rounds`
- [スキルを使う](./docs/usage/ja/skills.md) — `skills:` の登録、agent／workflow での `skills:` 指定、システムプロンプトへの追記のされ方
- [サブエージェントを使う](./docs/usage/ja/subagents.md) — `agents:` の登録、チャット／agent／workflow での `subagents:` 指定、モデル自身によるサブエージェント呼び出し（tool loop）、サブエージェントの入れ子
- [出力例](./docs/usage/ja/output.md) — 標準出力、`--stream`、`--json-schema`、`--json`
- [トラブルシュート](./docs/usage/ja/troubleshooting.md) — よくあるエラーと対処法
- [開発](./docs/usage/ja/development.md) — テスト、フォーマット、Lint、ビルド
