# lait

Lightweight AI Tool (lait) は、YAML で定義したハーネス、Agent Loop、Flow を CLI から実行・制御するためのツールです。

## 必要な環境

- Rust stable
- `rustfmt` と `clippy`（`rust-toolchain.toml` により自動的に指定されます）

## 使い方

リポジトリのルートで次のコマンドを実行します。

```sh
cargo run -- --model <MODEL_ID_OR_ALIAS> "プロンプト"
```

`makers`（cargo-make）を使う場合は、CLI の引数を `run` タスクの後ろに渡します。

```sh
makers run -- --model <MODEL_ID> "プロンプト"
```

`makers run` は macOS では Apple Clang（`/usr/bin/clang`、`/usr/bin/clang++`）と
`xcrun --sdk macosx --show-sdk-path` の SDK を自動的に設定してから `cargo run` を実行します。
Nix など別の `cc` が PATH にあっても、`aws-lc-sys` のリンクに必要な macOS SDK が使われます。

`lait` は `async-openai` を使って OpenAI Compatible API に接続し、単体のプロンプトをチャット補完として送信する CLI です。LM Studio のローカルサーバーを利用した動作確認や、LLM API 接続のサンプルとして使えます。

### LM Studio の準備

実行前に、次の状態にしてください。

1. LM Studio を起動し、使用するモデルをロードする。
2. LM Studio の Developer（Local Server）画面からサーバーを起動する。
3. ロードしたモデルの ID（`<MODEL_ID>`）を確認する。

LM Studio の既定のエンドポイントは `http://localhost:1234/v1` です。別のホストやポートで起動している場合は、`--base-url` または `OPENAI_BASE_URL` で変更できます。

### CLI 引数と環境変数

| CLI 引数 | 環境変数 | 説明 |
| --- | --- | --- |
| `--base-url <URL>` | `OPENAI_BASE_URL` | OpenAI Compatible API のベース URL。既定値は `http://localhost:1234/v1`。 |
| `--model <MODEL_ID_OR_ALIAS>` | `LLM_MODEL` | モデル ID または設定ファイルの alias。CLI 引数、環境変数、または設定ファイルで指定できます。 |
| `--api-key <KEY>` | `OPENAI_API_KEY` | API キー。任意。認証を有効にしたサーバーで指定します。 |
| `--show-reasoning` | — | 対応サーバーが返す `reasoning`（旧形式の `reasoning_content` にも対応）を回答前に表示します。既定では非表示です。 |
| `--json` | — | CLI の応答を JSON 形式で出力します。API の Structured Outputs を指定する `--json-schema` とは別の機能です。 |
| `--json-schema <FILE>` | — | API の Structured Outputs に使用する JSON Schema ファイル。指定時は `response_format` の `type` を `json_schema`、`strict` を `true` として送信します。 |
| `--schema-name <NAME>` | — | Structured Outputs のスキーマ名。既定値は `structured_output` です。`--json-schema` と組み合わせて使用します。 |
| `--reasoning-effort <EFFORT>` | `LLM_REASONING_EFFORT` | 推論の実行レベル。`none`、`minimal`、`low`、`medium`、`high`、`xhigh` のいずれかを指定します。未指定時は API リクエストにフィールドを追加しません。 |
| `<PROMPT>` | — | 送信する単一のプロンプト。 |

### 設定ファイル

CLI 引数や環境変数で指定していない値は、コマンドを実行したディレクトリの
`lait.config.yml` からデフォルトとして読み込まれます。設定できる基本項目は次のとおりです。

```yaml
# lait.config.yml
base_url: http://localhost:1234/v1
api_key: lm-studio
default:
  model: local-model
  reasoning_effort: medium
```

#### モデル定義と alias

複数の呼び出しモデルを設定ファイルに定義し、alias で使い回せます。`models` は alias をキー、
モデル定義の配列を値にするマップです。各要素には `provider.base_url` と `model_id` を指定し、
`provider.api_key` と `default_reasoning_effort` は任意で指定できます。プロバイダーのキーは
正式名称の `provider` を使用してください。

```yaml
# lait.config.yml
models:
  local:
    - provider:
        base_url: http://localhost:1234/v1
      model_id: local-model
      default_reasoning_effort: medium
  cloud:
    - provider:
        base_url: https://api.example.com/v1
        api_key: your-api-key
      model_id: cloud-model
      default_reasoning_effort: high

# alias は `default.model`、CLI、環境変数から参照できます。
default:
  model: local
```

