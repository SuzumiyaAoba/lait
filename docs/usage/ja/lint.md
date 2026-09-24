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

## ディレクトリを渡す

引数にディレクトリを渡すと、その配下を再帰的に探索して `.yml`/`.yaml` ファイルと、
`---` フロントマターで始まる `.md` ファイル（agent ファイル）をまとめてチェックします
（フロントマターのない通常の Markdown、たとえば README は自動的にスキップされます）。
`.git` などのドット始まりのエントリ（ファイル・ディレクトリとも）、`target/`、
`node_modules/` は探索対象から除外されます（引数に明示的に渡したファイルはこの限りでは
ありません。ドット始まりでも対象になります）。探索時は通常ファイルだけを対象にし、
シンボリックリンクをたどりません。探索結果はパス順に並びますが、同じディレクトリを
複数の引数から指定した場合のみ1回にまとめられます — `lait lint a.yml a.yml` のように
同じファイルを直接複数回指定した場合は、その回数だけチェックされます（[`lait test`](./testing.md)
のファイル単位の重複排除とは異なります）。

```sh
lait lint .
lait lint workflow.yml agents/
```

## `--format text|json|github`

既定は `text`（上記の人が読む形式）です。CI やエディタ連携向けに次の2つを選べます。

| `--format` の値 | 出力 |
| --- | --- |
| `json` | `file`/`line`/`severity`/`message` を持つ構造化レコードの配列を標準出力に出力します。`line` は、YAML 自体のパースエラーであれば実際の行番号、それ以外の指摘（未使用のスキーマや不明な参照など）ではメッセージ中の最初のクォート識別子をファイル内で検索したベストエフォートの行番号です。特定できない場合は `null` になります（`lait.config.yml` 由来の指摘は常に `null` です）。 |
| `github` | GitHub Actions のアノテーション形式 `::error file=<path>,line=<n>::<message>` / `::warning file=<path>,line=<n>::<message>` を1行ずつ出力します。行番号が特定できない場合は `line=` 部分を省略します（ファイル単位の注釈として扱われます）。CI のジョブ内でこの出力をそのまま流すと、該当ファイル・行に PR 上で直接エラーが表示されます。 |

いずれの形式でも終了コードの扱いは変わりません（1件でもエラーがあれば非 0）。

```sh
$ lait lint . --format json
[
  {
    "file": "workflow.yml",
    "line": 12,
    "severity": "warning",
    "message": "schema 'unused-schema' is defined in 'schemas:' but never referenced"
  }
]

$ lait lint . --format github
::warning file=workflow.yml,line=12::schema 'unused-schema' is defined in 'schemas:' but never referenced
```

## チェック内容

`lait run`/`lait agent run` がファイルを読み込む際に必ず行うチェック（ステップの形と
フィールドの組み合わせ、`id` の重複、`schemas:` の名前参照、jq 式・テンプレートの構文、
`retry`/`timeout`/`max_tool_rounds` の範囲、`break`/`stop`/`ask`/`write` を置ける場所など）に
加えて、`lint` は実際にそのステップが実行されるまで顕在化しない問題を検出します。

- テンプレート（`{{ inputs.x }}`）や jq 式（`$inputs.x`）が `inputs:` で宣言されていない入力を
  参照していないか（エラー。jq では `null` になり、テンプレートでは実行時エラーになります）
- テンプレート（`{{ steps.x }}`）や jq 式（`$steps.x`/`$steps["x"]`）が、そのファイルに存在
  しないステップ `id` を参照していないか（警告）
- `schemas:` に定義されているがどこからも参照されていないスキーマ、`agents:` に定義されて
  いるがどの `agent:` ステップからも使われていないエージェント（警告）
- `schemas:`・ステップ・エージェントの `{file: ...}` スキーマが存在し、JSON として読み込めるか
- `inputs:` やスキーマに JSON Schema として認識できない `type`（`type: sting` など）が
  書かれていないか（警告。認識できない型は検証されません）
- `mcp:`（ステップ・エージェント定義・`default.mcp`）に書いた名前が `lait.config.yml` の
  `mcp_servers:` に定義されているか
- `mcp:` が参照しているサーバーの `allowed_tools`（[MCP サーバーのツールを使う](./mcp.md#呼び出せるツールを制限するallowed_tools)を参照）が空リストになっていないか（警告。空リストの
  サーバーへのツール呼び出しは実行時に必ず拒否されます）
- `skills:`/`subagents:`/`tools:`（ステップ・エージェント定義・`default:`）に書いた名前が
  `lait.config.yml` の `skills:`/`agents:`/`tools:` に定義されているか
- `agent:` に指定したエージェントファイルが存在し読み込めるか、または名前が `agents:`
  （ワークフロー内、または `lait.config.yml`）に定義されているか。エージェントファイルの中身
  （システムプロンプトの構文・スキーマ・ツール名）もチェックします
- `workflow:` に指定した子ワークフローが存在し読み込めるか（パスはワークフローファイル自身の
  ディレクトリ基準、名前は `lait.config.yml` の `workflows:`）。子ワークフローも再帰的に
  チェックされ、循環参照も検出します。子ワークフローで見つかった問題には
  `in workflow '<パス>':` が前置されます

`lait.config.yml` が見つからない場合（カレントディレクトリに存在しない、または `--no-config`
を指定した場合）は、`mcp:`/`skills:`/`subagents:`/`tools:` と、登録名で指定した `agent:`/
`workflow:` のチェックだけをスキップし、その旨を警告
として1行だけ表示します（存在しない設定ファイルを前提に、書かれている名前すべてを「未定義」と
してエラーにすることはしません）。

## 出力例

```
$ lait lint workflow.yml agents/city-fact.md
workflow.yml:
  error: step 'summary' references input 'lang', which is not declared in 'inputs:'
  warning: schema 'unused-schema' is defined in 'schemas:' but never referenced
agents/city-fact.md: OK
lait: 1 of 2 file(s) had errors
```

| 表示 | 意味 |
| --- | --- |
| `error:` | `run`/`agent run` すれば必ず失敗する箇所です。 |
| `warning:` | 構文としては問題なく実行できるものの、書き手の意図と異なる可能性が高い箇所です（終了コードには影響しません）。 |

問題が見つからなかったファイルは `<FILE>: OK` とだけ表示されます。

関連: [ワークフロー（workflow.yml）](./workflow.md)、
[エージェント Markdown ファイル（agent.md）](./agent.md)、
[MCP サーバーのツールを使う](./mcp.md)、[スキルを使う](./skills.md)、
[サブエージェントを使う](./subagents.md)
