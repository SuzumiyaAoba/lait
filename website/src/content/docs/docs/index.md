---
title: "lait 利用ガイド（日本語）"
description: "lait の使い方ドキュメント一覧。まずは「はじめに」から読むのがおすすめです。"
---

`lait` の機能ごとの詳細ドキュメントです。まずは [はじめに](/lait/docs/getting-started/) から読むことをおすすめします。

- [はじめに](/lait/docs/getting-started/) — 必要な環境、LM Studio の準備、CLI 引数と環境変数、標準入力からのプロンプト（パイプ対応）、`--system`/`--show-usage`/`-o`/`--quiet`、`lait models`/`lait init`/`lait completions`/`lait man`、ビルドと実行、認証あり／なし
- [設定ファイル](/lait/docs/config/) — `lait.config.yml`、モデル定義と alias、設定値の優先順位、`${VAR_NAME}` による環境変数参照、`.env` の自動読み込み
- [ワークフロー（workflow.yml）](/lait/docs/workflow/) — `nodes:`（何をするか）と `steps:`（どう繋ぐか）の分離とノードの再利用、ワークフロー内でのモデル定義、JSON 出力と `jq` による加工、ファイルへの出力（`write_file`）、ファイル・画像の添付（`files`/`images`）、任意コマンドの実行（`command`）、ステップ間の値の受け渡し（`{{ steps.<id> }}`/`$steps`）、エラー処理（`retry`/`timeout`/`on_error`、ワークフロー全体の既定値）、条件分岐（`when`/`switch`）、並列実行（`parallel`）、条件ループ（`loop`）、配列反復（`for_each`）、早期終了（`stop`/`break`）、サブワークフロー呼び出し（`workflow`）
- [エージェント Markdown ファイル（agent.md）](/lait/docs/agent/) — frontmatter によるエージェント定義、ワークフローからの利用
- [ワークフロー／エージェントファイルの静的チェック（lint）](/lait/docs/lint/) — `lait lint`、未使用ノードや jq/テンプレート構文エラー、`mcp`/`skills`/`agent`/`workflow` 参照や `schema_name` の事前検出
- [MCP サーバーのツールを使う](/lait/docs/mcp/) — `mcp_servers:` の登録、チャット／agent／workflow での `mcp:` 指定、ツール名の修飾、`structured_output`/`--stream` との関係、`max_tool_rounds`
- [スキルを使う](/lait/docs/skills/) — `skills:` の登録、agent／workflow での `skills:` 指定、システムプロンプトへの追記のされ方、`mcp`/`--stream`/`structured_output` との違い
- [サブエージェントを使う](/lait/docs/subagents/) — `agents:` の登録、チャット／agent／workflow での `subagents:` 指定、ツール名とツール引数の扱い、サブエージェントの入れ子、`agent:`/`workflow:` ノードとの違い
- [出力例](/lait/docs/output/) — 標準出力、`--stream`、`--json-schema`、`--json`、`--render`
- [ファイル・画像の添付](/lait/docs/attachments/) — `--file` によるファイル内容の添付、`--image` による vision モデル向け画像入力
- [会話セッションと対話モード（lait chat）](/lait/docs/chat/) — `lait chat` REPL、`--session`/`lait sessions` による会話の保存と再開
- [名前付きプロンプトテンプレート（prompts）](/lait/docs/prompts/) — `prompts:` の登録、`-p`/`lait prompt`、`--var`
- [実行履歴（lait history）](/lait/docs/history/) — `lait history`/`show`/`search`、`--no-history`/`default.history`
- [トラブルシュート](/lait/docs/troubleshooting/) — よくあるエラーと対処法
- [開発](/lait/docs/development/) — テスト、フォーマット、Lint、ビルド

