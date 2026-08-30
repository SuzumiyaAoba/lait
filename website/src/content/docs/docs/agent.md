---
title: "エージェント Markdown ファイル（agent.md）"
description: "frontmatter によるエージェント定義、ワークフローからの利用、MCP ツールの利用、スキルの利用、サブエージェントの利用"
---

`lait agent run <FILE> <INPUT>` サブコマンドで、Markdown ファイル1つでエージェントを定義・
実行できます。ファイルは YAML の frontmatter（`---` で区切られたブロック）とそれに続く
Markdown 本文で構成され、本文がシステムプロンプトのテンプレートになります。

```markdown
---
name: city-fact
description: 文章から都市名と人口を抽出する
model: local
reasoning_effort: medium
temperature: 0.7
top_p: 0.9
max_tokens: 512
input_schema:
  schema:
    type: object
    properties:
      text: { type: string }
    required: [text]
output_schema:
  schema:
    type: object
    properties:
      city: { type: string }
      population: { type: integer }
    required: [city, population]
    additionalProperties: false
structured_output: true
schema_name: city_fact
---
次の文章から都市名と人口を JSON で抽出してください。

{{ input.text }}
```

```sh
cargo run -- agent run city-fact.md '{"text":"東京の人口は約1400万人です。"}'
```

- ファイルは1行目が必ず `---` で始まり、次に現れる `---` 行までが frontmatter（YAML）、
  それ以降が本文（システムプロンプトのテンプレート）になります。
- `agent:`（ノード）や `file_path:`（`json_schemas:`/`input_schema:`/`output_schema:`）に書く
  パスは、既存の `--json-schema <FILE>` や `lait.config.yml` の探索と同じく、常にコマンドを
  実行したディレクトリ（カレントディレクトリ）からの相対パスとして解決されます。エージェント
  ファイルや `workflow.yml` 自体の場所からの相対パスではないため、`workflow.yml` を別ディレクトリから
  実行する場合は注意してください。
- `model` / `reasoning_effort` / `temperature` / `top_p` / `max_tokens` は省略可能で、
  `lait.config.yml` の `default:` にフォールバックします。CLI から `--model` 等で上書きすること
  はできません。`temperature`（`0.0`〜`2.0`）・`top_p`（`0.0`〜`1.0`）・`max_tokens`（`1`以上）は
  範囲外の値を指定するとファイルの読み込み時点でエラーになります。
- `input_schema` / `output_schema` は、`json_schemas:` と同じ形式で、スキーマ本体を直接書く
  `schema:` と外部ファイルを指す `file_path:` のどちらか一方を指定します。
- `structured_output: true` を指定する場合は `output_schema` が必須です（逆に `output_schema`
  だけ指定して `structured_output` を省略/false にするのはエラーです）。`schema_name` は
  `structured_output: true` のときだけ使われ、省略時は `structured_output` になります。
- `input_schema` を指定すると、`INPUT` が JSON オブジェクトであること、
  `input_schema.schema.required` に列挙したフィールドがすべて存在すること、さらに
  `properties`/`items` で宣言したフィールドの `type`/`enum` やネストしたオブジェクト・配列の
  中身までを実行前に再帰的に検証します（`format`・`pattern`・数値の範囲・
  `additionalProperties`・`oneOf`/`anyOf`/`allOf`・`$ref` は検証しません）。検証に失敗すると
  モデルを呼び出さずにエラーになります。
- `INPUT` はまず JSON としてパースを試み、成功すればそのオブジェクト/配列/値がシステム
  プロンプトのテンプレートに渡され、失敗すれば文字列としてそのまま渡されます。
- システムプロンプトのテンプレートは handlebars 構文です。`{{ input.city }}` のようにドット
  区切りでフィールドにアクセスできます。オブジェクトや配列全体を JSON テキストとして埋め込む
  には `{{ json input }}` / `{{ json input.field }}` を使います。`{{ input }}` は `INPUT` が
  文字列/数値/真偽値のときだけ使え、オブジェクト/配列のときに `{{ input }}` を書くとエラーに
  なります（handlebars はオブジェクト/配列を既定では `[object]`/`[array]` という文字列に
  してしまうため、それをモデルに送ってしまう前にエラーとして止めています。`{{ json input }}`
  を使ってください）。テンプレート中の未定義の変数を参照した場合もエラーになります。
