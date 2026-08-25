# エージェント Markdown ファイル（agent.md）

[ドキュメント目次に戻る](./README.md)

`lait agent run <FILE> <INPUT>` サブコマンドで、Markdown ファイル1つでエージェントを定義・
実行できます。ファイルは YAML の frontmatter（`---` で区切られたブロック）とそれに続く
Markdown 本文で構成され、本文がシステムプロンプトのテンプレートになります。

```markdown
---
name: city-fact
description: 文章から都市名と人口を抽出する
model: local
reasoning_effort: medium
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
- `agent:`（step）や `file_path:`（`json_schemas:`/`input_schema:`/`output_schema:`）に書く
  パスは、既存の `--json-schema <FILE>` や `lait.config.yml` の探索と同じく、常にコマンドを
  実行したディレクトリ（カレントディレクトリ）からの相対パスとして解決されます。エージェント
  ファイルや `run.yml` 自体の場所からの相対パスではないため、`run.yml` を別ディレクトリから
  実行する場合は注意してください。
- `model` / `reasoning_effort` は省略可能で、`lait.config.yml` の `default:` にフォールバック
  します。CLI から `--model` 等で上書きすることはできません。
- `input_schema` / `output_schema` は、`json_schemas:` と同じ形式で、スキーマ本体を直接書く
  `schema:` と外部ファイルを指す `file_path:` のどちらか一方を指定します。
- `structured_output: true` を指定する場合は `output_schema` が必須です（逆に `output_schema`
  だけ指定して `structured_output` を省略/false にするのはエラーです）。`schema_name` は
  `structured_output: true` のときだけ使われ、省略時は `structured_output` になります。
- `input_schema` を指定すると、`INPUT` が JSON オブジェクトであること、および
  `input_schema.schema.required` に列挙したフィールドがすべて存在することを実行前に検証します
  （型やネストした構造までは検証しません）。検証に失敗するとモデルを呼び出さずにエラーになります。
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

## ワークフローからエージェントファイルを使う

`run.yml` の step で `prompt`/`input_schema`/`output_schema`/`schema_name` の代わりに `agent:` を
指定すると、その step はエージェント Markdown ファイルのシステムプロンプト・入出力スキーマ・
`model`/`reasoning_effort` を使って実行されます。`agent:` は `prompt` と同時には指定できず、
`input_schema`/`output_schema`/`schema_name` はエージェントファイル側で決まるため step には
書けません。

```yaml
# run.yml
default:
  model: local
steps:
  - agent: agents/city-fact.md
    jq: ".city"
```

`model`/`reasoning_effort` は `step` → エージェントファイルの frontmatter →
ワークフローの `default:` の順にフォールバックします。ステップの入力（前の step の出力、
または最初の step では `<PROMPT>`）は、`lait agent run` の `INPUT` と同じ規則でエージェントの
システムプロンプトに渡され、`{{ input.field }}` でアクセスできます。

なお、通常の `prompt:` を使う step のテンプレート構文（`{{ input }}` のみ）はこれまでどおりで、
`{{ input.field }}` のようなフィールドアクセスはエージェントファイルの本文でのみ使えます。

関連: [ワークフロー（run.yml）](./workflow.md)
