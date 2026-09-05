# lait 利用ガイド（日本語）

`lait` を初めて使う人は [はじめに](./getting-started.md) から、設定や自動化を始める人は目的に合うページから読んでください。

## まず読む

- [はじめに](./getting-started.md) — インストール、最短手順、接続先の準備、主要な CLI オプション。

## 設定

- [設定ファイル](./config.md) — `lait.config.yml`、モデル alias、優先順位、`.env`、各種レジストリの登録。
- [名前付きプロンプトテンプレート（prompts）](./prompts.md) — 繰り返し使うプロンプトと `--var` の定義・実行。
- [JSON Schema でエディタ補完（lait schema）](./schema.md) — `workflow.yml`/`lait.config.yml`/agent frontmatter の JSON Schema と yaml-language-server 連携。

## ワークフロー

- [ワークフロー（workflow.yml）](./workflow.md) — ノードとステップ、分岐・ループ・並列実行、JSON/jq、チェックポイント。
- [エージェント Markdown ファイル（agent.md）](./agent.md) — frontmatter とシステムプロンプトからエージェントを定義する方法。
- [ワークフロー／エージェントファイルの静的チェック（lint）](./lint.md) — `lait lint` で構文・参照・テンプレートを実行前に検査する方法。

## ツール・拡張

- [MCP サーバーのツールを使う](./mcp.md) — MCP サーバーの登録、ツール制限、チャット・agent・workflow からの利用。
- [スキルを使う](./skills.md) — Markdown スキルの登録と、agent／workflow のシステムプロンプトへの追加。
- [サブエージェントを使う](./subagents.md) — agent Markdown をモデルから呼び出せるツールとして公開する方法。
- [カスタムシェルツールを使う](./tools.md) — ローカルコマンドを MCP なしのモデル呼び出しツールとして公開する方法。
- [ファイル・画像の添付](./attachments.md) — `--file` と `--image` で入力にファイルや画像を添付する方法。

## 運用・診断

- [出力例](./output.md) — 通常出力、ストリーミング、JSON、Structured Outputs、Markdown 表示。
- [モデル比較（lait compare）](./compare.md) — 同一プロンプトを複数モデルへ並行送信し、応答・所要時間・usage を比較する方法。
- [会話セッションと対話モード（lait chat）](./chat.md) — REPL と `--session` による会話の保存・再開。
- [実行履歴（lait history）](./history.md) — 実行履歴の一覧、表示、検索、無効化。
- [トラブルシュート](./troubleshooting.md) — `lait doctor` による環境・設定・接続の一括診断、接続・認証・モデル・終了コード・詳細ログの確認。

## 開発

- [開発](./development.md) — テスト、フォーマット、Lint、ビルドの手順。