- レンダリングされた本文は system ロールのメッセージとして送信され、`INPUT`（元の生テキスト）
  は別途 user ロールのメッセージとして送信されます。

## MCP ツールの利用

frontmatter に `mcp:`（`lait.config.yml` の `mcp_servers:` エントリ名のリスト）と、任意で
`max_tool_rounds:`（既定 8）を指定すると、その agent の呼び出しに MCP ツールが渡されます。

```markdown
---
model: local
mcp: [filesystem]
max_tool_rounds: 8
---
{{ input.task }} を実行してください。
```

`mcp:` を省略した場合は `lait.config.yml` の `default.mcp` にフォールバックします。詳しい仕組みは
[MCP サーバーのツールを使う](/lait/docs/mcp/) を参照してください。

## スキルの利用

frontmatter に `skills:`（`lait.config.yml` の `skills:` エントリ名のリスト）を指定すると、
その内容がシステムプロンプトテンプレートのレンダリング結果の末尾に `---` 区切りで追記されます。

```markdown
---
model: local
skills: [code-review]
---
次の差分をレビューしてください。

{{ input.diff }}
```

`skills:` を省略した場合は `lait.config.yml` の `default.skills` にフォールバックします。詳しい
仕組みは [スキルを使う](/lait/docs/skills/) を参照してください。

## サブエージェントの利用

frontmatter に `subagents:`（`lait.config.yml` の `agents:` エントリ名のリスト）を指定すると、
その名前で登録されたエージェント Markdown ファイルが、この agent のモデル自身が呼び出すかどうか
判断できる「サブエージェント」ツールとして渡されます。MCP ツール（`mcp:`）と同じ tool loop の
仕組みに乗るため、モデルがツール呼び出しを返すたびに lait がそのサブエージェントを実行し、結果を
モデルに返す、というやり取りを最終回答が出るまで自動で繰り返します。

```markdown
---
model: local
subagents: [researcher]
---
{{ input.task }} について、必要であれば researcher に調査を任せてください。
```

`subagents:` を省略した場合は `lait.config.yml` の `default.subagents` にフォールバックします。
詳しい仕組みは [サブエージェントを使う](/lait/docs/subagents/) を参照してください。

## ワークフローからエージェントファイルを使う

`workflow.yml` の `nodes:` エントリで `prompt`/`input_schema`/`output_schema`/`schema_name` の
代わりに `agent:` を指定すると、そのノードはエージェント Markdown ファイルのシステムプロンプト・
入出力スキーマ・`model`/`reasoning_effort` を使って実行されます。`agent:` は `prompt` と同時には
指定できず、`input_schema`/`output_schema`/`schema_name` はエージェントファイル側で決まるため
ノードには書けません。

```yaml
# workflow.yml
default:
  model: local
nodes:
  city-fact:
    agent: agents/city-fact.md
    jq: ".city"
steps:
  - use: city-fact
```

`model`/`reasoning_effort`/`temperature`/`top_p`/`max_tokens` は `ノード` → エージェントファイルの
frontmatter → ワークフローの `default:` の順に、それぞれ独立してフォールバックします。ステップの
入力（前のステップの出力、または最初のステップでは `<PROMPT>`）は、`lait agent run` の `INPUT`
と同じ規則でエージェントのシステムプロンプトに渡され、`{{ input.field }}` でアクセスできます。

`prompt:` を使う通常のノードも同じ handlebars テンプレート（`{{ input.field }}`/`{{ json input }}`
を含む）でレンダリングされるため、フィールドアクセスはエージェントファイルの本文に限りません。
さらに、ワークフロー内で `id` を持つ他のステップの出力は、エージェントのシステムプロンプトから
も `{{ steps.<id> }}` として参照できます（詳細は
[ワークフロー（workflow.yml）](/lait/docs/workflow/#ステップ間の値の受け渡し-stepsid---steps) を参照）。

関連: [ワークフロー（workflow.yml）](/lait/docs/workflow/)、[サブエージェントを使う](/lait/docs/subagents/)

