# ワークフロー（workflow.yml）

[ドキュメント目次に戻る](./README.md)

`lait run <FILE> <PROMPT>` サブコマンドで、複数の LLM 呼び出しを YAML で逐次実行できます。
各 step は前の step の応答テキストを `{{ input }}` プレースホルダーで受け取り、次の step の
プロンプトに埋め込みます。最初の step の `{{ input }}` には `<PROMPT>`（CLI 引数）が使われます。

```yaml
# workflow.yml
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
cargo run -- run workflow.yml "要約・翻訳したい文章..."
```

- `steps` は配列の先頭から逐次実行するのが基本です。`when`/`switch` による分岐（後述）、
  `parallel` による複数の step 列の同時実行（ファンアウト/ファンイン）、`loop` による
  条件ループ、`for_each` による配列反復も可能です（いずれも後述）。
- `model` / `reasoning_effort` は step 単位で省略可能。省略時は
  ワークフロー直下の `default:` → `lait.config.yml` の `default:`、の順にフォールバックします。
- `id` は進捗表示（標準エラー出力）用のラベルで、省略した場合は `step-1`、`step-2`… になります。
- `prompt` も `agent` も `switch` も省略した step はモデルを呼び出さず、`jq` によるデータ変換
  のみを行います（後述）。この場合 `model` は不要です。
- 最後の step の出力のみを標準出力に出します。
- `run` サブコマンドでも `--no-config` は利用できます（例: `lait run workflow.yml "..." --no-config`）。

## ワークフロー内でのモデル定義

`workflow.yml` にも `lait.config.yml` と同じ形式の `models` を書けます。`default.model` /
`steps[].model` で参照するエイリアスをワークフローファイル内に閉じて定義でき、
`lait.config.yml` を用意しなくてもワークフロー単体で完結させられます。

```yaml
# workflow.yml
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

step に `output_schema`（と任意で `schema_name`）を指定すると、CLI の `--json-schema` /
`--schema-name` と同じく Structured Outputs を要求し、モデルの応答を JSON にできます。
さらに `jq` を指定すると、その step の出力（モデルの応答、または `prompt` を省略した場合は
そのときの `{{ input }}`）に [jq](https://jqlang.org/) フィルターを適用し、その結果が次の
step の `{{ input }}` になります。

`output_schema` に指定した値は、まずワークフロー直下の `json_schemas:` のキーとして解決を
試み、一致するキーがなければ（CLI と同じく）JSON Schema ファイルへのパスとして扱われます。
`json_schemas:` の各エントリは、スキーマ本体を直接書く `schema:` と、外部ファイルを指す
`file_path:` のどちらか一方を指定します。

```yaml
# workflow.yml
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
    output_schema: city_fact
    schema_name: city_fact
    jq: ".city"

  - id: introduce
    prompt: |
      次の都市名を使って一文で紹介してください。
      {{ input }}
```

- `schema:` はスキーマ本体を直接書くので、外部ファイルを用意せずワークフロー単体で完結
  させたり、複数の step から同じスキーマを名前で参照したりできます。
- `file_path:` は（`json_schemas:` を使わない場合の `output_schema: city.schema.json` と
  同じく）JSON Schema ファイルへのパスです。
- `json_schemas:` を使わず、これまでどおり `output_schema: city.schema.json` のように
  直接ファイルパスを指定することもできます（`json_schemas:` に同名のキーがある場合は
  そちらが優先されます）。
- `output_schema` を指定するには `prompt` が必須です（`prompt` のない step には適用先がありません）。
- `schema_name` は `output_schema` とセットで指定します（既定値は `structured_output`）。
- `jq` の出力が文字列の場合は `jq -r` のように引用符なしのテキストとして展開されます。それ以外
  （オブジェクト・配列・数値など）はコンパクトな JSON テキストとして展開されます。
- `jq` フィルターが複数の値を出力した場合は改行区切りで連結します。
- `jq` のみを指定して `prompt` を省略すると、モデルを呼び出さずにその時点の `{{ input }}` を
  変換するだけの step になります（`model` の指定は不要です）。この場合、入力は有効な JSON で
  ある必要があります。

### 入力の検証（`input_schema`）

`output_schema` が出力（モデルの応答）を検証するのに対して、`input_schema` は step が実行される
前の入力（`prompt` をレンダリングする前、あるいは `prompt` のない step では `jq` を適用する前
の `{{ input }}`）を検証します。指定した値は `output_schema` と同じく、まず `json_schemas:` の
キーとして解決を試み、一致するキーがなければ JSON Schema ファイルへのパスとして扱われます。

```yaml
json_schemas:
  city:
    schema:
      type: object
      required: [city]
