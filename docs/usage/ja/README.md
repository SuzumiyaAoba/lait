# lait 利用ガイド（日本語）

`lait` の機能ごとの詳細ドキュメントです。まずは [はじめに](./getting-started.md) から読むことをおすすめします。

- [はじめに](./getting-started.md) — 必要な環境、LM Studio の準備、CLI 引数と環境変数、ビルドと実行、認証あり／なし
- [設定ファイル](./config.md) — `lait.config.yml`、モデル定義と alias、設定値の優先順位、`${VAR_NAME}` による環境変数参照
- [ワークフロー（workflow.yml）](./workflow.md) — `nodes:`（何をするか）と `steps:`（どう繋ぐか）の分離とノードの再利用、ワークフロー内でのモデル定義、JSON 出力と `jq` による加工、ファイルへの出力（`write_file`）、ステップ間の値の受け渡し（`{{ steps.<id> }}`/`$steps`）、エラー処理（`retry`/`timeout`/`on_error`、ワークフロー全体の既定値）、条件分岐（`when`/`switch`）、並列実行（`parallel`）、条件ループ（`loop`）、配列反復（`for_each`）、早期終了（`stop`/`break`）、サブワークフロー呼び出し（`workflow`）
- [エージェント Markdown ファイル（agent.md）](./agent.md) — frontmatter によるエージェント定義、ワークフローからの利用
- [MCP サーバーのツールを使う](./mcp.md) — `mcp_servers:` の登録、チャット／agent／workflow での `mcp:` 指定、ツール名の修飾、`structured_output`/`--stream` との関係、`max_tool_rounds`
- [スキルを使う](./skills.md) — `skills:` の登録、agent／workflow での `skills:` 指定、システムプロンプトへの追記のされ方、`mcp`/`--stream`/`structured_output` との違い
- [出力例](./output.md) — 標準出力、`--stream`、`--json-schema`、`--json`
- [トラブルシュート](./troubleshooting.md) — よくあるエラーと対処法
- [開発](./development.md) — テスト、フォーマット、Lint、ビルド
