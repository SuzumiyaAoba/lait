# 設定ファイル

[ドキュメント目次に戻る](./README.md)

CLI 引数や環境変数で指定していない値は、コマンドを実行したディレクトリの
`lait.config.yml` からデフォルトとして読み込まれます。設定できる基本項目は次のとおりです。

```yaml
# lait.config.yml
base_url: http://localhost:1234/v1
api_key: lm-studio
default:
  model: local-model
  reasoning_effort: medium
  temperature: 0.7
  top_p: 0.9
  max_tokens: 512
  system: あなたは有能なアシスタントです。
```

`default:` には `model`/`reasoning_effort` に加えて、サンプリングパラメータ `temperature`
（`0.0`〜`2.0`）・`top_p`（`0.0`〜`1.0`）・`max_tokens`（`1`以上）も指定できます。
`system` はチャットモードの既定システムプロンプトで、CLI の `--system`/`--system-file` が
優先されます（システムプロンプトを自前で持つ agent／workflow はこの値を参照しません）。CLI の
`--temperature`/`--top-p`/`--max-tokens` と同じく、それぞれ独立してフォールバックします
（`reasoning_effort` と同じ仕組みで、`retry` のようなブロック単位のフォールバックではありません）。

`default:` にはこの他、`mcp:`/`skills:`/`subagents:`（後述の各節を参照）、`max_tool_rounds:`
（既定 8）、および次の 2 項目も指定できます。

- `render: true` — `--render` を渡さなくても応答を Markdown として端末表示します。
  チャット・`lait run`・`lait agent run`・`lait prompt run` のいずれにも効きます
  （[出力例](./output.md) を参照）。
- `history: false` — `--no-history` を渡さなくても `lait history` への記録を止めます
  （[実行履歴](./history.md) を参照）。

いずれも CLI フラグ（`--render`/`--no-history`）が指定されればそちらが優先されます。

## モデル定義と alias

複数の呼び出しモデルを設定ファイルに定義し、alias で使い回せます。`models` は alias をキー、
モデル定義の配列を値にするマップです。各要素には `provider.base_url` と `model_id` を指定し、
`provider.api_key`・`default_reasoning_effort`・`default_temperature`・`default_top_p`・
`default_max_tokens` は任意で指定できます。プロバイダーのキーは正式名称の `provider` を
使用してください。

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
      default_temperature: 0.7
      default_top_p: 0.9
      default_max_tokens: 512

# alias は `default.model`、CLI、環境変数から参照できます。
default:
  model: local
```

`default.model`、`--model`、`LLM_MODEL` には alias または生のモデル ID を指定できます。alias を指定した
場合は、対応する配列の先頭要素が使用され、その要素の `model_id` とプロバイダー設定が
リクエストに適用されます。生のモデル ID を指定した場合は、従来どおりトップレベル設定の
`base_url` などが使用されます。

## 設定値の優先順位

`base_url`/`api_key` は、次の優先順位で解決されます。CLI 引数と環境変数の間では CLI 引数が優先されます。

`CLI 引数 > 環境変数 > モデル定義（provider.*） > トップレベル設定（base_url/api_key） > 組み込み既定値`

たとえば alias のモデル定義が `provider.base_url` を持つ場合、その値はトップレベルの `base_url`
より優先されます。CLI の `--base-url` や `OPENAI_BASE_URL` を指定した場合は、それらがモデル定義を
上書きします。`provider.api_key` と `default_reasoning_effort` を省略した場合は、対応する
トップレベルの `api_key`、`default.reasoning_effort` がフォールバックとして使用されます。

`model`/`reasoning_effort`/サンプリングパラメータ/`mcp`/`skills`/`subagents`/`max_tool_rounds` の
解決順は、**呼び出し経路によって異なる 2 系統**があります（`base_url`/`api_key` とは別のチェーン
です）。

- **チャット／`lait prompt`／`-p`**: `CLI 引数（環境変数フォールバック込み） > 名前付きプロンプト
  の値（`prompts.<name>.model` など） > default: の値 > 組み込み既定値`
- **`lait agent run`／ワークフローノード**: `ノード自身（またはエージェント Markdown の
  frontmatter）の値 > エージェント Markdown の値（ノードが `agent:` を指す場合） >
  ワークフローファイルの `default:` > lait.config.yml の `default:` > 組み込み既定値`

どちらの経路でも、`lait.config.yml` の `default:` が最終手前のフォールバック層になります。

`base_url`、`api_key` はトップレベルの項目として、フォールバック用の `model`、`reasoning_effort`、
`temperature`、`top_p`、`max_tokens` は `default:` の配下にまとめて指定します。設定ファイルの自動読込を
無効にする場合は `--no-config` を指定してください。この場合は設定ファイルを読み込まず、CLI
引数、環境変数、既定値だけが使用されます。

## `${VAR_NAME}` による環境変数参照

トップレベルの `api_key`/`base_url`、および `models:` の `provider.api_key`/`provider.base_url`
には、`${VAR_NAME}` という記法で環境変数を埋め込めます。API キーなどの秘密情報を設定ファイルに
平文で書かずに済ませるためのものです。

```yaml
# lait.config.yml
models:
  cloud:
    - provider:
        base_url: https://api.example.com/v1
        api_key: "${CLOUD_API_KEY}"
      model_id: cloud-model
```

- `${VAR_NAME}` は文字列の中の任意の位置に埋め込めます（例: `https://${HOST}/v1`）。1つの値に
  複数の `${VAR_NAME}` を含めることもできます。
