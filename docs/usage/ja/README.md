# lait 利用ガイド（日本語）

`lait` を初めて使う人は [はじめに](./getting-started.md) から、設定や自動化を始める人は目的に合うページから読んでください。

## まず読む

- [はじめに](./getting-started.md) — インストール、最短手順、接続先の準備、主要な CLI オプション。

## 設定

- [設定ファイル](./config.md) — `lait.config.yml`、モデル alias、優先順位、`.env`、各種レジストリの登録。
- [名前付きプロンプトテンプレート（prompts）](./prompts.md) — 繰り返し使うプロンプトと `--var` の定義・実行。
- [JSON Schema でエディタ補完（lait schema）](./schema.md) — `workflow.yml`/`lait.config.yml`/`lait.deps.yml`/agent frontmatter の JSON Schema と yaml-language-server 連携。
- [GitHub 上のファイルを依存として取り込む（lait deps）](./deps.md) — GitHub リポジトリのワークフロー/エージェント/スキルを `lait.deps.yml` と `lait.lock` で管理し、名前で参照する方法。

## ワークフロー

- [ワークフロー（workflow.yml）](./workflow.md) — ステップ・入力と出力・型付きの値、分岐・ループ・並列実行、チェックポイント、v1 からの移行。
- [エージェント Markdown ファイル（agent.md）](./agent.md) — frontmatter とシステムプロンプトからエージェントを定義する方法。
- [ワークフロー／エージェントファイルの静的チェック（lint）](./lint.md) — `lait lint` で構文・参照・テンプレートを実行前に検査する方法。
- [決定的テスト（record & replay / lait test）](./testing.md) — `lait run --record`/`--replay` と `lait test` で、API を呼ばずに制御フローを検証する方法。
- [出力品質の評価（lait eval）](./eval.md) — ワークフロー/モデル+プロンプトを実際に実行し、contains/jq/llm_judge のアサーションで出力品質を評価する方法。

## ツール・拡張

- [MCP サーバーのツールを使う](./mcp.md) — MCP サーバーの登録、ツール制限、チャット・agent・workflow からの利用。
- [ツール周回の要約による圧縮（default.compaction）](./compaction.md) — 長い tool loop の履歴をモデル自身の要約で圧縮する方法。
- [スキルを使う](./skills.md) — Markdown スキルの登録と、agent／workflow のシステムプロンプトへの追加。
- [サブエージェントを使う](./subagents.md) — agent Markdown をモデルから呼び出せるツールとして公開する方法。
- [カスタムシェルツールを使う](./tools.md) — ローカルコマンドを MCP なしのモデル呼び出しツールとして公開する方法。
- [Jev 互換 API で判定する（decide / lait decide）](./jev.md) — TypeSafe Jev 互換の判定 API に yes/no・選択・スコアの質問を送り、確率つきの答えをワークフローの分岐などに使う方法。
- [ワークフロー/エージェントを MCP サーバーとして公開する（lait serve --mcp）](./serve.md) — `agents:`/`workflows:` の各エントリを MCP ツールとして他のクライアントに公開する方法。
- [ファイル・画像の添付](./attachments.md) — `--file` と `--image` で入力にファイルや画像を添付する方法。

## 運用・診断

- [実行トレース（lait run --trace-file / lait trace show / lait trace export）](./trace.md) — モデル呼び出し・ツール呼び出しを JSONL として記録し、実行軌跡を検査・集計したり OTLP へエクスポートしたりする方法。
- [出力例](./output.md) — 通常出力、ストリーミング、JSON、Structured Outputs、Markdown 表示。
- [モデル比較（lait compare）](./compare.md) — 同一プロンプトを複数モデルへ並行送信し、応答・所要時間・usage を比較する方法。
- [会話セッションと対話モード（lait chat）](./chat.md) — REPL と `--session` による会話の保存・再開。
- [実行履歴（lait history）](./history.md) — 実行履歴の一覧、表示、検索、無効化。
- [トラブルシュート](./troubleshooting.md) — `lait doctor` による環境・設定・接続の一括診断、接続・認証・モデル・終了コード・詳細ログの確認。

## 開発

- [開発](./development.md) — テスト、フォーマット、Lint、ビルドの手順。