steps:
  - id: introduce
    input_schema: city
    prompt: |
      次の都市名を使って一文で紹介してください。
      {{ input }}
```

検証は「JSON オブジェクトであること」と「`required` に列挙されたキーが揃っていること」だけを
確認する簡易的なもので、型やネストしたスキーマまでは検査しません。必須フィールドが欠けている
入力を早期に（モデルを呼び出す前に）エラーとして弾くためのものです。`agent` を指定した step では
agent ファイル側の `input_schema` が使われるため、step に `input_schema` を重ねて指定すること
はできません。

## エラー処理（`retry` / `timeout` / `on_error`）

step には `retry:`（再試行）、`timeout:`（1回の試行あたりの時間制限）、`on_error:`（再試行を
使い切っても失敗したときのフォールバック）を指定できます。いずれも、その step自身のアクション
（`input_schema` の検証 → `prompt`/`agent` の呼び出し → `jq`、をひとまとまりとして扱います）
に対して働きます。

```yaml
# workflow.yml
default:
  model: local
steps:
  - id: call
    prompt: "{{ input }}"
    timeout: 30
    retry:
      max_attempts: 3
      delay_seconds: 1
      backoff: 2.0
    on_error:
      steps:
        - jq: '{ fallback: .error }'
```

- `timeout:` は1回の試行にかける秒数の上限です。超過した試行は失敗として扱われ（`retry` が
  設定されていれば再試行の対象になります）、1以上である必要があります。
- `retry:` の `max_attempts` は初回を含む総試行回数で、必須かつ1以上です（`max_attempts: 3` は
  「1回試して、失敗したら最大2回まで再試行する」という意味です）。`delay_seconds`（初回の再試行
  前に待つ秒数、既定0）と `backoff`（再試行のたびに待機時間へ掛ける倍率、既定1.0）は任意です。
  進捗表示（標準エラー出力）に `-> attempt k/max failed: ...; retrying in Ns` の行が出ます。
- `on_error:` は、`retry` を使い切っても（あるいは `retry` がなければ最初の1回で）失敗した
  ときに、ワークフローを異常終了させる代わりに実行する `steps` です。入力には
  `{"error": "<失敗内容のメッセージ>", "input": <その step に入ってきた入力>}` という
  オブジェクトが渡されるので、`jq` で必要な形に加工してください。`on_error` の `steps` の中でも
  `stop`/`break`/`switch`/`parallel`/`loop`/`for_each` を含め、通常の step と同じ構文が使えます
  （`break` を使う場合は、失敗した step 自身が `loop`/`for_each` の本文の中にある必要があります）。
  `on_error` を指定しなければ、これまでどおり失敗はワークフロー全体のエラーになります。
- `retry`/`timeout`/`on_error` は `switch`/`parallel`/`loop`/`for_each` とは併用できません
  （これらは入れ子の step 列に処理を委ねるだけで、自分ではアクションを実行しないためです）。

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
# workflow.yml
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
# workflow.yml
steps:
  - id: triage
    prompt: |
      次の問い合わせを分類してください。
      {{ input }}
    output_schema: triage
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
  `switch` は常にどちらか一方の経路しか実行しないため、この通し番号は分岐をまたいでも連続します。

## 並列実行（`parallel`）

step に `parallel:` を指定すると、その step は他の全フィールド（`prompt`/`agent`/`jq`/`when`/
`switch` 等、`id` を除く）を持てない代わりに、複数の step 列（`branches`）を**同時実行**する
ファンアウト/ファンインになります。各 branch は、その `parallel` step に入ってきた時点の
`{{ input }}` を同じスナップショットとして受け取り、独立した step 列として並列に実行されます。

```yaml
# workflow.yml
default:
  model: local
steps:
  - id: analyze
    parallel:
      branches:
        - id: sentiment
          steps:
            - prompt: |
                次の文章の感情を一言で判定してください。
                {{ input }}
        - id: summary
          steps:
            - prompt: |
                次の文章を1行で要約してください。
                {{ input }}
        - id: keywords
          steps:
            - prompt: |
                次の文章からキーワードを3つ抽出してください。
                {{ input }}
      join: '{sentiment: .sentiment, summary: .summary, keywords: .keywords}'

  - id: report
    prompt: |
      次の情報からレポートを1段落で書いてください。
      {{ input }}