- 参照した環境変数が未設定の場合はエラーになります（空文字列として扱われることはありません）。
- `VAR_NAME` は英数字と `_` のみが使えます。
- この展開は設定ファイル（`lait.config.yml`、および後述するワークフローファイルの `models:`/
  トップレベル設定）から読み込んだ値にのみ適用されます。CLI の `--api-key`/`--base-url` に
  そのまま `${VAR_NAME}` と書いても展開されません（シェル側の変数展開に任せてください）。
- 後述の [MCP サーバー](#mcp-サーバー) の `command`/`args`/`env`/`cwd`/`url`/`headers` も同じ
  規則で `${VAR_NAME}` を展開します。`prompts:` のテンプレート本文や `skills:`/`agents:` の
  パス、`default.system`、ワークフローの `prompt:`/`system_prompt:` には**この展開は適用されません**
  （こちらは `--var`/handlebars のテンプレート変数で渡してください）。

## `.env` ファイルの自動読み込み

起動時にカレントディレクトリの `.env` ファイルが自動で読み込まれ、**未設定の環境変数のみ**が
補完されます（シェルで export 済みの変数が常に優先されます）。読み込まれた値は、CLI 引数の
環境変数フォールバック（`OPENAI_API_KEY` など）と `${VAR_NAME}` 展開の両方から参照できます。

```sh
# .env
OPENAI_API_KEY=sk-...
CLOUD_API_KEY="quoted value"   # コメント
export HOST=api.example.com    # export プレフィックスも可
```

- `.env` が存在しない場合は何もしません。壊れた行がある場合は行番号付きのエラーになります。
- 値は `'...'`（そのまま）・`"..."`（`\n` などのエスケープ対応）・裸のいずれでも書けます。
  複数行の値には対応していません。
- `--no-env` フラグで読み込みを無効化できます。

## MCP サーバー

`mcp_servers:` に MCP (Model Context Protocol) サーバーを登録すると、`--mcp`（チャット）・
agent ファイルの `mcp:`・ワークフローノードの `mcp:` から名前で参照してツールを使えるように
なります。詳しい使い方は [MCP サーバーのツールを使う](./mcp.md) を参照してください。

```yaml
# lait.config.yml
default:
  model: local
  mcp: [filesystem]        # 全経路（チャット / agent / workflow）の最終フォールバック
  max_tool_rounds: 8        # 省略時は 8

mcp_servers:
  # stdio: 子プロセスとして起動
  filesystem:
    command: npx
    args: ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    env:
      SOME_TOKEN: ${SOME_TOKEN}
    cwd: ./work             # 省略可

  # streamable HTTP: リモートサーバーに接続
  remote-search:
    url: https://mcp.example.com/mcp
    headers:
      Authorization: "Bearer ${SEARCH_TOKEN}"
```

- `command:`（stdio）と `url:`（streamable HTTP）はどちらか一方だけを指定します。両方または
  どちらも指定しない場合はエラーになります。
- `command`/`args`/`env` の値、`cwd`、`url`、`headers` の値は、いずれも `${VAR_NAME}` 展開の対象
  です（前節と同じ規則）。
- 実際に使われるサーバーだけがその場で接続されます（`mcp:` で名前を挙げていないサーバーは
  起動しません）。

## 名前付きプロンプト

`prompts:` に名前付きプロンプトを登録すると、`-p/--prompt-name <NAME>`（チャット）または
`lait prompt run <NAME>` から実行できます。詳しい使い方は
[名前付きプロンプトを使う](./prompts.md) を参照してください。

```yaml
# lait.config.yml
prompts:
  summarize:
    template: "次の文章を3行で要約してください。\n\n{{ input }}"
    model: local            # 省略時は default.model にフォールバック
    vars:
      style: casual         # --var style=formal で上書き可能
```

- `template` は handlebars テンプレートで、`{{ input }}`（位置引数／stdin）と
  `{{ vars.<key> }}`（`vars:` の既定値、`--var key=value` で上書き可能）を参照できます。
- `model` を省略した場合は `default.model` にフォールバックします。
- `lait prompt list` で登録済みのプロンプト名を一覧できます。

## スキル

`skills:` にスキル Markdown ファイルを登録すると、`default.skills`（チャット）・agent ファイルの
`skills:`・ワークフローノードの `skills:` から名前で参照して、その内容をシステムプロンプトに
追記できるようになります。詳しい使い方は [スキルを使う](./skills.md) を参照してください。

```yaml
# lait.config.yml
default:
  model: local
  skills: [code-review]   # 全経路（チャット / agent / workflow）の最終フォールバック

skills:
  code-review: skills/code-review.md
```

- 値はスキル Markdown ファイルへのパス、またはそのファイルを含むディレクトリへのパスです。
  ディレクトリを指定した場合は、その直下の `SKILL.md` が使われます（Anthropic の Agent Skills の
  慣習に合わせたもので、既存の `.claude/skills/<name>/` のようなディレクトリをそのまま指せます）。
- パスは、`mcp_servers:` とは異なり、その場では接続を持たず、実際に使われるたびにファイルを
  読み直します。

## サブエージェント

`agents:` にエージェント Markdown ファイルを登録すると、`--subagent`（チャット）・agent ファイルの
`subagents:`・ワークフローノードの `subagents:` から名前で参照して、モデル自身が実行時に呼び出す
かどうかを判断できる「サブエージェント」ツールとして使えるようになります。詳しい使い方は
[サブエージェントを使う](./subagents.md) を参照してください。

```yaml
# lait.config.yml
default:
  model: local
  subagents: [researcher]   # 全経路（チャット / agent / workflow）の最終フォールバック

agents:
  researcher: agents/researcher.md
```

- 値はエージェント Markdown ファイルへのパスです（`agent:` ノードと同じ形式のファイルを、
  そのまま名前を付けて登録します）。
- パスは、`skills:` と同じく、その場では接続を持たず、実際に使われるたびにファイルを読み直します。
