# ワークフロー（run.yml）

[ドキュメント目次に戻る](./README.md)

`lait run <FILE> <PROMPT>` サブコマンドで、複数の LLM 呼び出しを YAML で逐次実行できます。
各 step は前の step の応答テキストを `{{ input }}` プレースホルダーで受け取り、次の step の
プロンプトに埋め込みます。最初の step の `{{ input }}` には `<PROMPT>`（CLI 引数）が使われます。

```yaml
# run.yml
name: example-flow
description: 要約 → 翻訳 → 整形

# ワークフロー全体の既定値。省略時は lait.config.yml の default: にフォールバック
default:
  model: local
  reasoning_effort: medium

steps:
  - id: summarize
    prompt: |
      次の文章を3行で要約してください。
      {{ input }}

  - id: translate
    model: cloud          # step ごとに上書き可能
    prompt: |
      次の要約を英訳してください。
      {{ input }}

  - id: format
    prompt: |
      次の英訳を Markdown の箇条書きにしてください。
      {{ input }}
```

```sh
cargo run -- run run.yml "要約・翻訳したい文章..."
```

- `steps` は配列の先頭から逐次実行するのが基本です。`when`/`switch` による分岐は可能ですが
  （後述）、並列実行は行いません。
- `model` / `reasoning_effort` は step 単位で省略可能。省略時は
  ワークフロー直下の `default:` → `lait.config.yml` の `default:`、の順にフォールバックします。
- `id` は進捗表示（標準エラー出力）用のラベルで、省略した場合は `step-1`、`step-2`… になります。
- `prompt` も `agent` も `switch` も省略した step はモデルを呼び出さず、`jq` によるデータ変換
  のみを行います（後述）。この場合 `model` は不要です。
- 最後の step の出力のみを標準出力に出します。
- `run` サブコマンドでも `--no-config` は利用できます（例: `lait run run.yml "..." --no-config`）。

## ワークフロー内でのモデル定義

`run.yml` にも `lait.config.yml` と同じ形式の `models` を書けます。`default.model` /
`steps[].model` で参照するエイリアスをワークフローファイル内に閉じて定義でき、
`lait.config.yml` を用意しなくてもワークフロー単体で完結させられます。

```yaml
# run.yml
models:
  local:
    - provider:
        base_url: http://localhost:1234/v1
      model_id: local-model
  cloud:
    - provider:
        base_url: https://api.example.com/v1
        api_key: your-api-key
      model_id: cloud-model
      default_reasoning_effort: high

default:
  model: local

steps:
  - prompt: "次の文章を要約してください。\n{{ input }}"
  - model: cloud
    prompt: "次の要約を英訳してください。\n{{ input }}"
```

同じ名前のエイリアスがワークフローと `lait.config.yml` の両方にある場合は、ワークフロー内の
定義が優先されます。ワークフローに定義がないエイリアスは、これまでどおり `lait.config.yml`
の `models` から解決されます。

## JSON 出力の指定と jq による加工