```

- 全 branch が完了すると、各 branch の最終出力（モデル応答、または `jq` で加工した結果）を
  branch の `id` をキーにした JSON オブジェクトへ集約します。値は `when`/`jq` 入力と同じ
  ルールで、JSON としてパースできればそのまま、できなければ JSON 文字列としてラップされます。
  このオブジェクトは常に `branches` の宣言順でキーが並びます（branch の完了順ではありません）。
  そのため `parallel` の出力は実行タイミングに依存せず決定的です。
- `join`（任意）は、その集約オブジェクトに適用する [jq](https://jqlang.org/) フィルターで、
  step の `jq:` と同じ働きをします。省略した場合は、集約オブジェクトそのもの（JSON テキスト）が
  次の step の `{{ input }}` になります。
- `branches` の各要素は `id`（任意。省略時は `branch-1`、`branch-2`… になり、進捗表示と
  集約オブジェクトのキーの両方に使われます）と `steps`（空配列不可。通常の step 列と同様、
  `when`/`switch`/`parallel` を入れ子にできます）を持ちます。同じ `parallel` 内で `id` が
  重複しているとエラーになります（集約オブジェクトのキーとして意味を持つため、`switch` の
  `case.id`＝ラベルのみ、とは異なります）。
- branch のどれか1つでもエラーになった場合、その時点で `parallel` step 全体が失敗します
  （他の branch の結果を待たずに停止する場合があります）。
- `parallel` は `switch`/`loop`/`for_each` と同時に指定できません（1 step につき、これら
  ルーター系フィールドはどれか1つだけ）。
- 分岐中は進捗表示（標準エラー出力）が branch ごとに `[branch-id] [n] id` の形式でインター
  リーブされます（`n` は branch 内だけで完結するローカルな通し番号で、0から数え直します）。
  親の `steps` 側の通し番号は `parallel` step 自体で1つ進むだけで、`switch` と異なり
  branch 内の step 数だけ連続して進むことはありません（複数 branch が同時に進行するため、
  単一の通し番号では実行順を正しく表せないからです）。

## 条件ループ（`loop`）

step に `loop:` を指定すると、その step は他の全フィールド（`prompt`/`agent`/`jq`/`when`/
`switch`/`parallel`/`for_each` 等、`id` を除く）を持てない代わりに、`steps`（ループ本体）を
[jq](https://jqlang.org/) 条件が満たされるまで**逐次**繰り返し実行するループになります。各
イテレーションの最終出力が、次のイテレーションの `{{ input }}` になります（1回目は `loop`
step 自身に入ってきた入力）。

```yaml
# workflow.yml
default:
  model: local
steps:
  - id: retry-until-valid
    loop:
      until: '.valid == true'
      max_iterations: 5
      steps:
        - prompt: |
            前回の失敗理由を踏まえて、もう一度やり直してください。
            {{ input }}
          output_schema: validation_result
          schema_name: validation_result
```

- `while:` は各イテレーションの実行**前**（1回目を含む）に、その時点の入力に対して評価され
  ます。偽ならその時点で1回も実行せず終了するので、0回実行されることもあります。1回目の
  評価対象は `loop` step 自身に入ってきた入力です。`loop` がワークフローの先頭stepの場合、
  これは `<PROMPT>`（CLI引数）そのもの——多くの場合プレーンテキスト——なので、`while` を
  先頭stepに置く場合は特に、条件式が上記のJSON文字列ラップの影響を受けないか注意してください。
- `until:` は各イテレーションの実行**後**に、その出力に対して評価されます。真になった時点で
  停止するため、`steps` は必ず1回以上実行されます。
- `while`/`until` はどちらか一方が必須です（両方、またはどちらもない場合はエラー）。
- `max_iterations` は必須で、1以上である必要があります。この上限に達しても条件を満たさな
  かった場合は**エラーで停止**します（黙って打ち切って処理を続けることはありません）。上限は
  安全弁というより「N回以内に条件を満たすべき」というアサーションです。
- `steps` は空配列にできません。
- `while`/`until` の条件式は `when`/`switch` の `when` と同じ実装（`eval_when`）で評価される
  ため、直前の出力がJSONとしてパースできないプレーンテキストであれば、JSON文字列としてラップ
  された上で評価されます。したがって `until: '.valid == true'` のようにオブジェクトのフィー
  ルドを参照する条件を書く場合は、`steps` の最後のstepで `output_schema` か `jq` を使って
  JSONの値に整形しておく必要があります。
- 進捗表示（標準エラー出力）は `switch` と同様、実行された経路上の通し番号 `[n] id` が
  イテレーションをまたいで連続します（`loop` は並列実行される `parallel` と違い、本質的に
  逐次実行だからです）。加えて `-> iteration k/max` の行で現在のイテレーション回数が出力
  されます。

## 配列反復（`for_each`）

step に `for_each:` を指定すると、その step は他の全フィールド（`loop` と同じく `id` を除く
全フィールド）を持てない代わりに、jqフィルターで選んだ配列の**各要素**に対して `steps` を
1回ずつ（配列の順序どおり、**逐次**）実行し、結果を配列として集約するマップ処理になります。

```yaml
# workflow.yml
default:
  model: local
