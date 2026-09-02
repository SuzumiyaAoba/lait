# lait 利用ガイド（日本語）

`lait` の機能ごとの詳細ドキュメントです。まずは [はじめに](./getting-started.md) から読むことをおすすめします。

- [はじめに](./getting-started.md) — 必要な環境、LM Studio の準備、CLI 引数と環境変数、標準入力からのプロンプト（パイプ対応）、`--system`/`--show-usage`/`-o`/`--quiet`、`lait models`/`lait init`/`lait completions`/`lait man`、ビルドと実行、認証あり／なし
- [設定ファイル](./config.md) — `lait.config.yml`、モデル定義と alias、設定値の優先順位、`${VAR_NAME}` による環境変数参照、`.env` の自動読み込み、`workflows:`/`agents:`/`skills:` の登録と `lait workflow list`/`lait agent list`/`lait skill list`
- [ワークフロー（workflow.yml）](./workflow.md) — `nodes:`（何をするか）と `steps:`（どう繋ぐか）の分離とノードの再利用、ワークフロー内でのモデル定義、JSON 出力と `jq` による加工、ファイルへの出力（`write_file`）、ファイル・画像の添付（`files`/`images`）、任意コマンドの実行（`command`）、ステップ間の値の受け渡し（`{{ steps.<id> }}`/`$steps`）、追加パラメータの受け渡し（`lait run --var`/`{{ vars.<key> }}`/`$vars`）、エラー処理（`retry`/`timeout`/`on_error`、ワークフロー全体の既定値）、条件分岐（`when`/`switch`）、並列実行（`parallel`）、条件ループ（`loop`）、配列反復（`for_each`）、早期終了（`stop`/`break`）、サブワークフロー呼び出し（`workflow`）、対話的ユーザー入力（`type: ask`）、実行計画の表示（`lait run --dry-run`）、グラフ出力（`lait graph`）
- [エージェント Markdown ファイル（agent.md）](./agent.md) — frontmatter によるエージェント定義、ワークフローからの利用
- [ワークフロー／エージェントファイルの静的チェック（lint）](./lint.md) — `lait lint`、未使用ノードや jq/テンプレート構文エラー、`mcp`/`skills`/`agent`/`workflow` 参照や `schema_name` の事前検出
- [MCP サーバーのツールを使う](./mcp.md) — `mcp_servers:` の登録、チャット／agent／workflow での `mcp:` 指定、ツール名の修飾、`structured_output`/`--stream` との関係、`max_tool_rounds`
- [スキルを使う](./skills.md) — `skills:` の登録、agent／workflow での `skills:` 指定、システムプロンプトへの追記のされ方、`mcp`/`--stream`/`structured_output` との違い
- [サブエージェントを使う](./subagents.md) — `agents:` の登録、チャット／agent／workflow での `subagents:` 指定、ツール名とツール引数の扱い、サブエージェントの入れ子、`agent:`/`workflow:` ノードとの違い
- [出力例](./output.md) — 標準出力、`--stream`、`--json-schema`、`--json`、`--render`
- [ファイル・画像の添付](./attachments.md) — `--file` によるファイル内容の添付、`--image` による vision モデル向け画像入力
- [会話セッションと対話モード（lait chat）](./chat.md) — `lait chat` REPL、`--session`/`lait sessions` による会話の保存と再開
- [名前付きプロンプトテンプレート（prompts）](./prompts.md) — `prompts:` の登録、`-p`/`lait prompt`、`--var`
- [実行履歴（lait history）](./history.md) — `lait history`/`show`/`search`、`--no-history`/`default.history`
- [トラブルシュート](./troubleshooting.md) — よくあるエラーと対処法
- [開発](./development.md) — テスト、フォーマット、Lint、ビルド