`default.model`、`--model`、`LLM_MODEL` には alias または生のモデル ID を指定できます。alias を指定した
場合は、対応する配列の先頭要素が使用され、その要素の `model_id` とプロバイダー設定が
リクエストに適用されます。生のモデル ID を指定した場合は、従来どおりトップレベル設定の
`base_url` などが使用されます。

設定値は項目ごとに、次の優先順位で解決されます。CLI 引数と環境変数の間では CLI 引数が優先されます。

`CLI 引数 > 環境変数 > モデル定義 > 既存トップレベル設定 > 組み込み既定値`

たとえば alias のモデル定義が `provider.base_url` を持つ場合、その値はトップレベルの `base_url`
より優先されます。CLI の `--base-url` や `OPENAI_BASE_URL` を指定した場合は、それらがモデル定義を
上書きします。`provider.api_key` と `default_reasoning_effort` を省略した場合は、対応する
トップレベルの `api_key`、`default.reasoning_effort` がフォールバックとして使用されます。

`base_url`、`api_key` はトップレベルの項目として、フォールバック用の `model`、`reasoning_effort`
は `default:` の配下にまとめて指定します。設定ファイルの自動読込を
無効にする場合は `--no-config` を指定してください。この場合は設定ファイルを読み込まず、CLI
引数、環境変数、既定値だけが使用されます。

詳細なオプションは次のコマンドで確認できます。

```sh
cargo run -- --help
cargo run -- --version
makers --list-all-steps
```

### ワークフロー（`run.yml`）

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

#### ワークフロー内でのモデル定義

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

#### JSON 出力の指定と jq による加工

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

#### 条件分岐（`when` / `switch`）

