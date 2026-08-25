# lait 利用ガイド（日本語）

`lait` の機能ごとの詳細ドキュメントです。まずは [はじめに](./getting-started.md) から読むことをおすすめします。

- [はじめに](./getting-started.md) — 必要な環境、LM Studio の準備、CLI 引数と環境変数、ビルドと実行、認証あり／なし
- [設定ファイル](./config.md) — `lait.config.yml`、モデル定義と alias、設定値の優先順位
- [ワークフロー（run.yml）](./workflow.md) — step の逐次実行、ワークフロー内でのモデル定義、JSON 出力と `jq` による加工、条件分岐（`when`/`switch`）、並列実行（`parallel`）
- [エージェント Markdown ファイル（agent.md）](./agent.md) — frontmatter によるエージェント定義、ワークフローからの利用
- [出力例](./output.md) — 標準出力、`--json-schema`、`--json`
- [トラブルシュート](./troubleshooting.md) — よくあるエラーと対処法
- [開発](./development.md) — テスト、フォーマット、Lint、ビルド