step に `json_schema`（と任意で `schema_name`）を指定すると、CLI の `--json-schema` /
`--schema-name` と同じく Structured Outputs を要求し、モデルの応答を JSON にできます。
さらに `jq` を指定すると、その step の出力（モデルの応答、または `prompt` を省略した場合は
そのときの `{{ input }}`）に [jq](https://jqlang.org/) フィルターを適用し、その結果が次の
step の `{{ input }}` になります。

`json_schema` に指定した値は、まずワークフロー直下の `json_schemas:` のキーとして解決を
試み、一致するキーがなければ（CLI と同じく）JSON Schema ファイルへのパスとして扱われます。
`json_schemas:` の各エントリは、スキーマ本体を直接書く `schema:` と、外部ファイルを指す
`file_path:` のどちらか一方を指定します。

```yaml
# run.yml
default:
  model: local
json_schemas:
  city_fact:
    schema:
      type: object
      properties:
        city: { type: string }
        population: { type: integer }
      required: [city, population]
      additionalProperties: false
  weather_fact:
    file_path: weather.schema.json
steps:
  - id: extract
    prompt: |
      次の文章から都市名と人口を JSON で抽出してください。
      {{ input }}
    json_schema: city_fact
    schema_name: city_fact
    jq: ".city"

  - id: introduce
    prompt: |
      次の都市名を使って一文で紹介してください。
      {{ input }}
```

- `schema:` はスキーマ本体を直接書くので、外部ファイルを用意せずワークフロー単体で完結
  させたり、複数の step から同じスキーマを名前で参照したりできます。
- `file_path:` は（`json_schemas:` を使わない場合の `json_schema: city.schema.json` と
  同じく）JSON Schema ファイルへのパスです。
- `json_schemas:` を使わず、これまでどおり `json_schema: city.schema.json` のように
  直接ファイルパスを指定することもできます（`json_schemas:` に同名のキーがある場合は
  そちらが優先されます）。
- `json_schema` を指定するには `prompt` が必須です（`prompt` のない step には適用先がありません）。
- `schema_name` は `json_schema` とセットで指定します（既定値は `structured_output`）。
- `jq` の出力が文字列の場合は `jq -r` のように引用符なしのテキストとして展開されます。それ以外
  （オブジェクト・配列・数値など）はコンパクトな JSON テキストとして展開されます。
- `jq` フィルターが複数の値を出力した場合は改行区切りで連結します。
- `jq` のみを指定して `prompt` を省略すると、モデルを呼び出さずにその時点の `{{ input }}` を
  変換するだけの step になります（`model` の指定は不要です）。この場合、入力は有効な JSON で
  ある必要があります。

## 条件分岐（`when` / `switch`）

step には [jq](https://jqlang.org/) フィルターを条件式として使う2種類の分岐構文が使えます。
条件式はその時点の `{{ input }}` に対して評価されます。JSON としてパースできればそのオブジェクト
/配列/値に対して、パースできないプレーンテキストであれば JSON 文字列としてラップした上で評価
されるため、直前の step が `prompt` のみ（構造化出力なし）でテキストを返した場合でも条件式は
壊れません。条件式の出力はちょうど1つの値である必要があり（0個・複数個はエラー）、その値が
`false`/`null` なら偽、それ以外はすべて真になります（jq 自身の truthy/falsy 判定と同じです）。

### `when:` ―― step 単位のガード

既存の step（`prompt`/`agent`/`jq` のいずれか）に `when:` を追加すると、条件が偽のときその
step 全体（モデル呼び出しや `jq` を含む）をスキップし、入力を無変換のまま次の step に渡します。

```yaml
# run.yml
steps:
  - id: maybe-translate
    when: '.lang != "en"'
    prompt: |
      次の文章を英訳してください。
      {{ input }}
```

### `switch:` ―― 複数分岐

step に `switch:` を指定すると、その step は他の全フィールド（`prompt`/`agent`/`jq`/`when` 等）
を持てない代わりに分岐ルーターになります。`cases` を先頭から評価し、最初に `when` が真になった
ケースの `steps`（入れ子の step 列）を実行します。どのケースにも一致しなかった場合は `else:` の
`steps` を実行し、`else:` がなければエラーで停止します（分岐漏れを黙って通過させないためです）。
分岐後は `switch` step の続き（親の `steps` の次の要素）にそのまま戻ります。

```yaml
# run.yml
steps:
  - id: triage
    prompt: |
      次の問い合わせを分類してください。
      {{ input }}
    json_schema: triage
    schema_name: triage

  - id: route
    switch:
      cases:
        - id: high              # 任意。進捗表示用のラベル
          when: '.severity == "high"'
          steps:
            - id: escalate
              model: cloud
              prompt: "緊急対応メモを書いてください。\n{{ input }}"
        - id: medium
          when: '.severity == "medium"'
          steps:
            - id: draft-reply
              prompt: "通常対応の返信文を作成してください。\n{{ input }}"
      else:
        - id: auto-close
          jq: ".summary"

  - id: notify
    prompt: "次の内容を1行の通知文にしてください。\n{{ input }}"
```

- `switch` の `steps`（`cases[].steps`/`else`）は空配列にできません。少なくとも1step必要です。
- `switch` は `id` によるジャンプ（`goto`）やループではありません。分岐は非循環で、実行後は必ず
  親の `steps` に戻ります。
- 分岐が入ると進捗表示（標準エラー出力）は `[index/total]` ではなく、実行された経路上の通し
  番号 `[n] id` になります（スキップされた step も番号を1つ消費し `[n] id (skipped)` と出ます）。
