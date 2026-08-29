# ワークフロー／エージェントファイルの静的チェック（lint）

[ドキュメント目次に戻る](./README.md)

`lait lint <FILE>...` サブコマンドで、`workflow.yml`（`.yml`/`.yaml`）とエージェント Markdown
ファイル（`.md`）をモデルに実際にリクエストを送らずに静的チェックできます。CI やコミット前の
確認に使うことを想定しています（Rust ソース自体の `cargo clippy` とは別物です。そちらは
[開発](./development.md) を参照してください）。

```sh
cargo run -- lint workflow.yml agents/city-fact.md
```

引数には拡張子が異なる複数のファイルをまとめて渡せます。あるファイルにエラーがあっても、
残りのファイルは最後までチェックされます。1件でもエラーがあれば終了コードは `0` 以外になります。

## チェック内容

`lait run`/`lait agent run` がファイルを読み込む際に必ず行う構造チェック（frontmatter・
`nodes:`/`steps:` の形、`switch`/`parallel`/`loop`/`for_each` の組み合わせ制約、
`retry`/`timeout`/`max_tool_rounds` の範囲など）に加えて、`lint` はそれ単体では検出できない、
実際にそのステップ／ノードが実行されるまで顕在化しないエラーを検出します。

- `nodes:` に定義されているが、どの `steps:`（`switch`/`parallel`/`loop`/`for_each` の中も含む）
  からも `use:` されていないノード（警告）
- `when` / `switch` の `case.when` / `loop` の `while`/`until` / `for_each` の `items` /
  `parallel`・`for_each` の `join` に書いた jq フィルタの構文エラー
- `prompt:`（ノード）およびエージェントファイルのシステムプロンプトテンプレートの handlebars
  構文エラー
- `input_schema`/`output_schema` に指定した名前・パスが解決できるか（`json_schemas:` のキー、
  または実在するファイルパスであるか）
- `schema_name`（省略時は `structured_output`）が Structured Outputs のスキーマ名として妥当か
  （1〜64文字、ASCII の英数字・`_`・`-` のみ）
- `mcp:`（ノード・エージェントファイル・`default.mcp`）に書いた名前が `lait.config.yml` の
  `mcp_servers:` に定義されているか
- `skills:`（ノード・エージェントファイル・`default.skills`）に書いた名前が `lait.config.yml` の
  `skills:` に定義されているか
- `subagents:`（ノード・エージェントファイル・`default.subagents`）に書いた名前が
  `lait.config.yml` の `agents:` に定義されているか
- `agent:` に指定したエージェントファイルが存在し、読み込めるか（`agent:` は `lait run` と同じく
  カレントディレクトリからの相対パスとして解決されます。詳細は
  [エージェント Markdown ファイル（agent.md）](./agent.md) を参照）
- `workflow:` に指定したサブワークフローファイルが存在し、読み込めるか（`workflow.yml` 自身の
  ディレクトリからの相対パスとして解決されます）。サブワークフローも再帰的にチェックされ、
  循環参照（`workflow:` が巡り巡って自分自身を呼ぶ）も検出します

`lait.config.yml` が見つからない場合（カレントディレクトリに存在しない、または `--no-config`
を指定した場合）は、`mcp:`/`skills:`/`subagents:` の名前チェックだけをスキップし、その旨を警告
として1行だけ表示します（存在しない設定ファイルを前提に、書かれている名前すべてを「未定義」と
してエラーにすることはしません）。

## 出力例

```
$ lait lint workflow.yml agents/city-fact.md
workflow.yml:
  error: node 'extract': 'jq' has an invalid jq filter ".foo[": failed to parse jq filter ...
  warning: node 'unused-step' is defined in 'nodes:' but never referenced by a step's 'use'
agents/city-fact.md: OK
lait: 1 of 2 file(s) had errors
```

- `error:` は `run`/`agent run` すれば必ず失敗する箇所です。
- `warning:` は構文としては問題なく実行できるものの、書き手の意図と異なる可能性が高い箇所です
  （終了コードには影響しません）。
- 問題が見つからなかったファイルは `<FILE>: OK` とだけ表示されます。

関連: [ワークフロー（workflow.yml）](./workflow.md)、
[エージェント Markdown ファイル（agent.md）](./agent.md)、
[MCP サーバーのツールを使う](./mcp.md)、[スキルを使う](./skills.md)、
[サブエージェントを使う](./subagents.md)