step には [jq](https://jqlang.org/) フィルターを条件式として使う2種類の分岐構文が使えます。
条件式はその時点の `{{ input }}` に対して評価されます。JSON としてパースできればそのオブジェクト
/配列/値に対して、パースできないプレーンテキストであれば JSON 文字列としてラップした上で評価
されるため、直前の step が `prompt` のみ（構造化出力なし）でテキストを返した場合でも条件式は
壊れません。条件式の出力はちょうど1つの値である必要があり（0個・複数個はエラー）、その値が
`false`/`null` なら偽、それ以外はすべて真になります（jq 自身の truthy/falsy 判定と同じです）。

**`when:` ―― step 単位のガード**

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

**`switch:` ―― 複数分岐**

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

### エージェント Markdown ファイル（agent.md）

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

#### ワークフローからエージェントファイルを使う

`run.yml` の step で `prompt`/`json_schema`/`schema_name` の代わりに `agent:` を指定すると、
その step はエージェント Markdown ファイルのシステムプロンプト・入出力スキーマ・
`model`/`reasoning_effort` を使って実行されます。`agent:` は `prompt` と同時には指定できず、
`json_schema`/`schema_name` はエージェントファイル側で決まるため step には書けません。

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

### ビルドと実行

開発中は `cargo run` で実行できます。

```sh
cargo run -- --model "モデル ID" "Rustについて一文で説明してください。"
```

同じリクエストを `makers` から実行する場合は次のようにします。`--` より後ろの引数は
そのまま `lait` に転送されるため、プロンプト中の空白や日本語も保持されます。

```sh
makers run -- --model "モデル ID" "Rustについて一文で説明してください。"
```

推論モデルに推論の実行レベルを指定する場合は、`--reasoning-effort` に
`none`、`minimal`、`low`、`medium`、`high`、`xhigh` のいずれかを渡します。
`none` は推論を行わない指定です（サーバーとモデルが対応している場合）。未指定時は
`reasoning_effort` フィールドを送信せず、サーバー側の既定値を使用します。

```sh
cargo run -- --model "モデル ID" --reasoning-effort high "複雑な問題を解いてください。"
```

環境変数でも指定できます。CLI 引数と環境変数の両方を指定した場合は CLI 引数が優先されます。

```sh
export LLM_REASONING_EFFORT="medium"
cargo run -- --model "モデル ID" "複雑な問題を解いてください。"
```

ビルド、テスト、フォーマット確認、Clippy も `makers` のタスクとして実行できます。

```sh
makers build
makers test
makers fmt-check
makers clippy
```

対応サーバーから返された推論内容を回答前に表示する場合は、`--show-reasoning` を指定します。推論内容が返されない場合は、従来どおり回答のみが表示されます。

```sh
cargo run -- --show-reasoning --model "モデル ID" "Rustについて一文で説明してください。"
```

モデル ID に空白が含まれる場合は、上の例のように引用符で囲んでください。環境変数を使う場合は、次のように設定してからプロンプトだけを指定できます。

```sh
export LLM_MODEL="モデル ID"
cargo run -- "Rustについて一文で説明してください。"
```

リリースビルドを作成して実行する場合は、次のコマンドを使います。

```sh
cargo build --release --locked
./target/release/lait --model "モデル ID" "こんにちは。"
```

### 認証なし／認証あり

LM Studio のローカルサーバーで認証を無効にしている場合は、API キーを指定せずに実行できます。

```sh
cargo run -- --model "モデル ID" "こんにちは。"
```

サーバー側で認証を有効にしている場合は、`OPENAI_API_KEY` または `--api-key` でキーを指定します。シェルの履歴にキーを残したくない場合は環境変数を使ってください。

```sh
export OPENAI_API_KEY="your-api-key"
cargo run -- --model "モデル ID" "認証付きでリクエストしてください。"
```

`makers` で実行する場合も同じ環境変数を利用できます。

```sh
export OPENAI_API_KEY="your-api-key"
makers run -- --model "モデル ID" "認証付きでリクエストしてください。"
```

一時的に CLI 引数で指定することもできます。

```sh
cargo run -- --api-key "your-api-key" --model "モデル ID" "こんにちは。"
```

一時的な指定は `makers` でも可能です。

```sh
makers run -- --api-key "your-api-key" --model "モデル ID" "こんにちは。"
```

### 出力例

リクエストが成功すると、モデルの応答テキストが標準出力に表示されます。応答内容はロードしたモデルによって異なります。

```text
こんにちは。今日はどのようなお手伝いができますか？
```

`--json-schema` を指定すると、API の Structured Outputs に JSON Schema を渡せます。`--json-schema` はモデルの応答形式を指定するオプションで、CLI の標準出力形式は変更しません。スキーマは JSON ファイルに記述します。

```json
{
  "type": "object",
  "properties": {
    "answer": { "type": "string" }
  },
  "required": ["answer"],
  "additionalProperties": false
}
```

```sh
cargo run -- --model "モデル ID" --json-schema response.schema.json --schema-name answer_response "JSON Schema に従って回答してください。"
```

`--schema-name` を省略した場合の名前は `structured_output` です。Structured Outputs は常に strict モード（`strict: true`）でリクエストされます。

`--json` を指定すると、API の応答を CLI 用の JSON オブジェクトとして標準出力に表示します。これは `--json-schema` とは別の機能で、`content` と `reasoning` のキーを常に含みます。

| キー | 型 | 説明 |
| --- | --- | --- |
| `content` | `string` | 回答テキスト。 |
| `reasoning` | `string` または `null` | 推論テキスト。推論がない場合は `null`。 |

```sh
cargo run -- --json --model "モデル ID" "Rustについて一文で説明してください。"
```

```json
{
  "content": "Rustは安全性と性能を両立したシステムプログラミング言語です。",
  "reasoning": null
}
```

### トラブルシュート

- `connection refused` になる場合は、LM Studio の Local Server が起動しているか、ホストとポートが正しいか確認してください。既定値と異なる場合は `--base-url http://ホスト:ポート/v1` または `OPENAI_BASE_URL` を指定します。
- `404` やモデルが見つからないエラーになる場合は、ベース URL に `/v1` が含まれていることと、LM Studio でロード済みのモデル ID と `<MODEL_ID>` が完全に一致することを確認してください。
- `401` や認証エラーになる場合は、サーバーの認証設定を確認し、`OPENAI_API_KEY` または `--api-key` に正しいキーを指定してください。認証なしの LM Studio では API キーを省略します。
- `--model` が不足しているというエラーになる場合は、`--model <MODEL_ID>` を指定するか、`LLM_MODEL` を設定してください。
- 応答に時間がかかる場合は、モデルが LM Studio にロード済みか、LM Studio のサーバーログにエラーが出ていないかを確認してください。初回リクエストはモデルの準備に時間がかかることがあります。

CLI のヘルプとバージョンは以下で確認できます。

```sh
cargo run -- --help
cargo run -- --version
```

## 開発

テスト、フォーマット、Lint、リリースビルドは次のコマンドで実行できます。

```sh
cargo test --locked
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo build --release --locked
```

`makers` で同じ検証を行う場合は `makers test`、`makers fmt-check`、`makers clippy`、
`makers build` を使用できます。macOS arm64 で発生する `ld: library not found for -liconv`
には、`makers` の cargo ラッパーが Apple Clang と Xcode SDK を設定して対応します。

GitHub Actions でも同じチェックを実行します。
