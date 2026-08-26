# lait 利用ガイド（日本語）

`lait` の機能ごとの詳細ドキュメントです。まずは [はじめに](./getting-started.md) から読むことをおすすめします。

- [はじめに](./getting-started.md) — 必要な環境、LM Studio の準備、CLI 引数と環境変数、ビルドと実行、認証あり／なし
- [設定ファイル](./config.md) — `lait.config.yml`、モデル定義と alias、設定値の優先順位、`${VAR_NAME}` による環境変数参照
- [ワークフロー（workflow.yml）](./workflow.md) — step の逐次実行、ワークフロー内でのモデル定義、JSON 出力と `jq` による加工、ファイルへの出力（`write_file`）、エラー処理（`retry`/`timeout`/`on_error`、ワークフロー全体の既定値）、条件分岐（`when`/`switch`）、並列実行（`parallel`）、条件ループ（`loop`）、配列反復（`for_each`）
- [エージェント Markdown ファイル（agent.md）](./agent.md) — frontmatter によるエージェント定義、ワークフローからの利用
- [出力例](./output.md) — 標準出力、`--json-schema`、`--json`
- [トラブルシュート](./troubleshooting.md) — よくあるエラーと対処法
- [開発](./development.md) — テスト、フォーマット、Lint、ビルド
