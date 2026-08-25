# lait

Lightweight AI Tool (lait) は、YAML で定義したハーネス、Agent Loop、Flow を CLI から実行・制御するためのツールです。

`lait` は `async-openai` を使って OpenAI Compatible API に接続し、単体のプロンプトをチャット補完として送信する CLI です。LM Studio のローカルサーバーを利用した動作確認や、LLM API 接続のサンプルとして使えます。

## クイックスタート

- Rust stable と `rustfmt`/`clippy`（`rust-toolchain.toml` により自動的に指定されます）が必要です。

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
- [設定ファイル](./docs/usage/ja/config.md) — `lait.config.yml`、モデル定義と alias、設定値の優先順位
- [ワークフロー（workflow.yml）](./docs/usage/ja/workflow.md) — step の逐次実行、ワークフロー内でのモデル定義、JSON 出力と `jq` による加工、ステップ間の値の受け渡し（`{{ steps.<id> }}`/`$steps`）、エラー処理（`retry`/`timeout`/`on_error`）、条件分岐（`when`/`switch`）、並列実行（`parallel`）、条件ループ（`loop`）、配列反復（`for_each`）、早期終了（`stop`/`break`）
- [エージェント Markdown ファイル（agent.md）](./docs/usage/ja/agent.md) — frontmatter によるエージェント定義、ワークフローからの利用
- [出力例](./docs/usage/ja/output.md) — 標準出力、`--json-schema`、`--json`
- [トラブルシュート](./docs/usage/ja/troubleshooting.md) — よくあるエラーと対処法
- [開発](./docs/usage/ja/development.md) — テスト、フォーマット、Lint、ビルド
