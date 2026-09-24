# エージェント Markdown ファイル（agent.md）

[ドキュメント目次に戻る](./README.md)

`lait agent run <FILE> <INPUT>` サブコマンドで、Markdown ファイル1つでエージェントを定義・
実行できます。ファイルは YAML の frontmatter（`---` で区切られたブロック）とそれに続く
Markdown 本文で構成され、本文がシステムプロンプトのテンプレートになります。frontmatter の
補完・検証には [`lait schema agent`](./schema.md) が出力する JSON Schema を使えます。

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
  type: object
  properties:
    text: { type: string }
  required: [text]
output_schema:
  type: object
  properties:
    city: { type: string }
    population: { type: integer }
  required: [city, population]
  additionalProperties: false
schema_name: city_fact
---
次の文章から都市名と人口を JSON で抽出してください。

{{ input.text }}
```

```sh
cargo run -- agent run city-fact.md '{"text":"東京の人口は約1400万人です。"}'
```

`<FILE>` はパスの代わりに `lait.config.yml` の `agents:` に登録した名前（または
[`lait deps`](./deps.md) で取り込んだ `agent` 依存の名前）でも指定できます。`lait run` の
`workflows:` 解決と同じく、実在するファイルがあればそちらが優先され、存在しない場合にだけ
`agents:` から名前解決されます。

## ファイルの構成とパス

ファイルは1行目が必ず `---` で始まり、次に現れる `---` 行までが frontmatter（YAML）、
それ以降が本文（システムプロンプトのテンプレート）になります。

`input_schema`/`output_schema` に `{file: <パス>}` で指定したスキーマファイルは、**この
エージェントファイルのディレクトリ** からの相対パスとして解決されます。エージェントとスキーマを
同じ場所に置けば、どこから実行しても同じように読み込めます。

frontmatter のモデル・サンプリング関連フィールドは次のとおりです。

| フィールド | 説明 |
| --- | --- |
| `model` | 使用するモデル（alias またはモデル ID）。 |
| `reasoning_effort` | 推論 effort レベル。 |
| `temperature` | サンプリング温度（`0.0`〜`2.0`）。 |
| `top_p` | nucleus sampling の確率質量（`0.0`〜`1.0`）。 |
| `max_tokens` | 最大出力トークン数（`1`以上）。 |

いずれも省略可能で、`lait.config.yml` の `default:` にフォールバックします。直接
`lait agent run` を実行する場合、CLI の `--model` などでエージェントファイルの値を上書きする
ことはできません。`temperature`・`top_p`・`max_tokens` は、範囲外の値を指定するとファイルの
読み込み時点でエラーになります。

## 入力の渡し方

`INPUT` はまず JSON としてパースされます。パースに成功した場合はオブジェクト・配列・値が
テンプレートに渡され、失敗した場合は文字列としてそのまま渡されます。レンダリングされた本文は
system ロールのメッセージとして送信され、`INPUT`（元の生テキスト）は別途 user ロールの
メッセージとして送信されます。

## 入力の検証（`input_schema`）

`input_schema` / `output_schema` には、JSON Schema の本体を直接書くか、`{file: <パス>}` で
JSON ファイルを指定します（`file` だけを持つマッピングがファイル参照、それ以外はスキーマ本体です）。

`input_schema` を指定すると、`INPUT` がスキーマの `type`（`type: object` なら JSON オブジェクト）
に合うこと、`required` に列挙したフィールドがすべて存在すること、さらに `properties`/`items`
で宣言したフィールドの `type`/`enum` やネストしたオブジェクト・配列の中身までを実行前に
再帰的に検証します。`format`・`pattern`・数値の範囲・`additionalProperties`・
`oneOf`/`anyOf`/`allOf`・`$ref` は検証しません。検証に失敗するとモデルを呼び出さずにエラーになります。

## テンプレートの書き方

本文のシステムプロンプトは handlebars 構文です。

- `{{ input.city }}` のようにドット区切りでフィールドにアクセスできます。
- オブジェクトや配列全体を JSON テキストとして埋め込むには、`{{ json input }}` または
  `{{ json input.field }}` を使います。
- `{{ input }}` は値のテキスト形式を出力します（文字列はそのまま、オブジェクト・配列などは
  コンパクトな JSON）。`{{ json input }}` は文字列も引用符付きの JSON として出力します。
- テンプレート中の未定義の変数を参照した場合もエラーになります。

## 構造化出力（`output_schema`）

`output_schema` を指定すると Structured Outputs（`response_format` の `json_schema`、strict
モード）を要求します。省略すると通常のテキスト応答です。`schema_name` はそのスキーマ名で、
`output_schema` と一緒にだけ指定でき、省略時は `structured_output` です。

ワークフローの `agent:` ステップとして実行した場合、`output_schema` を持つエージェントの結果は
パース済みの JSON（スキーマで検証済み）として次のステップに渡ります。

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
[MCP サーバーのツールを使う](./mcp.md) を参照してください。

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
仕組みは [スキルを使う](./skills.md) を参照してください。

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
詳しい仕組みは [サブエージェントを使う](./subagents.md) を参照してください。

## カスタムシェルツールの利用

frontmatter に `tools:`（`lait.config.yml` の `tools:` エントリ名のリスト）を指定すると、
対応するローカルコマンドが呼び出し可能なツールとして渡されます。`mcp:`/`subagents:` と同じ
tool loop の仕組みに乗ります。

```markdown
---
model: local
tools: [ripgrep]
---
{{ input.task }} を実行してください。
```

`tools:` を省略した場合は `lait.config.yml` の `default.tools` にフォールバックします。詳しい
仕組みは [カスタムシェルツールを使う](./tools.md) を参照してください。

## ワークフローからエージェントを使う

ワークフローの `agent:` ステップにエージェントファイルのパス（ワークフローファイルからの
相対パス）を書くと、そのファイルのシステムプロンプト・入出力スキーマ・設定を使って実行します。
ワークフローの `agents:` に同じ形式のエージェントを直接定義することもできます（本文の代わりに
`system:` を書きます）。

```yaml
# workflow.yml
default:
  model: local
agents:
  translator:
    model: cloud
    system: "次の文章を英訳してください。"
steps:
  - id: city
    agent: agents/city-fact.md
    output: '.city'
  - agent: translator
```

`model`/`reasoning_effort`/`temperature`/`top_p`/`max_tokens` やツール連携の設定は、ステップ →
エージェント定義 → ワークフローの `default:` → `lait.config.yml` の `default:` の順に、それぞれ
独立してフォールバックします。ステップの入力は `lait agent run` の `INPUT` と同じくユーザー
メッセージとして送られ（オブジェクトなどはコンパクトな JSON テキスト）、システムプロンプトの
テンプレートからは値として `{{ input.field }}` でアクセスできます。

ワークフローのステップとして実行した場合、システムプロンプトからは `{{ steps.<id> }}`（記録済みの
ステップの値）・`{{ inputs.<name> }}`（ワークフローの入力）・`{{ loop.index }}` も参照できます
（詳細は [ワークフロー（workflow.yml）](./workflow.md#値とテンプレートjq) を参照）。
`lait agent run` から直接実行したときやサブエージェントとして呼び出されたときは、これらは
使えません。

関連: [ワークフロー（workflow.yml）](./workflow.md)、[サブエージェントを使う](./subagents.md)
