---
title: "ワークフロー（workflow.yml）"
description: "nodes/steps の分離とノードの再利用、モデル定義、JSON 出力と jq による加工、write_file、MCP ツールの利用、system_prompt、スキルの利用、サブエージェントの利用、steps 参照、retry/timeout/on_error、when/switch、parallel、loop、for_each、stop/break、サブワークフロー呼び出し"
---

`lait run <FILE> <PROMPT>` サブコマンドで、複数の LLM 呼び出しを YAML で逐次実行できます。
ワークフローファイルは **`nodes:`**（何をするか。プロンプトやモデル呼び出し、jq によるデータ
変換などの定義）と **`steps:`**（どう繋ぐか。実行順序と分岐・ループなどの制御）の2つに分かれます。
各ノードは前のステップの応答テキストを `{{ input }}` プレースホルダーで受け取り、次のステップの
プロンプトに埋め込みます。最初のステップの `{{ input }}` には `<PROMPT>`（CLI 引数）が使われます。
`prompt` はエージェント Markdown ファイルのシステムプロンプトと同じテンプレートエンジン
（[handlebars](https://handlebarsjs.com/)）でレンダリングされるため、入力が JSON オブジェクト/
配列のときは裸の `{{ input }}` は使えません。`{{ input.field }}` でフィールドにアクセスするか、
`{{ json input }}` で全体をコンパクトな JSON テキストとして展開してください（詳細は後述）。

```yaml
# workflow.yml
name: example-flow
description: 要約 → 翻訳 → 整形

# ワークフロー全体の既定値。省略時は lait.config.yml の default: にフォールバック
default:
  model: local
  reasoning_effort: medium

# nodes: --- 何をするか（定義。順序は持たない）
nodes:
  summarize:
    prompt: |
      次の文章を3行で要約してください。
      {{ input }}

  translate:
    model: cloud          # ノードごとに上書き可能
    prompt: |
      次の要約を英訳してください。
      {{ input }}

  format:
    prompt: |
      次の英訳を Markdown の箇条書きにしてください。
      {{ input }}

# steps: --- どう繋ぐか（制御。ここが実行順序）
steps:
  - use: summarize
  - use: translate
  - use: format
```

```sh
cargo run -- run workflow.yml "要約・翻訳したい文章..."
```

- `nodes:` はキーをノード id とするマップで、順序を持ちません。各ノードは `prompt`/
  `system_prompt`/`agent`/`workflow`/`command`/`files`/`images`/`jq`/`model`/`reasoning_effort`/
  `temperature`/`top_p`/`max_tokens`/`input_schema`/`output_schema`/`schema_name`/`write_file`/
  `retry`/`timeout` を持てます（後述）。
- `steps:` は配列で、各要素（**ステップ**、または**参照サイト**）が `use: <ノードid>` で
  `nodes:` のどれを実行するかを指定します。配列の先頭から逐次実行するのが基本です。
  `when`/`switch` による分岐（後述）、`parallel` による複数のステップ列の同時実行
  （ファンアウト/ファンイン）、`loop` による条件ループ、`for_each` による配列反復も可能です
  （いずれも後述）。
- 同じノードを複数のステップから `use:` できます。書き直したりコピーしたりする必要はありません
  （詳しくは「[ノードの再利用](#ノードの再利用)」を参照）。
- `model` / `reasoning_effort` / `temperature` / `top_p` / `max_tokens` はノード単位で省略可能。
  省略時はそれぞれ独立して、ワークフロー直下の `default:` → `lait.config.yml` の `default:`、
  の順にフォールバックします。`temperature`（`0.0`〜`2.0`）・`top_p`（`0.0`〜`1.0`）・
  `max_tokens`（`1`以上）は CLI の `--temperature`/`--top-p`/`--max-tokens` と同じサンプリング
  パラメータで、範囲外の値は実行前にエラーになります。
- ステップの `id`（省略可）は進捗表示（標準エラー出力）用のラベルであり、`{{ steps.<id> }}`/
  `$steps` に記録されるキーでもあります。省略した場合は `use:` したノードの id が使われ、
  それも無い場合（`switch`/`parallel`/`loop`/`for_each` に `id` を付けなかった場合）は
  `step-1`、`step-2`… になります。
- `prompt` も `system_prompt` も `agent` も `workflow` も `command` も持たないノードはモデルを
  呼び出さず、`jq` によるデータ変換のみを行います（後述）。この場合 `model` は不要です。
  `command`（後述）を持つノードもモデルを呼び出さず、代わりに任意のコマンドを実行します。
- 最後のステップの出力のみを標準出力に出します。
- `run` サブコマンドでも `--no-config` は利用できます（例: `lait run workflow.yml "..." --no-config`）。

## ノードとステップの分離

`nodes:` と `steps:` を分ける理由は2つあります。

1. **読みやすさ**: 長いプロンプトが `switch`/`loop` などの制御構造の入れ子に直接埋め込まれると、
   フロー全体の形が読み取りにくくなります。`nodes:` にアクションの中身を出し、`steps:` 側は
   `use: <id>` の一覧にすることで、フローの骨格だけを見通せます。
2. **再利用**: 同じ処理を複数の分岐・ループから使う場合、以前はコピー＆ペーストするしかありません
   でしたが、`nodes:` に一度書けば複数のステップから参照できます。

### ノードの再利用

`nodes:` の1つのエントリは、`steps:` 側の複数の場所から `use:` できます。

```yaml
nodes:
  ask-clarifying-question:
    prompt: "不足している情報を尋ねる質問を1つ生成してください: {{ input }}"
  answer:
    prompt: "十分な情報があるので回答してください: {{ input }}"

steps:
  - switch:
      cases:
        - when: '.confidence < 0.5'
          steps:
            - use: ask-clarifying-question
      else:
        - use: answer   # 別のノード
  - loop:
      until: '.sufficient == true'
      max_iterations: 3
      steps:
        - use: ask-clarifying-question   # 同じノードをループ本体からも再利用
```

- `{{ steps.<id> }}`/`$steps` に記録されるキーは、そのステップ自身の `id`（省略時は `use:` した
  ノードの id）です。同じノードを別々のステップから使う場合、区別したければステップごとに
  異なる `id:` を付けてください。同じキーで複数回記録された場合は、最後に実行された結果で
  上書きされます（`loop`/`for_each` の本体で同じノードが繰り返し実行される場合と同じ挙動です）。
- ステップの `id:` は、それが `use:` していない別のノードの id と同じにはできません
  （`{{ steps.<id> }}` のキーが衝突してあいまいになるため、パース時にエラーになります）。
- ノードは `write_file:` を持てますが、`for_each` の `max_concurrency` が2以上の本体から
  `use:` することはできません（複数の要素が同時にそのノードを実行すると、同じパスへ競合して
  書き込んでしまうためです。詳しくは後述の[ファイルへの出力](#ファイルへの出力write_file)を
  参照）。
- `nodes:` に定義したが `steps:` のどこからも `use:` されていないノードがあってもエラーには
  なりません（ノードのライブラリとして定義だけ用意しておくような使い方も可能です）。
- `nodes:` はワークフローファイルごとに閉じたスコープです。`workflow:` ノードで別のワークフロー
  ファイルを呼び出しても、呼び出し先の `use:` が呼び出し元の `nodes:` を見に行くことはありません
  （`models:`/`json_schemas:` とは異なり、継承・マージされません）。

## ワークフロー内でのモデル定義

`workflow.yml` にも `lait.config.yml` と同じ形式の `models` を書けます。`default.model` /
`nodes[].model` で参照するエイリアスをワークフローファイル内に閉じて定義でき、
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
      default_temperature: 0.7
      default_max_tokens: 512

default:
  model: local

nodes:
  summarize:
    prompt: "次の文章を要約してください。\n{{ input }}"
  translate:
    model: cloud
    prompt: "次の要約を英訳してください。\n{{ input }}"

steps:
  - use: summarize
  - use: translate
```

同じ名前のエイリアスがワークフローと `lait.config.yml` の両方にある場合は、ワークフロー内の
定義が優先されます。ワークフローに定義がないエイリアスは、これまでどおり `lait.config.yml`
の `models` から解決されます。

`provider.api_key`/`provider.base_url` には `${VAR_NAME}` で環境変数を埋め込めます（[設定ファイル
の該当節](/lait/docs/config/#var_name-による環境変数参照)を参照）。API キーをワークフローファイルに平文で
書かずに済みます。

## JSON 出力の指定と jq による加工

ノードに `output_schema`（と任意で `schema_name`）を指定すると、CLI の `--json-schema` /
`--schema-name` と同じく Structured Outputs を要求し、モデルの応答を JSON にできます。
さらに `jq` を指定すると、そのノードの出力（モデルの応答、または `prompt` を省略した場合は
そのときの `{{ input }}`）に [jq](https://jqlang.org/) フィルターを適用し、その結果が次の
ステップの `{{ input }}` になります。

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
nodes:
  extract:
    prompt: |
      次の文章から都市名と人口を JSON で抽出してください。
      {{ input }}
    output_schema: city_fact
    schema_name: city_fact
    jq: ".city"

  introduce:
    prompt: |
      次の都市名を使って一文で紹介してください。
      {{ input }}
steps:
  - id: extract
    use: extract
  - use: introduce
```

- `schema:` はスキーマ本体を直接書くので、外部ファイルを用意せずワークフロー単体で完結
  させたり、複数のノードから同じスキーマを名前で参照したりできます。
- `file_path:` は（`json_schemas:` を使わない場合の `output_schema: city.schema.json` と
  同じく）JSON Schema ファイルへのパスです。
- `json_schemas:` を使わず、これまでどおり `output_schema: city.schema.json` のように
  直接ファイルパスを指定することもできます（`json_schemas:` に同名のキーがある場合は
  そちらが優先されます）。
- `output_schema` を指定するには `prompt`/`system_prompt`/`agent` のいずれかが必須です
  （モデルを呼び出さないノードには適用先がありません）。
- `schema_name` は `output_schema` とセットで指定します（既定値は `structured_output`）。
- `jq` の出力が文字列の場合は `jq -r` のように引用符なしのテキストとして展開されます。それ以外
  （オブジェクト・配列・数値など）はコンパクトな JSON テキストとして展開されます。
- `jq` フィルターが複数の値を出力した場合は改行区切りで連結します。
- `jq` のみを指定して `prompt`/`system_prompt` を両方省略すると、モデルを呼び出さずにその時点の
  `{{ input }}` を変換するだけのノードになります（`model` の指定は不要です）。この場合、入力は
  有効な JSON である必要があります。

## ファイルへの出力（`write_file`）

ノードに `write_file:` を指定すると、そのノードの最終出力（`jq` を指定している場合はその結果）を
指定したパスに書き出します。次のステップに渡される `{{ input }}` は変わりません（あくまで副作用
としてファイルにも書き出すだけです）。

```yaml
# workflow.yml
nodes:
  summarize:
    prompt: |
      次の文章を3行で要約してください。
      {{ input }}
    write_file: summary.txt
steps:
  - id: summarize
    use: summarize
```

- パスは `agent:` と同じく、**コマンドを実行したカレントディレクトリ**からの相対パスとして
  解決されます（`workflow:` とは異なり、ワークフローファイル自身のディレクトリ基準ではあり
  ません）。
- ファイルが既に存在する場合は上書きします。親ディレクトリが存在しない場合はエラーになります
  （自動作成はしません）。
- `retry` で同じノードが再試行された場合、書き込みも試行のたびに行われます（最後に成功した
  試行の内容がファイルに残ります）。
- `for_each` の `max_concurrency` が2以上の本体（複数の要素が同時に処理される）からは
  `write_file` を持つノードを `use:` できません。パスは固定文字列なので、同時に走る複数の要素が
  同じパスへ競合して書き込んでしまうためです。`max_concurrency: 1`（既定）の本体や、`loop`の
  本体では問題なく使えます（各イテレーションが同じパスへ順番に上書きしていく、という動作に
  なります）。
- `parallel` の複数の branch がそれぞれ異なる `write_file` パスを持つノードを使う分には問題
  ありません（branch ごとに別々のステップ列なので、パスさえ重複しなければ競合しません）。同じ
  パスを持つノードを複数の branch から使った場合は、`for_each` の場合と同様に競合するので
  避けてください。
- 単独で（`prompt`/`agent`/`jq` などを何も伴わずに）`write_file` だけを指定したノードも作れます。
  この場合、その時点の `{{ input }}` をそのままファイルに書き出すだけのノードになります。

### 入力の検証（`input_schema`）

`output_schema` が出力（モデルの応答）を検証するのに対して、`input_schema` はノードが実行される
前の入力（`prompt` をレンダリングする前、あるいは `prompt` のないノードでは `jq` を適用する前
の `{{ input }}`）を検証します。指定した値は `output_schema` と同じく、まず `json_schemas:` の
キーとして解決を試み、一致するキーがなければ JSON Schema ファイルへのパスとして扱われます。

```yaml
json_schemas:
  city:
    schema:
      type: object
      required: [city]
nodes:
  introduce:
    input_schema: city
    prompt: |
      次の都市名を使って一文で紹介してください。
      {{ input }}
steps:
  - id: introduce
    use: introduce
```

検証は「JSON オブジェクトであること」「`required` に列挙されたキーが揃っていること」に加えて、
`properties`/`items` で宣言したフィールドの `type`（配列で複数型を許容する書き方も含む）や
`enum`、そしてネストしたオブジェクト・配列の中身も再帰的に確認します。ただし `format`・
`pattern`・数値の範囲・`additionalProperties`・`oneOf`/`anyOf`/`allOf`・`$ref` などは検査しない
簡易的なものです。また、スキーマに書かれていないフィールドが入力に含まれていても拒否しません
（`output_schema`／Structured Outputs 用の strict スキーマは `additionalProperties: false` を
要求しますが、同じスキーマを `input_schema` としても使い回せるようにするためです）。必須
フィールドの欠落や型の不一致を早期に（モデルを呼び出す前に）エラーとして弾くためのものです。
`agent` を指定したノードでは agent ファイル側の `input_schema` が使われるため、ノードに
`input_schema` を重ねて指定することはできません。

## MCP ツールの利用（`mcp`）

`prompt`/`system_prompt`/`agent` を持つノードに `mcp:` で `lait.config.yml` の `mcp_servers:`（登録方法は
[設定ファイル](/lait/docs/config/#mcp-サーバー) を参照）のエントリ名を並べると、そのノードのモデル
呼び出しに MCP ツールが渡され、モデルがツール呼び出しを返すたびに lait が実行してその結果を
モデルに返す、というやり取りを最大 `max_tool_rounds` 回まで自動で繰り返します。

```yaml
# workflow.yml
default:
  model: local
  mcp: [filesystem]        # ワークフロー全体の既定
  max_tool_rounds: 8
nodes:
  research:
    prompt: "{{ input }} について調べて要約してください。"
    mcp: [filesystem, remote-search]   # ノードで上書き
  summarize:
    agent: agents/summarize.md
    # mcp を書かなければ agent ファイル → default.mcp の順にフォールバック
steps:
  - use: research
  - use: summarize
```

- `mcp`/`max_tool_rounds` は `model`/`reasoning_effort` と同じ、ノード → （`agent` ノードなら）
  agent ファイルの frontmatter → ワークフローの `default:` → `lait.config.yml` の `default:` の
  順に、それぞれ独立してフォールバックします。
- `mcp`/`max_tool_rounds` は `prompt`/`system_prompt`/`agent` を持つノードだけに指定できます。データ変換のみの
  `jq` ノードや `workflow:` ノード（サブワークフロー側の各ノードに指定してください）には
  指定できません。
- `max_tool_rounds`（既定 8）に達してもモデルが最終回答を返さない場合はエラーになります。
- `structured_output: true`（`output_schema`）と `mcp` は併用できます。ツールを呼び出している間は
  `response_format` を送らず、モデルがツール呼び出しを止めた最後のラウンドだけ `response_format`
  を付けて再送します。多くの OpenAI 互換サーバーは、厳密な `json_schema` の `response_format` を
  渡された状態だとツール呼び出しを一切返さなくなるため、この順序で送ることでツールと
  structured output を両立させています。
- `--stream` と `mcp` の併用は未対応です（ストリームの `tool_calls` はチャンクに分割されて届く
  ため、まだ組み立て直す実装がありません）。
- ツール呼び出しはノードの `retry` が包む単位に含まれます。リトライが発生すると、副作用のある
  ツール呼び出しも含めてそのノードのツール周回全体がやり直されます。
- 対応モデルは LM Studio 上のツール呼び出し対応モデル（`qwen3` 系など）を想定しています。
  ツール呼び出しに対応していないモデルでは `tool_calls` が一切返らず、ツールは呼ばれません。

## システムプロンプトの指定（`system_prompt`）

ノードには `system_prompt:` で、ユーザーメッセージ（`prompt`）とは別にシステムプロンプトを
指定できます。指定しない場合、これまで通り `prompt` のレンダリング結果だけがユーザー
メッセージとして送られ、システムプロンプトは付きません。

```yaml
# workflow.yml
default:
  model: local
  system_prompt: "あなたは日本語のプロの校正者です。"   # ワークフロー全体の既定
nodes:
  proofread:
    system_prompt: "文体は「ですます調」に統一してください。"   # ノードで上書き
    prompt: "{{ input }}"
  translate:
    prompt: "{{ input }}"
    # system_prompt を書かなければ default.system_prompt にフォールバック
  classify:
    system_prompt: "次のテキストを spam/ham に分類し、ラベルだけを出力してください。"
    # prompt を省略すると、テンプレート展開を挟まず現在の入力がそのままユーザー
    # メッセージとして送られる（agent ノードが current_input をそのまま渡すのと同じ）
steps:
  - use: proofread
  - use: translate
  - use: classify
```

- `system_prompt` は `prompt` と同じテンプレートエンジン（handlebars）でレンダリングされ、
  `{{ input }}` / `{{ steps.<id> }}` にアクセスできます。
- `system_prompt` は `skills`/`mcp` と同じ、ノード → ワークフローの `default:` の順に
  独立してフォールバックします（`lait.config.yml` 側にはフォールバックしません）。
- `prompt` を省略して `system_prompt` だけを指定したノードでは、現在の入力（テンプレート
  展開なしの生のテキスト）がそのままユーザーメッセージになります。入力が JSON オブジェクト/
  配列でもエラーになりません（`prompt: "{{ input }}"` はオブジェクト/配列入力を拒否します）。
- `system_prompt` は `agent` ノードには指定できません。agent Markdown ファイルの本文が
  既にシステムプロンプトだからです。`workflow:` ノード（サブワークフロー側の各ノードに
  指定してください）にも指定できません。
- `skills` の追記先はこの `system_prompt`（レンダリング後）です。両方を指定した場合、
  `system_prompt` の内容の後に `---` 区切りでスキルの内容が続きます。`system_prompt` を
  指定していないノードでは、スキルの内容だけがシステムメッセージとして送られます。

## ファイル・画像の添付（`files` / `images`）

`prompt`/`system_prompt`/`agent` を持つノードには、CLI の `--file`/`--image`
（[ファイル・画像の添付](/lait/docs/attachments/)を参照）と同じ添付を `files:`/`images:` で指定
できます。

```yaml
# workflow.yml
default:
  model: local
nodes:
  review:
    prompt: "次の差分をレビューしてください。\n{{ input }}"
    files: [diff.patch, CONTRIBUTING.md]
  describe:
    prompt: "この画像に写っているものを説明してください。"
    images: [photo.png, "https://example.com/cat.png"]
steps:
  - use: review
  - use: describe
```

- `files:` に列挙した各パスはテキストとして読み込まれ、（レンダリング後の）`prompt`
  （`prompt` を省略したノードでは現在の入力）の後ろに、ファイル名付きのフェンスコードブロック
  として追記されます。バイナリファイル（UTF-8 として読めない内容）はエラーになります。
- `images:` に列挙した各値は、ローカルファイルパスなら base64 データ URL に、
  `http://`/`https://` から始まる値ならそのまま `image_url` として、ユーザーメッセージに
  添付されます。vision 対応モデルでのみ意味を持ちます。
- どちらもパスは、`agent:`/`write_file:` と同じく**コマンドを実行したカレントディレクトリ**
  からの相対パスとして解決されます。テンプレート展開はされません（固定のパス/URL のみ）。
- `files`/`images` は `prompt`/`system_prompt`/`agent` を持つノードにしか指定できません。
  データ変換のみの `jq` ノードや、`workflow:`/`command` を持つノードには指定できません。

## スキルの利用（`skills`）

`prompt`/`system_prompt`/`agent` を持つノードに `skills:` で `lait.config.yml` の `skills:`（登録方法は
[設定ファイル](/lait/docs/config/#スキル) を参照）のエントリ名を並べると、そのノードのシステム
プロンプトの末尾に、それぞれのスキルファイルの内容が追記されます。詳しくは
[スキルを使う](/lait/docs/skills/) を参照してください。

```yaml
# workflow.yml
default:
  model: local
  skills: [code-review]     # ワークフロー全体の既定
nodes:
  review:
    prompt: "次の差分をレビューしてください。\n{{ input }}"
    skills: [code-review, style-guide]   # ノードで上書き
  summarize:
    agent: agents/summarize.md
    # skills を書かなければ agent ファイル → default.skills の順にフォールバック
steps:
  - use: review
  - use: summarize
```

- `skills` は `mcp` と同じ、ノード → （`agent` ノードなら）agent ファイルの frontmatter →
  ワークフローの `default:` → `lait.config.yml` の `default:` の順にフォールバックします。
- `skills` は `prompt`/`system_prompt`/`agent` を持つノードだけに指定できます。データ変換のみの `jq` ノードや
  `workflow:` ノード（サブワークフロー側の各ノードに指定してください）には指定できません。
- スキルの内容は、ノードの `system_prompt`（指定されていれば、レンダリング後）や agent
  ファイルのシステムプロンプトの後に `---` 区切りで追記され、システムメッセージになります
  （`prompt` 自体は別のユーザーメッセージのままです）。ノード/agent 自身の指示が優先して
  読まれるようにするための順序です。`system_prompt` を指定していない `prompt` ノードでは、
  スキルの内容だけがシステムメッセージとして送られます。
- スキルファイルの本文は handlebars テンプレートとしてレンダリングされません（`{{ }}` を含む
  コード例などをそのまま書けます）。
- `mcp` と異なり `--stream` との併用に制限はありません（スキルは単なる静的な追記であり、
  ツール呼び出しの仕組みを使わないためです）。

## サブエージェントの利用（`subagents`）

`prompt`/`system_prompt`/`agent` を持つノードに `subagents:` で `lait.config.yml` の `agents:`（登録方法は
[設定ファイル](/lait/docs/config/#サブエージェント) を参照）のエントリ名を並べると、そのノードのモデル
呼び出しに、対応するエージェント Markdown ファイルが「サブエージェント」ツールとして渡されます。
`mcp` と同じ tool loop の仕組みに乗るため、モデルがツール呼び出しを返すたびに lait がそのサブ
エージェントを実行して結果を受け取り、モデルに返す、というやり取りを最終回答が出るまで自動で
繰り返します。

```yaml
# workflow.yml
default:
  model: local
  subagents: [researcher]        # ワークフロー全体の既定
nodes:
  triage:
    prompt: "{{ input }} を調査してください。必要であれば researcher に任せてください。"
    subagents: [researcher, fact-checker]   # ノードで上書き
  summarize:
    agent: agents/summarize.md
    # subagents を書かなければ agent ファイル → default.subagents の順にフォールバック
steps:
  - use: triage
  - use: summarize
```

- `subagents` は `mcp`/`skills` と同じ、ノード → （`agent` ノードなら）agent ファイルの
  frontmatter → ワークフローの `default:` → `lait.config.yml` の `default:` の順にフォールバック
  します。
- `subagents` は `prompt`/`system_prompt`/`agent` を持つノードだけに指定できます。データ変換のみの `jq` ノードや
  `workflow:` ノード（サブワークフロー側の各ノードに指定してください）には指定できません。
- モデルに渡すツール名は `agent__<サブエージェント名>`（例: `agent__researcher`）の形に修飾され、
  `mcp` のツール名（`<サーバー名>__<ツール名>`）と衝突しないようになっています。
- サブエージェント側の agent ファイルが `input_schema` を持つ場合、モデルの引数はそのスキーマの
  形そのままサブエージェントの `{{ input }}` になります。`input_schema` を持たない場合は
  `{ "input": ... }` という汎用の1フィールドのツール引数になり、その `input` の値
  （文字列ならそのまま、それ以外は JSON テキストとして）がサブエージェントの `{{ input }}` に
  なります。
- サブエージェント自身も、自分の frontmatter で `model`/`mcp`/`skills`/`subagents` などを独立に
  持てます。サブエージェントがさらに別のサブエージェントを呼ぶような入れ子も可能ですが、循環
  （巡り巡って自分自身を呼ぶ）や過度な深さは `workflow:` の入れ子と同様にエラーになります。
- `--stream` との併用は `mcp` と同じ理由（ストリームの `tool_calls` を再組み立てする実装が
  まだない）で未対応です。

それぞれの詳細は [エージェント Markdown ファイル（agent.md）](/lait/docs/agent/#サブエージェントの利用)
にもあります。詳しい仕組みは [サブエージェントを使う](/lait/docs/subagents/) を参照してください。

## ステップ間の値の受け渡し（`{{ steps.<id> }}` / `$steps`）

これまでの `{{ input }}` は「直前のステップの出力」しか参照できませんでしたが、`id` を持つ
ステップの出力は `steps.<id>` という名前でも記録され、以降のどのステップからでも参照できます。

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
      required: [city]
nodes:
  extract:
    prompt: |
      次の文章から都市名を JSON で抽出してください。
      {{ input }}
    output_schema: city_fact

  weather:
    prompt: "{{ steps.extract.city }} の天気を教えてください。"

  combine:
    jq: '{ city: $steps.extract.city, weather: . }'
steps:
  - id: extract
    use: extract
  - use: weather
  - use: combine
```

- `prompt` テンプレートからは `{{ steps.<id> }}` / `{{ steps.<id>.field }}` / `{{ json steps.<id> }}`
  で参照します（`{{ input }}` と同じ handlebars の記法・strict mode に従うため、未記録の `id` や
  存在しないフィールドを参照するとエラーになります）。
- `jq` フィルター（`when`、`switch` の `when`、`loop` の `while`/`until`、`for_each` の `items`、
  すべての `join`、そしてノードの `jq` 自身）からは `$steps.<id>` という jq のグローバル変数として
  参照します（`$steps.extract.city` のように）。未記録の `id` を参照した場合、handlebars と違って
  jq はエラーにならず `null` になります（通常の jq のフィールドアクセスの挙動どおりです）。
- 記録されるキーは、そのステップの `id:`（省略時は `use:` したノードの id）です。`id` も
  `use:` も持たない（`switch`/`parallel`/`loop`/`for_each` で `id` を省略した）ステップは
  記録されません。
- 同じキーで複数回記録された場合（`loop`/`for_each` の本体で同じステップ/ノードが繰り返し
  実行された場合や、同じノードを別々のステップから `use:` した場合）は、`steps.<id>` は
  最後に実行された結果で上書きされます。
- `parallel` の branch の中で記録した `id` は、その branch の中だけで有効です。branch の外
  （`parallel` の次のステップ以降）からは参照できません（各 branch は同時に走るため、複数の
  branch が同じ `id` を記録した場合に「どちらが正か」を決められないからです）。branch の結果を
  外へ出す方法は、これまでどおり `parallel` の `join` です。

## エラー処理（`retry` / `timeout` / `on_error`）

ノードには `retry:`（再試行）、`timeout:`（1回の試行あたりの時間制限）を指定できます。
いずれも、そのノード自身のアクション（`input_schema` の検証 → `prompt`/`agent` の呼び出し →
`jq`、をひとまとまりとして扱います）に対して働きます。`on_error:`（再試行を使い切っても
失敗したときのフォールバック）はノードではなく、それを `use:` するステップ側に指定します
（同じノードでも呼び出し元によって異なる復旧フローを持たせられます）。

```yaml
# workflow.yml
default:
  model: local
nodes:
  call:
    prompt: "{{ input }}"
    timeout: 30
    retry:
      max_attempts: 3
      delay_seconds: 1
      backoff: 2.0
  fallback:
    jq: '{ fallback: .error }'
steps:
  - id: call
    use: call
    on_error:
      steps:
        - use: fallback
```

- `timeout:` は1回の試行にかける秒数の上限です。超過した試行は失敗として扱われ（`retry` が
  設定されていれば再試行の対象になります）、1以上である必要があります。
- `retry:` の `max_attempts` は初回を含む総試行回数で、必須かつ1以上です（`max_attempts: 3` は
  「1回試して、失敗したら最大2回まで再試行する」という意味です）。`delay_seconds`（初回の再試行
  前に待つ秒数、既定0）と `backoff`（再試行のたびに待機時間へ掛ける倍率、既定1.0）は任意です。
  進捗表示（標準エラー出力）に `-> attempt k/max failed: ...; retrying in Ns` の行が出ます。
- `on_error:` は、`retry` を使い切っても（あるいは `retry` がなければ最初の1回で）失敗した
  ときに、ワークフローを異常終了させる代わりに実行する `steps` です。入力には
  `{"error": "<失敗内容のメッセージ>", "input": <そのステップに入ってきた入力>}` という
  オブジェクトが渡されるので、`jq` で必要な形に加工してください。`on_error` の `steps` の中でも
  `stop`/`break`/`switch`/`parallel`/`loop`/`for_each` を含め、通常のステップと同じ構文が使えます
  （`break` を使う場合は、失敗したステップ自身が `loop`/`for_each` の本文の中にある必要が
  あります）。`on_error` を指定しなければ、これまでどおり失敗はワークフロー全体のエラーになります。
  `on_error` は `use:` を伴わないステップには指定できません（失敗しうるアクションがないためです）。
- `retry`/`timeout` は `workflow:` を持つノードには指定できません（サブワークフロー自身の
  ステップに設定してください。呼び出し元の既定値をそこに重ねて適用すると、サブワークフロー側が
  既に継承している既定値と二重になってしまうためです）。一方 `on_error` は `workflow:` ノードの
  `use:` サイトに指定でき、サブワークフロー全体が失敗した場合を呼び出し元でまとめて捕捉できます。
- `switch`/`parallel`/`loop`/`for_each` のステップは `use`/`when`/`on_error`/`stop`/`break` の
  いずれも併用できません（これらは入れ子のステップ列に処理を委ねるだけで、自分ではアクションを
  実行しないためです）。

### ワークフロー全体の既定値（`default.retry` / `default.timeout`）

`default:` 直下に `retry:`/`timeout:` を書くと、`model`/`reasoning_effort` と同じように
ワークフロー全体の既定値になります。`prompt`/`agent` でモデルを呼び出すノードが自分自身の
`retry`/`timeout` を持たない場合、この既定値が使われます。

```yaml
# workflow.yml
default:
  model: local
  retry:
    max_attempts: 3
    delay_seconds: 1
  timeout: 30
nodes:
  echo:
    prompt: "{{ input }}"          # default.retry / default.timeout が適用される
  echo_override:
    prompt: "{{ input }}"
    retry:
      max_attempts: 1              # このノードだけ既定値を上書き（timeout は default の30秒のまま）
  summarize:
    jq: '.summary'                 # jq のみのノードには適用されない
steps:
  - use: echo
  - use: echo_override
  - use: summarize
```

- 適用されるのは `prompt`/`agent` でモデルを呼び出すノードだけです。`jq` のみのノードや
  `workflow:` ノードには適用されません（`workflow:` 側の `retry`/`timeout` はサブワークフロー
  自身のステップに設定してください）。
- `retry`/`timeout` はそれぞれ独立してフォールバックしますが、`retry` は
  `max_attempts`/`delay_seconds`/`backoff` を**まとめて1つの単位として**フォールバックします
  （フィールドごとに個別マージはしません）。つまりノード側に
  `retry: { max_attempts: 2 }` だけを書くと、`delay_seconds`/`backoff` は
  `default.retry` の値ではなく、それぞれの既定値（`0`/`1.0`）になります。
- `switch`/`parallel`/`loop`/`for_each`/`on_error` の内側のノードにもこの既定値は届きます。
- `retry.max_attempts` は必須かつ1以上、`timeout` は1以上である必要があります（ノード単位の
  `retry`/`timeout` と同じ検証です）。

## 条件分岐（`when` / `switch`）

ステップには [jq](https://jqlang.org/) フィルターを条件式として使う2種類の分岐構文が使えます。
条件式はその時点の `{{ input }}` に対して評価されます。JSON としてパースできればそのオブジェクト
/配列/値に対して、パースできないプレーンテキストであれば JSON 文字列としてラップした上で評価
されるため、直前のノードが `prompt` のみ（構造化出力なし）でテキストを返した場合でも条件式は
壊れません。条件式の出力はちょうど1つの値である必要があり（0個・複数個はエラー）、その値が
`false`/`null` なら偽、それ以外はすべて真になります（jq 自身の truthy/falsy 判定と同じです）。

### `when:` ―― ステップ単位のガード

`use:` を持つステップに `when:` を追加すると、条件が偽のときそのステップ全体（ノードの実行）を
スキップし、入力を無変換のまま次のステップに渡します。

```yaml
# workflow.yml
nodes:
  translate:
    prompt: |
      次の文章を英訳してください。
      {{ input }}
steps:
  - id: maybe-translate
    when: '.lang != "en"'
    use: translate
```

`use:` を持たないステップに `when:` と `stop:`/`break:` を組み合わせることもできます
（「[早期終了](#早期終了stop--break)」を参照）。

### `switch:` ―― 複数分岐

ステップに `switch:` を指定すると、そのステップは他の全フィールド（`use`/`when`/`on_error` 等、
`id` を除く）を持てない代わりに分岐ルーターになります。`cases` を先頭から評価し、最初に `when`
が真になったケースの `steps`（入れ子のステップ列）を実行します。どのケースにも一致しなかった
場合は `else:` の `steps` を実行し、`else:` がなければエラーで停止します（分岐漏れを黙って
通過させないためです）。分岐後は `switch` ステップの続き（親の `steps` の次の要素）にそのまま
戻ります。

```yaml
# workflow.yml
nodes:
  triage:
    prompt: |
      次の問い合わせを分類してください。
      {{ input }}
    output_schema: triage
    schema_name: triage

  escalate:
    model: cloud
    prompt: "緊急対応メモを書いてください。\n{{ input }}"

  draft-reply:
    prompt: "通常対応の返信文を作成してください。\n{{ input }}"

  auto-close:
    jq: ".summary"

  notify:
    prompt: "次の内容を1行の通知文にしてください。\n{{ input }}"

steps:
  - id: triage
    use: triage

  - id: route
    switch:
      cases:
        - id: high              # 任意。進捗表示用のラベル
          when: '.severity == "high"'
          steps:
            - use: escalate
        - id: medium
          when: '.severity == "medium"'
          steps:
            - use: draft-reply
      else:
        - use: auto-close

  - id: notify
    use: notify
```

- `switch` の `steps`（`cases[].steps`/`else`）は空配列にできません。少なくとも1ステップ必要です。
- `switch` は `id` によるジャンプ（`goto`）やループではありません。分岐は非循環で、実行後は必ず
  親の `steps` に戻ります。
- 分岐が入ると進捗表示（標準エラー出力）は `[index/total]` ではなく、実行された経路上の通し
  番号 `[n] id` になります（スキップされたステップも番号を1つ消費し `[n] id (skipped)` と出ます）。
  `switch` は常にどちらか一方の経路しか実行しないため、この通し番号は分岐をまたいでも連続します。

## 並列実行（`parallel`）

ステップに `parallel:` を指定すると、そのステップは他の全フィールド（`use`/`when`/`switch` 等、
`id` を除く）を持てない代わりに、複数のステップ列（`branches`）を**同時実行**する
ファンアウト/ファンインになります。各 branch は、その `parallel` ステップに入ってきた時点の
`{{ input }}` を同じスナップショットとして受け取り、独立したステップ列として並列に実行されます。

```yaml
# workflow.yml
default:
  model: local
nodes:
  sentiment:
    prompt: |
      次の文章の感情を一言で判定してください。
      {{ input }}
  summary:
    prompt: |
      次の文章を1行で要約してください。
      {{ input }}
  keywords:
    prompt: |
      次の文章からキーワードを3つ抽出してください。
      {{ input }}
  report:
    prompt: |
      次の情報からレポートを1段落で書いてください。
      {{ input }}
steps:
  - id: analyze
    parallel:
      branches:
        - id: sentiment
          steps:
            - use: sentiment
        - id: summary
          steps:
            - use: summary
        - id: keywords
          steps:
            - use: keywords
      join: '{sentiment: .sentiment, summary: .summary, keywords: .keywords}'

  - id: report
    use: report
```

- 全 branch が完了すると、各 branch の最終出力（モデル応答、または `jq` で加工した結果）を
  branch の `id` をキーにした JSON オブジェクトへ集約します。値は `when`/`jq` 入力と同じ
  ルールで、JSON としてパースできればそのまま、できなければ JSON 文字列としてラップされます。
  このオブジェクトは常に `branches` の宣言順でキーが並びます（branch の完了順ではありません）。
  そのため `parallel` の出力は実行タイミングに依存せず決定的です。
- `join`（任意）は、その集約オブジェクトに適用する [jq](https://jqlang.org/) フィルターで、
  ノードの `jq:` と同じ働きをします。省略した場合は、集約オブジェクトそのもの（JSON テキスト）が
  次のステップの `{{ input }}` になります。
- `branches` の各要素は `id`（任意。省略時は `branch-1`、`branch-2`… になり、進捗表示と
  集約オブジェクトのキーの両方に使われます）と `steps`（空配列不可。通常のステップ列と同様、
  `when`/`switch`/`parallel` を入れ子にできます）を持ちます。同じ `parallel` 内で `id` が
  重複しているとエラーになります（集約オブジェクトのキーとして意味を持つため、`switch` の
  `case.id`＝ラベルのみ、とは異なります）。
- branch のどれか1つでもエラーになった場合、その時点で `parallel` ステップ全体が失敗します
  （他の branch の結果を待たずに停止する場合があります）。
- `parallel` は `switch`/`loop`/`for_each` と同時に指定できません（1ステップにつき、これら
  ルーター系フィールドはどれか1つだけ）。
- 分岐中は進捗表示（標準エラー出力）が branch ごとに `[branch-id] [n] id` の形式でインター
  リーブされます（`n` は branch 内だけで完結するローカルな通し番号で、0から数え直します）。
  親の `steps` 側の通し番号は `parallel` ステップ自体で1つ進むだけで、`switch` と異なり
  branch 内のステップ数だけ連続して進むことはありません（複数 branch が同時に進行するため、
  単一の通し番号では実行順を正しく表せないからです）。

## 条件ループ（`loop`）

ステップに `loop:` を指定すると、そのステップは他の全フィールド（`use`/`when`/`switch`/
`parallel`/`for_each` 等、`id` を除く）を持てない代わりに、`steps`（ループ本体）を
[jq](https://jqlang.org/) 条件が満たされるまで**逐次**繰り返し実行するループになります。各
イテレーションの最終出力が、次のイテレーションの `{{ input }}` になります（1回目は `loop`
ステップ自身に入ってきた入力）。

```yaml
# workflow.yml
default:
  model: local
nodes:
  retry-once-more:
    prompt: |
      前回の失敗理由を踏まえて、もう一度やり直してください。
      {{ input }}
    output_schema: validation_result
    schema_name: validation_result
steps:
  - id: retry-until-valid
    loop:
      until: '.valid == true'
      max_iterations: 5
      steps:
        - use: retry-once-more
```

- `while:` は各イテレーションの実行**前**（1回目を含む）に、その時点の入力に対して評価され
  ます。偽ならその時点で1回も実行せず終了するので、0回実行されることもあります。1回目の
  評価対象は `loop` ステップ自身に入ってきた入力です。`loop` がワークフローの先頭ステップの場合、
  これは `<PROMPT>`（CLI引数）そのもの——多くの場合プレーンテキスト——なので、`while` を
  先頭ステップに置く場合は特に、条件式が上記のJSON文字列ラップの影響を受けないか注意してください。
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
  ルドを参照する条件を書く場合は、`steps` の最後のステップが使うノードで `output_schema` か
  `jq` を使ってJSONの値に整形しておく必要があります。
- 進捗表示（標準エラー出力）は `switch` と同様、実行された経路上の通し番号 `[n] id` が
  イテレーションをまたいで連続します（`loop` は並列実行される `parallel` と違い、本質的に
  逐次実行だからです）。加えて `-> iteration k/max` の行で現在のイテレーション回数が出力
  されます。

## 配列反復（`for_each`）

ステップに `for_each:` を指定すると、そのステップは他の全フィールド（`loop` と同じく `id` を
除く全フィールド）を持てない代わりに、jqフィルターで選んだ配列の**各要素**に対して `steps` を
1回ずつ（配列の順序どおり、**逐次**）実行し、結果を配列として集約するマップ処理になります。

```yaml
# workflow.yml
default:
  model: local
nodes:
  summarize-item:
    prompt: "この項目を要約してください: {{ input }}"
steps:
  - id: process-items
    for_each:
      items: '.items'
      steps:
        - use: summarize-item
      join: 'map(select(. != null))'
```

- `items:` は、その時点の入力に対して評価するjqフィルターで、**単一のJSON配列を1つだけ**返す
  必要があります（`.items` のように配列そのものを返してください。`.items[]` のような
  ストリーム展開ではありません。要素数はゼロでも構いません）。それ以外の値（オブジェクトや
  文字列など）を返した場合や、出力が0個・複数個の場合はエラーになります。
- 配列の各要素が、そのイテレーションの `{{ input }}` になります。`parallel` の branch と
  異なり、`for_each` の本体は要素そのもの以外——周辺のフィールドを含む入力全体——には
  アクセスできません。周辺情報が必要な場合は、`for_each` の手前に `jq` を持つノードのステップを
  置いて、各要素にあらかじめ文脈を埋め込んでおいてください。
- 各イテレーションの最終出力を、`items` の並び順で配列に集約します。`join`（任意）は、その
  配列に適用する jq フィルターで、`parallel` の `join` と同じ役割です。省略した場合は、
  集約した配列そのもの（JSONテキスト）が次のステップの `{{ input }}` になります。
- 配列の要素が0件の場合は0回実行され、出力は空配列（`join` があればそれを適用した結果）に
  なります。これはエラーではありません。
- `max_iterations` は持ちません。配列の長さで自然に有界なため、`for_each` では不要と判断
  しています。
- 進捗表示は `loop` と同様、`switch` と同じ通し番号方式（要素をまたいで連続）に加えて、
  `-> item k/n` の行で現在の要素の位置を出力します。

### 並行実行（`max_concurrency`）

`max_concurrency` を指定すると、要素の処理を最大その数まで同時に実行します。省略時は `1`
（これまでどおりの完全な逐次実行）です。

```yaml
# workflow.yml
default:
  model: local
nodes:
  summarize-item:
    prompt: "この項目を要約してください: {{ input }}"
steps:
  - for_each:
      items: '.items'
      max_concurrency: 4
      steps:
        - use: summarize-item
      join: 'map(select(. != null))'
```

- 結果は完了順ではなく、常に `items` の並び順で集約されます（`join` に渡される配列の順序は
  `max_concurrency: 1` のときと変わりません）。
- `max_concurrency` が2以上のとき、`for_each` の本体は `parallel` の branch と同じ扱いになります。
  すなわち、本体の中で記録した `{{ steps.<id> }}`/`$steps` はその要素の処理の中だけで有効で
  （他の要素や `for_each` の外からは見えません）、本体の中で `stop`/`break` を使うことはできません
  （複数の要素が同時に走っているときに「このループを打ち切る」「ワークフローを止める」対象を
  一意に決められないためです）。同じ理由で、`write_file` を持つノードを本体から `use:` する
  こともできません。`max_concurrency: 1`（既定）のときは、これまでどおり `loop` の
  イテレーションと同じ扱い（`{{ steps.* }}` は要素をまたいで引き継がれ、`break` も
  `write_file` も使えます）です。
- 進捗表示も `parallel` と同様になり、要素ごとに `[item-n]` を付けた行が入り交じって出力されます。
- 値は1以上である必要があります。

## 早期終了（`stop` / `break`）

ステップに `stop: true` または `break: true` を指定すると、`use:` があればそのノードのアクション
を実行した**後**に、通常の逐次実行を打ち切ります。`use:` を伴わずに `stop:`/`break:` だけを
指定することもでき、その場合は何も実行せず、その時点の `{{ input }}` をそのまま次に渡した上で
打ち切ります。どちらも `when:` と組み合わせて「ある条件が満たされたら打ち切る」という使い方が
基本です。

```yaml
# workflow.yml
nodes:
  check:
    prompt: "十分な情報が揃っているか判定してください: {{ input }}"
    output_schema: judgement
  ask-more:
    prompt: "不足している情報を尋ねる質問を1つ生成してください: {{ input }}"
steps:
  - id: check
    use: check

  - when: '.sufficient == true'
    stop: true          # 揃っていれば、ここでワークフロー全体を正常終了する

  - id: ask-more
    use: ask-more
```

```yaml
# workflow.yml
nodes:
  refine:
    prompt: "{{ input }}"
    output_schema: validation_result
steps:
  - id: refine
    loop:
      until: '.valid == true'
      max_iterations: 5
      steps:
        - use: refine
        - when: '.attempts >= 3'
          break: true    # 3回試したら、valid になっていなくてもループを打ち切る
```

- `stop: true` は、その時点の値を**ワークフロー全体の最終出力**として、残りのステップ
  （ネストしている `loop`/`for_each`/`switch` の外側も含めて）を一切実行せずに正常終了します。
  ただし `parallel` の branch の中では使えません（他の branch が並行して走っている以上、
  「ワークフローを止める」という操作の意味が定義できないためです）。
- `break: true` は、最も内側の `loop`/`for_each` の本文だけを打ち切ります。`loop` では
  `while`/`until` を満たした場合と同じ扱いで（`until` の場合は `max_iterations` 超過の
  エラーにはなりません）、`for_each` ではそこまでに集めた結果で `join` を実行します。
  `break: true` を使うには、`switch` の中などを経由してでもよいので、`loop`/`for_each` の
  本文の中にいる必要があります（`parallel` の branch をまたいで外側のループを終了することは
  できません）。
- `stop`/`break` は `use`/`when`/`on_error` と併用できますが、`switch`/`parallel`/`loop`/
  `for_each` との併用は不可です。両方を同じステップに指定することもできません。

## サブワークフロー呼び出し（`workflow`）

ノードに `workflow:` を指定すると、そのノードは `prompt`/`agent` の代わりに**別のワークフロー
YAML ファイル**を、その時点の入力に対して実行します。サブワークフローの最終出力が、このノード
の出力になります。共通のステップ列を別ファイルに切り出して、複数のワークフローから再利用できます。

```yaml
# workflow.yml
default:
  model: local
nodes:
  summarize:
    workflow: ./shared/summarize.yml
    jq: '.summary'
steps:
  - id: summarize
    use: summarize
```

```yaml
# shared/summarize.yml
nodes:
  summarize:
    prompt: |
      次の文章を3行で要約してJSON `{ "summary": "..." }` で返してください。
      {{ input }}
    output_schema: summary
steps:
  - use: summarize
```

- `workflow:` に書くパスは、**このノードが定義されているワークフローファイル自身のディレクトリ**
  からの相対パスとして解決されます。`agent:`（常にコマンドを実行したカレントディレクトリからの
  相対パス）とは解決基準が異なるので注意してください。サブワークフローが入れ子になる場合、
  各ファイルは自分のいる場所からの相対パスで次のファイルを指せます。
- サブワークフロー側の `default:`/`models:`/`json_schemas:` が優先され、サブワークフロー側に
  定義がない項目は、呼び出し元（さらにその呼び出し元、…）の定義にフォールバックします。
  共通の `default.model` や `models:` エイリアスを一番外側のワークフローに一度書いておけば、
  サブワークフロー側で省略できます。`nodes:` はこのフォールバックの対象外です。サブワークフロー
  の `use:` は常にそのファイル自身の `nodes:` だけを見ます。
- サブワークフローに `name`/`description` があれば、進捗表示（標準エラー出力）に出力されます。
- `{{ steps.<id> }}`/`$steps` はサブワークフローの境界を越えません。サブワークフロー内で記録
  されたステップの出力は、そのサブワークフローの中だけで参照でき、呼び出し元には見えません。
  逆に、呼び出し元で記録された `{{ steps.* }}` もサブワークフロー側からは参照できません
  （`agent:` と同じく、別ファイルとして完全に独立した単位として扱われます）。サブワークフロー
  内部で `stop:`/`break:` を使っても、それはそのサブワークフロー自身の実行を早期終了するだけで、
  呼び出し元のワークフロー全体には影響しません。
- `workflow:` を持つノードは `prompt`/`agent`（どちらかで直接モデルを呼ぶ）と併用できません。
  `model`/`reasoning_effort`/`temperature`/`top_p`/`max_tokens`/`input_schema`/`output_schema`/
  `schema_name` はサブワークフロー側のステップが個別に持つため、このノードには指定できません。
  同様に `retry`/`timeout` もサブワークフロー側のステップに指定してください。`on_error` は
  ノードではなく `use:` サイト側の話なので、`workflow:` ノードに対しても通常どおり指定でき、
  サブワークフローが全体として失敗した場合を呼び出し元でまとめて捕捉できます。`use:` サイトの
  `when`/`jq`/`stop`/`break` も通常どおり併用できます。
- 循環参照（A が B を呼び、B が A を呼ぶ、など）はエラーになります。ネストの深さにも上限が
  あります。

## 任意コマンドの実行（`command`）

ノードに `command:` を指定すると、そのノードは `prompt`/`agent` の代わりに**外部コマンド**を
実行します。コマンドの標準入力にはその時点の `{{ input }}` がそのままパイプされ、標準出力
（末尾の改行は1つだけ取り除かれます。シェルの `$(...)` と同じ扱いです）がこのノードの出力
になります。既存のシェルツール（`jq`/`rg`/`sort`/自作スクリプトなど）をワークフローの
パイプラインに直接組み込めます。

```yaml
# workflow.yml
nodes:
  count-lines:
    command: ["wc", "-l"]
    jq: 'tonumber'
  format:
    prompt: "行数: {{ input }}"
steps:
  - use: count-lines
  - use: format
```

```sh
printf 'a\nb\nc\n' | cargo run -- run workflow.yml -
```

- `command:` は配列で、先頭要素が実行するプログラム、残りがその引数です。
  **シェルを経由せず直接 exec します**（`sh -c` や文字列連結は行いません）。そのためプロンプト
  と同様、各要素は `{{ input }}`/`{{ steps.<id> }}` を含む handlebars テンプレートとして
  レンダリングできますが、レンダリング結果が余分な引数や別のコマンドとして解釈されることは
  ありません。
- 終了コードが 0 以外の場合、そのノードの実行は失敗として扱われます（標準エラー出力の内容が
  エラーメッセージに含まれます）。他のノードの失敗と同じく、ステップの `retry`/`on_error` で
  拾えます。
- 標準出力が UTF-8 として読めない場合はエラーになります（バイナリ出力は非対応です）。
- コマンドの出力は、他のノードの出力と同じく、この後に指定した `jq`/`write_file` を通ります。
- `retry`/`timeout` は指定すれば適用されますが、`jq` 専用ノードと同じく、ワークフロー全体の
  `default.retry`/`default.timeout` へは自動的にフォールバックしません（LLM 呼び出し向けの
  既定値を、無関係なコマンド実行にまで及ぼさないためです）。
- `command` は `prompt`/`system_prompt`/`agent`/`workflow`/`files`/`images` と併用できません。
  実行するのは1つのアクションだけだからです。
