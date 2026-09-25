# ワークフロー（workflow.yml）

[ドキュメント目次に戻る](./README.md)

`lait run <FILE> [PROMPT]` は、YAML で書いたワークフローを実行します。ワークフローは
**ステップ**（`steps:`）を上から順に実行し、各ステップの結果（値）が次のステップの入力に
なります。LLM 呼び出し・エージェント・コマンド・jq による変換・人間への確認・分岐・並列・
ループを同じ形式のステップとして組み合わせられます。

このページはワークフローファイル **バージョン 2** の仕様です。バージョン 1（`nodes:` と
`use:` を使う形式）からの書き換えは [バージョン 1 からの移行](#バージョン-1-からの移行) を
参照してください。エディタで補完・検証を効かせたい場合は [`lait schema workflow`](./schema.md)
が出力する JSON Schema を使えます。

## 最小構成

`lait.config.yml` の `default.model` が設定済みであれば、次の1ステップだけで動きます。

```yaml
# workflow.yml
steps:
  - prompt: "次の文章を3行で要約してください。\n{{ input }}"
```

```sh
lait run workflow.yml "要約したい文章"
```

ステップを並べると、前のステップの結果が次のステップの `{{ input }}` になります。

```yaml
steps:
  - id: summary
    prompt: "次の文章を3行で要約してください。\n{{ input }}"
  - prompt: "次の要約を英訳してください。\n{{ input }}"
  - write: summary.en.txt
```

## 設計の考え方

- **1ステップ = 1つの種類**: 各ステップは `prompt:`/`agent:`/`run:` などの **種類キーを
  ちょうど1つ** 持ち、そのキーでステップの種類が決まります。種類ごとに使えるフィールドが
  決まっており、他の種類のフィールドを書くと読み込み時にエラーになります。
- **値は型付き**: ステップ間を流れるのは JSON の値です。LLM のテキスト応答やコマンドの
  出力は文字列、構造化出力や jq の結果はその JSON のまま次のステップに渡ります。文字列が
  たまたま JSON に見えても勝手に解釈し直されることはありません。
- **入出力の宣言**: ワークフローが受け取る名前付きパラメータは `inputs:` で宣言し、型を
  JSON Schema で指定します。未宣言の入力や型違いは実行前にエラーになります。結果は
  `output:` で整形できます。
- **読み込み時の検証**: ステップの形、フィールドの組み合わせ、id の重複、スキーマ参照、
  jq・テンプレートの構文、`break`/`stop`/`ask`/`write` を置ける場所は、最初のステップを
  実行する前にすべて検査されます。

## ファイルの構成

| キー | 内容 |
| --- | --- |
| `version` | 任意。`2` のみ（省略時は 2）。`1` や `nodes:` を持つファイルは移行案内付きのエラーになります。 |
| `name` / `description` | 任意。実行時に進捗表示へ出力されます。 |
| `inputs` | 名前付き入力の宣言（[入力と出力](#入力と出力)）。 |
| `output` | ワークフローの結果を計算する jq 式（[入力と出力](#入力と出力)）。 |
| `timeout` | このファイル1回の実行にかけられる秒数の上限（[エラー処理](#エラー処理retry--timeout--on_error)）。 |
| `default` | LLM ステップ（`prompt`/`agent`）の既定値（[モデル設定](#モデル設定と既定値default--models)）。 |
| `models` | このファイルで使うモデルエイリアス（`lait.config.yml` の `models:` と同じ形式）。 |
| `schemas` | 名前付き JSON Schema（[スキーマ](#スキーマschemas)）。 |
| `agents` | インラインのエージェント定義（[`agent`](#agent--エージェント定義で呼び出す)）。 |
| `steps` | 実行するステップの列（必須、1つ以上）。 |

```yaml
version: 2
name: triage
description: 問い合わせを分類して返信を下書きする

inputs:
  tone: { type: string, default: 丁寧, description: 返信の口調 }

output: '{category: $steps.classify.category, reply: .}'

default:
  model: local

schemas:
  classification:
    type: object
    properties:
      category: { type: string, enum: [bug, question, other] }
    required: [category]
    additionalProperties: false

steps:
  - id: classify
    prompt: "次の問い合わせを分類してください。\n{{ input }}"
    output_schema: classification
  - prompt: |
      {{ inputs.tone }}な口調で、{{ steps.classify.category }} への返信を書いてください。
      問い合わせ: {{ steps.classify }}
```

## ステップ

### ステップの種類

| 種類キー | 分類 | 内容 |
| --- | --- | --- |
| [`prompt`](#prompt--llm-を呼び出す) | アクション | プロンプトテンプレートで LLM を呼び出す |
| [`agent`](#agent--エージェント定義で呼び出す) | アクション | エージェント定義（システムプロンプト・スキーマ・設定の組）で LLM を呼び出す |
| [`run`](#run--コマンドを実行する) | アクション | コマンドを実行する（シェルを経由しない） |
| [`decide`](#decide--jev-互換-api-で判定する) | アクション | Jev 互換 API に型付きの質問（yes/no・選択・スコア）を送り、確率つきの答えを得る |
| [`jq`](#jq--値を変換する) | アクション | jq 式で値を変換する |
| [`ask`](#ask--人間に尋ねる) | アクション | 端末で人間の回答を受け取る |
| [`write`](#write--ファイルに書き出す) | アクション | 値をファイルに書き出す（値はそのまま次へ） |
| [`workflow`](#workflow--別のワークフローを呼び出す) | アクション | 別のワークフローファイルを子として実行する |
| [`group`](#group--ステップをまとめる) | 制御 | ステップ列を1つのステップとしてまとめる |
| [`switch`](#switch--条件分岐) | 制御 | 条件に最初に一致したケースを実行する |
| [`parallel`](#parallel--並列実行) | 制御 | 複数のステップ列を同時に実行する |
| [`for_each`](#for_each--配列の各要素に対して実行) | 制御 | 配列の各要素に対してステップ列を実行する |
| [`while` / `until`](#while--until--条件ループ) | 制御 | 条件を満たす間／満たすまで繰り返す |
| [`stop` / `break`](#stop--break--早期終了) | 制御 | ワークフロー／ループを打ち切る |

種類キーが無い、または2つ以上あるステップは読み込み時にエラーになります。

### 共通フィールド

すべてのステップは種類キーに加えて次のフィールドを持てます（`stop`/`break` は
`retry`/`timeout`/`on_error` を除く）。

| フィールド | 内容 |
| --- | --- |
| `id` | このステップの値を `steps.<id>`（テンプレート）/`$steps.<id>`（jq）として記録します。ファイル内で一意である必要があります。 |
| `when` | jq の条件式。偽（`false`/`null`）ならステップを丸ごとスキップし、値はそのまま次へ渡ります。 |
| `input` | jq 式。ステップが処理する値を、流れてきた値から計算します（省略時は `.`）。 |
| `output` | jq 式。ステップの結果（`.`）を、このステップの値に変換します。 |
| `retry` | 失敗時の再試行（`max_attempts`/`delay_seconds`/`backoff`）。 |
| `timeout` | 1回の試行の秒数上限。 |
| `on_error` | 再試行を使い切っても失敗したときに代わりに実行するステップ列。 |

1つのステップは次の順で処理されます。

1. `when` を評価し、偽ならスキップ
2. `input` → 種類ごとの処理 → `output`（この3つがまとめて `retry`/`timeout` の対象）
3. 失敗したら `on_error` を実行し、その結果をステップの値にする
4. `id` があれば値を記録する

`id` は英字または `_` で始まり、英数字・`_`・`-` だけを含む名前です（`$steps` で `-` を含む
id を参照するときは `$steps["my-id"]` と書きます）。`id` を持たないステップの値は記録されず、
進捗表示では `step-<番号> (<種類>)` と表示されます。

```yaml
steps:
  - id: fetch
    run: [curl, -s, "https://example.com/api/items"]
    output: fromjson            # 文字列の JSON を値に変換
    retry: { max_attempts: 3, delay_seconds: 2, backoff: 2.0 }
    timeout: 30
  - when: '.items | length > 0'
    input: '.items[0]'          # 最初の要素だけを処理する
    prompt: "次の項目を説明してください: {{ input.title }}"
```

## 値とテンプレート・jq

### 値の流れ

- 最初のステップの入力は `PROMPT`（CLI 引数、または標準入力）で、**文字列** です。
  `inputs:` を宣言したワークフローは `PROMPT` を省略でき、その場合の最初の入力は `null` です。
- 各ステップの値が次のステップの入力になります。値の種類はステップによって決まります。

| ステップ | 値 |
| --- | --- |
| `prompt`/`agent`（`output_schema` なし） | 応答テキスト（文字列） |
| `prompt`/`agent`（`output_schema` あり） | 応答をパースした JSON（スキーマで検証済み） |
| `run` | 標準出力（文字列、末尾の改行1つを除去） |
| `decide` | 応答の `answers`（質問 id をキーにしたオブジェクト、宣言順） |
| `ask` | 回答（文字列） |
| `jq` | jq 式の結果 |
| `write` | 入力そのまま |
| `workflow` | 子ワークフローの結果 |
| `group`/`switch`/`while`/`until` | 最後に実行されたステップの値 |
| `parallel` | branch 名をキーにしたオブジェクト（宣言順） |
| `for_each` | 各要素の結果の配列（要素順） |

テキストが必要な場所（ユーザーメッセージ、コマンドの標準入力、`write` の内容、`lait run` の
最終出力）では、値は **テキスト形式** に変換されます。文字列はそのまま、それ以外はコンパクトな
JSON です。文字列の JSON を値として扱いたい場合は `output: fromjson` のように明示的に変換します。

### テンプレート（handlebars）

`prompt`/`system`/`ask`/`run` の各引数/`write` のパス/`files`/`images`、エージェントの
システムプロンプトは [handlebars](https://handlebarsjs.com/) テンプレートです。

| 変数 | 内容 |
| --- | --- |
| `{{ input }}` | このステップの入力 |
| `{{ steps.<id> }}` | 記録済みのステップの値 |
| `{{ inputs.<name> }}` | ワークフローの入力 |
| `{{ loop.index }}` / `{{ loop.item }}` | 最も内側の `for_each`（`index` と `item`）・`while`/`until`（`index`）の情報（0 始まり） |

- `{{ x }}` は値のテキスト形式を出力します。オブジェクトや配列はコンパクトな JSON に
  なります（`[object]` にはなりません）。`{{ json x }}` は常に JSON として出力します
  （文字列も引用符付き）。フィールドは `{{ input.city }}` のようにアクセスできます。
- 存在しない変数・フィールドを参照するとエラーになります（strict mode）。
- `{{#each}}`/`{{#if}}` などの handlebars の組み込みヘルパーも使えます。

### jq 式

`when`/`input`/`output`/`jq`/`switch` の `when`/`for_each`/`while`/`until`/`with`/
トップレベルの `output` は [jq](https://jqlang.org/) 式です。`.` が対象の値で、グローバル変数
`$steps`/`$inputs`/`$loop` がテンプレートの `steps`/`inputs`/`loop` に対応します。

- jq 式は **ちょうど1つの値** を返す必要があります。0個・複数個はエラーです（ストリームを
  まとめたいときは `[.items[]]` のように配列で囲みます）。
- 条件（`when` など）は jq の真偽判定に従い、`false`/`null` だけが偽です。
- 未記録の `$steps.x` や未宣言の `$inputs.x` は（jq の通常の挙動どおり）`null` になります。
  [`lait lint`](./lint.md) はこうした参照を検出します。

## 入力と出力

### `inputs:` と `--input`

`inputs:` は入力名から JSON Schema への対応です。`default` を持つ入力は省略可能、それ以外は
必須です。型名だけを書く短縮形（`text: string`）も使えます。

```yaml
inputs:
  text: string
  lang: { type: string, default: ja, description: 出力言語 }
  max_items: { type: integer, default: 5 }
  tags: { type: array, items: { type: string }, default: [] }
steps:
  - prompt: "{{ inputs.lang }} で {{ inputs.max_items }} 件にまとめてください: {{ inputs.text }}"
```

```sh
lait run summarize.yml --input text="$(cat article.md)" --input lang=en --input tags='["a","b"]'
```

- `--input KEY=VALUE` は繰り返し指定でき、同じキーは後の指定が勝ちます。
- `type: string` と宣言した入力は値をそのまま文字列として受け取ります（`--input id=0012` は
  `"0012"`）。それ以外の入力は、JSON として解釈できれば JSON の値として、できなければ文字列と
  して受け取ります。
- 値は宣言したスキーマで検証されます。宣言されていない名前を渡すとエラーになります。
- テンプレートからは `{{ inputs.<name> }}`、jq からは `$inputs.<name>` で参照します。
  子ワークフローには呼び出し側の入力は見えません（`with:` で明示的に渡します）。

### `output:`

トップレベルの `output:` は、最後のステップの値（`.`）・`$steps`・`$inputs` からワークフローの
結果を計算する jq 式です。省略すると最後の値がそのまま結果になります。`stop` で終了した場合も
適用されます。

```yaml
output: '{summary: $steps.summary, translated: .}'
```

## アクションステップ

### `prompt` — LLM を呼び出す

`prompt:` はユーザーメッセージのテンプレートです。

```yaml
steps:
  - id: extract
    prompt: "次の文章から都市名を抽出してください。\n{{ input }}"
    system: "あなたは正確な情報抽出器です。"
    model: cloud
    temperature: 0.2
    output_schema:
      type: object
      properties: { city: { type: string } }
      required: [city]
      additionalProperties: false
```

| フィールド | 内容 |
| --- | --- |
| `system` | システムプロンプトのテンプレート。省略時は `default.system`。 |
| `model` / `reasoning_effort` / `temperature` / `top_p` / `max_tokens` | モデルとサンプリング設定（[モデル設定](#モデル設定と既定値default--models)）。 |
| `mcp` / `max_tool_rounds` / `skills` / `subagents` / `tools` | ツール連携（[ツール連携](#ツール連携mcp--skills--subagents--tools)）。 |
| `files` | 内容を添付するテキストファイル（テンプレート）。ファイル名付きのコードブロックとしてユーザーメッセージの後ろに追記されます。 |
| `images` | 添付する画像のパスまたは `http(s)://` URL（テンプレート）。vision 対応モデル向けです。 |
| `input_schema` | 入力の検証に使うスキーマ（[スキーマ](#スキーマschemas)）。 |
| `output_schema` | Structured Outputs で要求するスキーマ。応答は JSON としてパースされ、スキーマで検証されます。 |
| `schema_name` | Structured Outputs のスキーマ名。省略時は `schemas:` の名前、それもなければ `structured_output`。 |

`files`/`images` のパスはコマンドを実行したカレントディレクトリからの相対パスです。

### `agent` — エージェント定義で呼び出す

`agent:` は、システムプロンプト・入出力スキーマ・モデル設定・ツールをまとめた **エージェント
定義** を使って LLM を呼び出します。ユーザーメッセージはステップの入力（テキスト形式）です。
エージェントのシステムプロンプトは入力を `{{ input }}` として（`steps`/`inputs`/`loop` も
参照可能）レンダリングされます。

`agent:` の値は次のいずれかです。

1. このファイルの `agents:` に定義した名前
2. エージェント Markdown ファイルへのパス（`/` を含むか `.md` で終わるもの。ワークフロー
   ファイルからの相対パス）
3. `lait.config.yml` の `agents:` に登録した名前

```yaml
agents:
  reviewer:
    model: cloud
    tools: [ripgrep]
    output_schema: { file: schemas/review.json }
    system: |
      あなたはコードレビュアーです。次の差分をレビューしてください。
      {{ input }}

steps:
  - id: review
    agent: reviewer
  - agent: ./agents/summarize.md      # ファイル
    temperature: 0.3                   # ステップ側で上書き
  - agent: researcher                  # lait.config.yml の agents: に登録済み
```

- `agents:` の各エントリは [エージェント Markdown ファイル](./agent.md) の frontmatter と
  同じフィールドに、本文に相当する `system:`（必須）を加えたものです。
- ステップ側に書けるのは `model`/サンプリング設定/ツール連携/`files`/`images` です。いずれも
  ステップ → エージェント定義 → ワークフローの `default:` → `lait.config.yml` の `default:` の
  順にフォールバックします。
- エージェントが `input_schema` を持つ場合、入力はそのスキーマで検証されます。`output_schema`
  を持つ場合、結果はパース済みの JSON になります。

### `run` — コマンドを実行する

`run:` は実行するプログラムと引数の配列です。**シェルを経由せず直接実行** するため、
テンプレートの展開結果が別の引数やコマンドとして解釈されることはありません。

```yaml
inputs:
  base: { type: string, default: main }
steps:
  - run: [git, diff, --stat, "{{ inputs.base }}"]
  - run: [jq, -r, .title]          # 入力は標準入力に渡されます
  - run: [sh, -c, 'wc -l | tr -d " "']
    output: tonumber
```

- 入力のテキスト形式が標準入力に渡され、標準出力（UTF-8、末尾の改行を1つ除去）が値に
  なります。JSON を出力するコマンドは `output: fromjson` で値に変換できます。
- 終了コードが 0 以外なら失敗です（標準エラー出力がエラーメッセージに含まれます）。
- 標準出力・標準エラーはそれぞれ最大 16 MiB まで保持します。超えるとコマンドを停止して
  失敗にします。
- プログラムのパスはカレントディレクトリ基準（または `PATH` から検索）です。

`env`/`cwd` で、コマンドから見える環境変数と作業ディレクトリを制限できます。意味は
[カスタムシェルツール](./tools.md)の `tools:` の同名フィールドと同じです。

| フィールド | 説明 |
| --- | --- |
| `env`（任意） | 子プロセスに渡す環境変数の**許可リスト**。1件以上指定すると、コマンドはここに列挙した変数**だけ**を見ます（lait 自身が継承している API キーなどは引き継がれません）。省略時は lait の環境変数をそのまま継承します。値は `${VAR_NAME}` で環境変数を参照できます（テンプレート `{{ }}` は展開しません）。 |
| `cwd`（任意） | コマンドの作業ディレクトリ。省略時は lait のカレントディレクトリを継承します。`${VAR_NAME}` 展開に対応します（テンプレートは展開しません）。 |

```yaml
steps:
  - run: [gh, pr, list, --json, title]
    env:
      PATH: "${PATH}"            # command[0] の名前解決に必要
      GH_TOKEN: "${GH_TOKEN}"
    cwd: "${HOME}/src/project"
```

`env` を1件でも指定すると完全な許可リストになるため、プログラムを `PATH` から探す場合は
`PATH` も含めてください。参照した環境変数が未定義の場合、そのステップはエラーになります。

### `decide` — Jev 互換 API で判定する

`decide:` は、入力を `state` として [Jev 互換 API](./jev.md)（`lait.config.yml` の `jev:` で
設定）に送り、型付きの質問への答えを値にします。文章は生成しません。

```yaml
steps:
  - id: triage
    decide:
      urgent: { type: noul, instructions: 今日中に対応が必要か }
      team:
        type: choice
        criteria: { billing: 支払い・請求書, sales: null }
  - switch:
      - when: .urgent.noul > 0.8
        steps:
          - jq: '"escalate to " + $steps.triage.team.choice'
    else:
      - jq: '"queue"'
```

- 質問の型は `noul`（はい/いいえの確率）・`choice`（選択肢）・`score`（順序つきレベル）です。
  質問の書き方、`model:` フィールド、応答の形は [Jev 互換 API で判定する](./jev.md) を参照して
  ください。
- `default.retry`/`default.timeout` が適用されます。`--replay` 中は実行できません。

### `jq` — 値を変換する

```yaml
steps:
  - jq: '{title: .title, tags: [.tags[] | ascii_downcase]}'
```

モデルを呼び出さずに値を変換します。結果の値が次のステップに渡ります。

### `ask` — 人間に尋ねる

`ask:` は質問のテンプレートです。回答（文字列）がステップの値になります。

```yaml
steps:
  - id: draft
    prompt: "次の依頼への返信を下書きしてください: {{ input }}"
  - id: confirm
    ask: "この下書きで送信しますか?\n{{ steps.draft }}"
    choices: ["yes", "no"]
    default: "no"
  - when: '. == "no"'
    stop: true
```

| フィールド | 内容 |
| --- | --- |
| `choices` | 回答をこのいずれかと完全一致に制限します。一致しなければエラーです（再入力は求めません）。 |
| `default` | 標準入力が端末でないとき（CI・パイプ実行など）に使う回答。`choices` があればそのいずれかである必要があります。 |
| `multiline` | `true` なら1行ではなく EOF まで読み取ります。 |

- 標準入力が端末のときは質問を標準エラー出力に表示して1行（末尾の改行を除く）を読みます。
- 標準入力が端末でないときは **何も読まず**、`default` があればそれを、なければエラーにします。
- `parallel` の branch や並行 `for_each` の中（子ワークフローを含む）では使えません。

### `write` — ファイルに書き出す

`write:` は書き出し先のパスのテンプレートです。入力のテキスト形式をファイルに書き、
**入力をそのまま** 次のステップへ渡します。

```yaml
inputs:
  name: string
steps:
  - id: summary
    prompt: "要約してください: {{ input }}"
  - write: "out/{{ inputs.name }}.md"
  - prompt: "英訳してください: {{ input }}"   # 要約がそのまま渡る
```

- パスはカレントディレクトリからの相対パスです。既存ファイルは上書きし、親ディレクトリは
  作成しません。
- 並行 `for_each` の中では、固定のパス（テンプレートを含まないパス）には書けません。要素ごとに
  異なるパス（例: `out/{{ loop.index }}.md`）にしてください。

### `workflow` — 別のワークフローを呼び出す

`workflow:` は別のワークフローファイルを子として実行し、その結果（子の `output`）をこの
ステップの値にします。

```yaml
inputs:
  lang: { type: string, default: ja }
steps:
  - id: summary
    workflow: ./shared/summarize.yml
    with: '{lang: $inputs.lang, max_lines: 3}'
```

```yaml
# shared/summarize.yml
inputs:
  lang: string
  max_lines: { type: integer, default: 5 }
output: '{summary: .}'
steps:
  - prompt: "{{ inputs.lang }} で {{ inputs.max_lines }} 行に要約: {{ input }}"
```

- `workflow:` の値は、ワークフローファイルへのパス（`/` を含むか `.yml`/`.yaml` で終わるもの。
  呼び出し側ファイルからの相対パス）か、`lait.config.yml` の `workflows:` に登録した名前です。
- 子の最初の入力は、このステップの入力です。
- `with:` は子の `inputs:` に渡すオブジェクトを計算する jq 式です。子の入力として宣言され、
  スキーマで検証されます。
- 子の `default:`/`models:` は呼び出し側の値より優先され、子に無い項目は呼び出し側の値に
  フォールバックします。`schemas:`/`agents:`/`steps` の id はファイルごとに独立しています。
- 子の中の `stop` は子の実行だけを終了します。子の `timeout:` は子の実行時間を制限します。
- 循環呼び出しはエラーになり、ネストの深さにも上限があります。

## 制御ステップ

### `group` — ステップをまとめる

`group:` はステップ列を1つのステップとして実行します。複数のステップにまとめて `when`/
`retry`/`timeout`/`on_error` を適用したいときに使います。

```yaml
inputs:
  url: string
steps:
  - group:
      - id: fetch
        run: [curl, -sf, "{{ inputs.url }}"]
      - prompt: "要約してください: {{ input }}"
    retry: { max_attempts: 2 }
    on_error:
      - jq: '"取得に失敗しました: \(.error)"'
```

### `switch` — 条件分岐

`switch:` はケースの配列です。上から順に `when` を評価し、最初に真になったケースの `steps`
を実行します。どれも一致しなければ `else:` を実行し、`else:` が無ければエラーになります。

```yaml
steps:
  - id: triage
    prompt: "緊急度を判定してください: {{ input }}"
    output_schema: { type: object, properties: { severity: { type: string } }, required: [severity] }
  - switch:
      - when: '.severity == "high"'
        steps:
          - prompt: "緊急対応メモを書いてください: {{ input }}"
      - when: '.severity == "medium"'
        steps:
          - prompt: "返信文を下書きしてください: {{ input }}"
    else:
      - jq: '"対応不要"'
```

### `parallel` — 並列実行

`parallel:` は branch 名からステップ列への対応です。すべての branch が同じ入力を受け取って
同時に実行され、結果は branch 名をキーにしたオブジェクト（宣言順）になります。

```yaml
steps:
  - id: analysis
    parallel:
      sentiment:
        - prompt: "感情を一言で: {{ input }}"
      keywords:
        - prompt: "キーワードを3つ: {{ input }}"
    output: '"感情: \(.sentiment) / キーワード: \(.keywords)"'
```

- どれか1つの branch が失敗すると `parallel` 全体が失敗します。
- branch の中で記録した `steps.<id>` はその branch の中でだけ有効です。branch の外へは
  結果のオブジェクトを通して渡します。
- branch の中で `stop` は使えません。`break` は branch の中のループに対してだけ使えます。
  `ask` は使えません。
- 進捗表示は `[branch 名] [番号] ラベル` の形式で行が入り交じります。

### `for_each` — 配列の各要素に対して実行

`for_each:` は配列を返す jq 式です。各要素を入力として `steps` を実行し、結果を配列
（要素の順）にします。

```yaml
steps:
  - for_each: '.items'
    max_concurrency: 4
    steps:
      - prompt: "{{ loop.index }} 番目の項目を要約: {{ input }}"
      - write: "out/item-{{ loop.index }}.md"
    output: 'map(select(. != ""))'
```

- `for_each` の式は配列を1つ返す必要があります（`.items[]` のようなストリームではなく
  `.items`）。空配列なら0回実行され、結果は空配列です。
- 本体では `{{ loop.item }}`/`$loop.item`（要素）と `{{ loop.index }}`/`$loop.index`
  （0 始まりの位置）が使えます。
- `max_concurrency`（既定 1）が2以上なら要素を並行に処理します。その場合、本体は `parallel`
  の branch と同じ扱いで、`steps.<id>` は要素ごとに独立し、`stop`/`break`/`ask` は使えず、
  `write` はパスにテンプレートを含む必要があります。逐次実行（既定）では記録した
  `steps.<id>` は要素をまたいで引き継がれ、`break` で打ち切れます。

### `while` / `until` — 条件ループ

`while:`/`until:` は条件の jq 式で、`steps`（本体）と `max_iterations`（必須、1以上）を
持ちます。各反復の結果が次の反復の入力になります。

```yaml
schemas:
  validation_result:
    type: object
    properties:
      valid: { type: boolean }
      text: { type: string }
    required: [valid, text]
steps:
  - until: '.valid == true'
    max_iterations: 5
    steps:
      - prompt: "前回の結果を見直して修正し、妥当かどうかも判定してください: {{ input }}"
        output_schema: validation_result
```

- `while` は各反復の **前** に評価し、偽なら終了します（0回もありえます）。
- `until` は各反復の **後** に評価し、真になったら終了します（必ず1回以上実行されます）。
- `max_iterations` に達しても条件を満たさない場合は **エラー** です（黙って打ち切りません）。
- 本体では `{{ loop.index }}`/`$loop.index`（0 始まりの反復回数）が使えます。

### `stop` / `break` — 早期終了

`stop: true` はワークフロー（このファイル）を、その時点の値を結果として正常終了します。
`break: true` は最も内側の `for_each`/`while`/`until` を、その時点の値で打ち切ります。
どちらも `when` と組み合わせて使うのが基本で、`output` で値を整形することもできます。

```yaml
schemas:
  judgement:
    type: object
    properties:
      sufficient: { type: boolean }
      answer: { type: string }
    required: [sufficient, answer]
steps:
  - id: check
    prompt: "情報が十分か判定し、十分なら回答してください: {{ input }}"
    output_schema: judgement
  - when: '.sufficient'
    output: '.answer'
    stop: true
  - prompt: "不足している情報を尋ねる質問を1つ作ってください: {{ input }}"
```

- `break` はループの本体（`switch`/`group`/`on_error` を経由してもよい）の中でだけ使えます。
  `parallel` の branch や並行 `for_each` をまたぐことはできません。
- `stop` は `parallel` の branch や並行 `for_each` の中では使えません。
- `false` は指定できません（`stop: true`/`break: true` のみ）。

## スキーマ（`schemas:`）

JSON Schema は次の3通りで指定できます。

| 書き方 | 意味 |
| --- | --- |
| `output_schema: city` | `schemas:` に定義した名前 |
| `output_schema: { type: object, ... }` | スキーマ本体を直接記述 |
| `output_schema: { file: schemas/city.json }` | JSON ファイル（このワークフローファイルからの相対パス） |

```yaml
schemas:
  city:
    type: object
    properties: { city: { type: string } }
    required: [city]
    additionalProperties: false
  weather: { file: schemas/weather.json }
```

- `schemas:` に無い名前を参照すると読み込み時にエラーになります。
- `{file: ...}` 以外のオブジェクトはスキーマ本体として扱います。
- 入力・出力の検証では `type`（型の配列を含む）・`enum`・`required` を、`properties`/`items`
  を通して再帰的に確認します。`format`・`pattern`・数値の範囲・`additionalProperties`・
  `oneOf`/`anyOf`/`allOf`・`$ref` は検証しません（Structured Outputs の制約はサーバー側で
  適用されます）。

## モデル設定と既定値（`default:` / `models:`）

`default:` は LLM ステップ（`prompt`/`agent`）の既定値です。

```yaml
default:
  model: local
  reasoning_effort: medium
  temperature: 0.7
  system: "あなたは日本語で簡潔に答えるアシスタントです。"
  mcp: [filesystem]
  retry: { max_attempts: 3, delay_seconds: 1 }
  timeout: 60
```

| キー | 内容 |
| --- | --- |
| `model` / `reasoning_effort` / `temperature` / `top_p` / `max_tokens` | モデルとサンプリング設定 |
| `system` | `prompt` ステップの既定のシステムプロンプト |
| `mcp` / `max_tool_rounds` / `skills` / `subagents` / `tools` | ツール連携 |
| `retry` / `timeout` | モデルを呼び出すステップ（`prompt`/`agent`/`decide`）の既定の再試行・タイムアウト |

- 各項目はそれぞれ独立して、ステップ → （`agent` ならエージェント定義）→ ワークフローの
  `default:` → `lait.config.yml` の `default:` の順にフォールバックします（`system`/`retry`/
  `timeout` は `lait.config.yml` にはフォールバックしません）。
- `retry` はフィールドごとではなく、まとまり全体でフォールバックします。
- `temperature`（0.0〜2.0）・`top_p`（0.0〜1.0）・`max_tokens`（1以上）の範囲外の値は
  読み込み時にエラーです。

`models:` は `lait.config.yml` の `models:` と同じ形式で、ワークフロー内のエイリアスが同名の
設定ファイル側のエイリアスより優先されます。`provider.api_key`/`provider.base_url` には
`${VAR_NAME}` で環境変数を埋め込めます（[設定ファイル](./config.md#var_name-による環境変数参照)）。

```yaml
models:
  local:
    - provider: { base_url: http://localhost:1234/v1 }
      model_id: local-model
  cloud:
    - provider:
        base_url: https://api.example.com/v1
        api_key: ${CLOUD_API_KEY}
      model_id: cloud-model
```

## ツール連携（`mcp` / `skills` / `subagents` / `tools`）

LLM ステップ・エージェント定義・`default:` には、`lait.config.yml` に登録した名前で
ツールを渡せます。モデルがツール呼び出しを返すたびに lait が実行して結果を返す、という
やり取りを最大 `max_tool_rounds` 回（既定 8）まで繰り返します。

| キー | 内容 | 詳細 |
| --- | --- | --- |
| `mcp` | MCP サーバーのツール（`<サーバー名>__<ツール名>`） | [MCP](./mcp.md) |
| `skills` | システムプロンプトの末尾に追記するスキル | [スキル](./skills.md) |
| `subagents` | エージェントをツールとして呼び出す（`agent__<名前>`） | [サブエージェント](./subagents.md) |
| `tools` | カスタムシェルツール（`tool__<名前>`） | [カスタムシェルツール](./tools.md) |

```yaml
steps:
  - prompt: "{{ input }} について調べて要約してください。"
    mcp: [filesystem]
    subagents: [researcher]
    max_tool_rounds: 12
```

- `output_schema` と併用できます。ツールを呼び出している間は `response_format` を送らず、
  最後のラウンドだけ付けて再送します。
- `--stream` とも併用できます。
- ツール呼び出しはステップの `retry` の単位に含まれます（再試行すると副作用のある
  ツール呼び出しもやり直されます）。

## エラー処理（`retry` / `timeout` / `on_error`）

```yaml
steps:
  - id: call
    prompt: "{{ input }}"
    timeout: 30
    retry:
      max_attempts: 3      # 初回を含む総試行回数（必須、1以上）
      delay_seconds: 1     # 最初の再試行までの待機秒数（既定 0）
      backoff: 2.0         # 再試行のたびに待機時間に掛ける倍率（既定 1.0）
    on_error:
      - jq: '{fallback: true, reason: .error}'
```

- `retry`/`timeout` は `input` → 処理 → `output` をまとめた1回の試行に対して働きます。
  タイムアウトした試行も失敗として再試行の対象になります。待機時間は最大1時間です。
- `on_error` は、再試行を使い切っても失敗したときに、ワークフローを失敗させる代わりに実行
  されます。入力は `{"error": "<失敗内容と原因のチェーン>", "input": <このステップに流れてきた値>}`
  で、`on_error` の結果がこのステップの値になります（`id` があれば記録されます）。
- 制御ステップ（`group`/`switch`/`parallel`/`for_each`/`while`/`until`）にも `retry`/
  `timeout`/`on_error` を指定できます。本体全体が1つの単位として扱われます。
- `default.retry`/`default.timeout` はモデルを呼び出すステップ（`prompt`/`agent`/`decide`）にだけ適用されます。
- 実行全体（`lait run` の中断や上位の `timeout`）がキャンセルされた場合は `on_error` は
  実行されません。

### ワークフロー全体のタイムアウト（`timeout:`）

トップレベルの `timeout:` は、このファイル1回の実行にかけられる秒数の上限です。超過すると
実行中のステップがキャンセルされ、エラーで終了します（`--checkpoint` を付けていれば、その
時点までの状態が保存されます）。子ワークフローとして実行される場合も、その子の実行時間に
適用されます。

## パスの解決

| 対象 | 基準 |
| --- | --- |
| `agent:` のファイルパス、`workflow:` のファイルパス、`{file: ...}` のスキーマ（`schemas:`・ステップ・`agents:`） | そのワークフローファイルのディレクトリ |
| エージェント Markdown ファイル内の `{file: ...}` のスキーマ | そのエージェントファイルのディレクトリ |
| `files`/`images`/`write`、`run` のプログラム | コマンドを実行したカレントディレクトリ |

ワークフローを構成するファイル（エージェント・子ワークフロー・スキーマ）はワークフローと
一緒に置けば、どこから実行しても同じように解決されます。実行時のデータ（添付・出力先）は
実行した場所が基準です。

## 並行実行の制約

`parallel` の branch と `max_concurrency` が2以上の `for_each` の本体は並行に実行される
ため、次の制約があります。いずれも読み込み時（子ワークフローはその読み込み時）に検査されます。

| 対象 | 制約 |
| --- | --- |
| `stop` | 使えません |
| `break` | 本体の中のループに対してだけ使えます |
| `ask` | 使えません（子ワークフローの中も含む） |
| `write` | 並行 `for_each` の中では、テンプレートを含まない固定のパスに書けません（子ワークフローの中も含む） |
| `steps.<id>` | 並行に実行される単位ごとに独立し、外には伝わりません |

## 実行計画の表示（`lait run --dry-run`）

`--dry-run` を付けると、モデルの呼び出し・MCP サーバーの起動・コマンドの実行を一切行わずに、
実行計画を標準出力に表示します。

```sh
lait run workflow.yml "本文" --input lang=en --dry-run
```

- 各ステップの順序と種類、LLM ステップの解決後のモデル ID と `base_url`、実効の `retry`/
  `timeout`、制御構造の中身を表示します。モデルが解決できない場合は実行時と同じエラーになります。
- テンプレートは値が分かっている範囲（`inputs` と、最初のステップの `input`）でレンダリング
  され、それ以外は `[unrendered: ...]` 付きの原文で表示されます。
- 子ワークフローは展開されません（その子に対して `--dry-run` を実行してください）。

## グラフ出力（`lait graph`）

`lait graph <FILE>` はステップの流れを Mermaid（既定）または DOT（`--format dot`）の
グラフとして出力します。

```sh
lait graph workflow.yml
lait graph workflow.yml --format dot | dot -Tsvg > workflow.svg
```

アクションは矩形、制御ステップは菱形、`stop`/`break` は端点として描かれ、`switch` の各
ケースには条件が、ループ本体・branch・`on_error` にはサブグラフが付きます。子ワークフローは
展開されません。

## 実行の再開（`lait run --checkpoint` / `--resume`）

`--checkpoint` を付けると、トップレベルのステップが完了するたびに状態（現在の値・記録済みの
`steps`・`inputs`）を `.lait/runs/<run-id>.json` に保存します。失敗した実行は `--resume` で
最後に完了したトップレベルステップの直後から再開できます。

```sh
lait run workflow.yml "本文" --checkpoint
#   note: run checkpointed as '20260903-141233-123456789-12345-0'; resume with `lait run workflow.yml --resume 20260903-141233-123456789-12345-0`

lait run workflow.yml --resume 20260903-141233-123456789-12345-0
lait runs list
lait runs show 20260903-141233-123456789-12345-0
```

- 再開の単位はトップレベルのステップです。制御ステップは中断した場合、最初からやり直されます
  （中の `run`/`write` などの副作用が二重に起きることがあります）。
- `--resume` では `FILE` は保存時と同じである必要があり、`PROMPT` は使われません。
  `--input` を指定しなければ記録された `inputs` を使い、指定すればその値で進めて記録も
  上書きします。チェックポイントには入力値がそのまま保存されるので、機密情報の扱いに注意して
  ください（`.gitignore` には `.lait/` が含まれています）。
- 完了済みのステップ列（`id`、無ければ `step-<位置>`）が現在のファイルと異なる場合は再開を
  拒否します。未実行の範囲だけが変わっている場合は警告して再開します。
- `--resume` は `--checkpoint` を含意します。正常に完了した実行は `status: completed` として
  残り、再開はできません。
- 以前の形式（バージョン 1 のワークフロー用）のチェックポイントは再開できません。

## そのほか

- `<FILE>` はパスの代わりに `lait.config.yml` の `workflows:` に登録した名前でも指定できます
  （[設定ファイル](./config.md#ワークフローの登録と一覧表示)）。
- 通常は最終結果のテキスト形式だけを標準出力に出します。`--output` でファイルに、`--json`
  で CLI 用の JSON として出力します。進捗は標準エラー出力に出ます。
- 設定・ワークフロー・子ワークフロー・エージェント・スキーマの読み込み中も Ctrl-C で中断
  できます。実行時に読み込む各ファイルは最大 16 MiB です。
- `lait lint` は、未宣言の `inputs.*` 参照・存在しない `steps.*` 参照・読み込めない
  スキーマ/エージェント/子ワークフロー・未登録のツール名・使われていない `schemas:`/`agents:`
  を報告します（[lint](./lint.md)）。

## バージョン 1 からの移行

バージョン 2 は互換性のない変更です。バージョン 1 のファイル（`nodes:` を持つもの、または
`version: 1`）は読み込み時に移行を促すエラーになります。主な対応は次のとおりです。

| バージョン 1 | バージョン 2 |
| --- | --- |
| `nodes:` に定義して `steps:` から `use:` | ステップに直接書く。再利用したい LLM 設定は `agents:` に、処理のまとまりは子ワークフローに |
| `type: prompt` / `prompt:` / `system_prompt:` | `prompt:` / `system:` |
| `system_prompt` だけのノード（入力をそのまま送る） | `prompt: "{{ input }}"` と `system:` |
| `type: agent` / `agent:`（カレントディレクトリ基準） | `agent:`（ワークフローファイル基準、または `agents:`/設定ファイルの名前） |
| `type: command` / `command:` | `run:` |
| `type: transform` / `jq:` | `jq:` |
| ノードの `jq:`（出力の加工） | ステップの `output:` |
| ノードの `write_file:` | 後続の `- write: <パス>` ステップ（パスはテンプレート可） |
| `type: ask` / `prompt:` | `ask:` |
| `type: workflow` / `workflow:` | `workflow:`（`with:` で入力を渡せる） |
| `json_schemas:` の `{schema: ...}` / `{file_path: ...}` | `schemas:` にスキーマ本体 / `{file: ...}` |
| `output_schema: <パス>`（名前が無ければパス扱い） | `output_schema: {file: <パス>}`（文字列は常に `schemas:` の名前） |
| エージェントの `structured_output: true` + `output_schema` | `output_schema` のみ |
| `switch: {cases: [{id, when, steps}], else}` | `switch: [{when, steps}]` と `else:` |
| `parallel: {branches: [{id, steps}], join}` | `parallel: {<名前>: [...]}` と `output:` |
| `loop: {while/until, max_iterations, steps}` | `while:`/`until:` と `max_iterations:`・`steps:` |
| `for_each: {items, steps, join, max_concurrency}` | `for_each: <式>` と `steps:`・`max_concurrency:`・`output:` |
| `on_error: {steps: [...]}` | `on_error: [...]` |
| `use:` と `stop: true` を同じステップに | アクションの後に `- stop: true`（条件付きなら `group` でまとめる） |
| `default.system_prompt` / `default.workflow_timeout` | `default.system` / トップレベルの `timeout:` |
| `lait run --var KEY=VALUE` / `{{ vars.x }}` / `$vars.x` | `inputs:` で宣言して `--input KEY=VALUE` / `{{ inputs.x }}` / `$inputs.x` |
| 出力は常に文字列で、次の入力で JSON として再解釈 | 型付きの値。文字列の JSON は `output: fromjson` で変換 |
| jq の複数出力は改行で連結 | 1つの値のみ。`[...]` で配列にまとめる |
| `{{ input }}` にオブジェクトを渡すとエラー | コンパクトな JSON として出力 |
| ノードを使わないステップの `id` は省略時ノード id | `id` を書いたステップだけが記録される |

書き換えの例です。

```yaml
# バージョン 1
json_schemas:
  city:
    schema: { type: object, required: [city] }
nodes:
  extract:
    type: prompt
    prompt: "都市名を抽出: {{ input }}"
    output_schema: city
    jq: '.city'
    write_file: city.txt
steps:
  - id: extract
    use: extract
  - loop:
      until: '. != ""'
      max_iterations: 3
      steps:
        - use: extract
```

```yaml
# バージョン 2
schemas:
  city: { type: object, required: [city] }
steps:
  - id: extract
    prompt: "都市名を抽出: {{ input }}"
    output_schema: city
    output: '.city'
  - write: city.txt
  - until: '. != ""'
    max_iterations: 3
    steps:
      - prompt: "都市名を抽出: {{ input }}"
        output_schema: city
        output: '.city'
```