steps:
  - id: process-items
    for_each:
      items: '.items'
      steps:
        - prompt: "この項目を要約してください: {{ input }}"
      join: 'map(select(. != null))'
```

- `items:` は、その時点の入力に対して評価するjqフィルターで、**単一のJSON配列を1つだけ**返す
  必要があります（`.items` のように配列そのものを返してください。`.items[]` のような
  ストリーム展開ではありません。要素数はゼロでも構いません）。それ以外の値（オブジェクトや
  文字列など）を返した場合や、出力が0個・複数個の場合はエラーになります。
- 配列の各要素が、そのイテレーションの `{{ input }}` になります。`parallel` の branch と
  異なり、`for_each` の本体は要素そのもの以外——周辺のフィールドを含む入力全体——には
  アクセスできません。周辺情報が必要な場合は、`for_each` の手前に `jq` stepを置いて、
  各要素にあらかじめ文脈を埋め込んでおいてください。
- 各イテレーションの最終出力を、`items` の並び順で配列に集約します。`join`（任意）は、その
  配列に適用する jq フィルターで、`parallel` の `join` と同じ役割です。省略した場合は、
  集約した配列そのもの（JSONテキスト）が次のstepの `{{ input }}` になります。
- 配列の要素が0件の場合は0回実行され、出力は空配列（`join` があればそれを適用した結果）に
  なります。これはエラーではありません。
- `max_iterations` は持ちません。配列の長さで自然に有界なため、`for_each` では不要と判断
  しています。
- 進捗表示は `loop` と同様、`switch` と同じ通し番号方式（要素をまたいで連続）に加えて、
  `-> item k/n` の行で現在の要素の位置を出力します。

## 早期終了（`stop` / `break`）

step に `stop: true` または `break: true` を指定すると、その step自身のアクション
（`prompt`/`agent`/`jq`。両方省略も可）を実行した**後**に、通常の逐次実行を打ち切ります。
どちらも `when:` と組み合わせて「ある条件が満たされたら打ち切る」という使い方が基本です。

```yaml
# workflow.yml
steps:
  - id: check
    prompt: "十分な情報が揃っているか判定してください: {{ input }}"
    output_schema: judgement

  - when: '.sufficient == true'
    stop: true          # 揃っていれば、ここでワークフロー全体を正常終了する

  - id: ask-more
    prompt: "不足している情報を尋ねる質問を1つ生成してください: {{ input }}"
```

```yaml
# workflow.yml
steps:
  - id: refine
    loop:
      until: '.valid == true'
      max_iterations: 5
      steps:
        - prompt: "{{ input }}"
          output_schema: validation_result
        - when: '.attempts >= 3'
          break: true    # 3回試したら、valid になっていなくてもループを打ち切る
```

- `stop: true` は、その時点の値を**ワークフロー全体の最終出力**として、残りの step
  （ネストしている `loop`/`for_each`/`switch` の外側も含めて）を一切実行せずに正常終了します。
  ただし `parallel` の branch の中では使えません（他の branch が並行して走っている以上、
  「ワークフローを止める」という操作の意味が定義できないためです）。
- `break: true` は、最も内側の `loop`/`for_each` の本文だけを打ち切ります。`loop` では
  `while`/`until` を満たした場合と同じ扱いで（`until` の場合は `max_iterations` 超過の
  エラーにはなりません）、`for_each` ではそこまでに集めた結果で `join` を実行します。
  `break: true` を使うには、`switch` の中などを経由してでもよいので、`loop`/`for_each` の
  本文の中にいる必要があります（`parallel` の branch をまたいで外側のループを終了することは
  できません）。
- `stop`/`break` は他の全フィールド（`prompt`/`agent`/`jq`/`when` などの併用は可能ですが、
  `switch`/`parallel`/`loop`/`for_each` との併用は不可）と同じ排他ルールに従います。両方を
  同じ step に指定することもできません。
