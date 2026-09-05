# 設定ファイル

[ドキュメント目次に戻る](./README.md)

CLI 引数や環境変数で指定していない値は、`lait.config.yml` からデフォルトとして
読み込まれます。ファイルはカレントディレクトリから探し始め、見つからなければ
`git` が `.git` を探すのと同じ要領で親ディレクトリを順にさかのぼって探します
（プロジェクトのサブディレクトリから実行しても見つかります）。`--config <PATH>`
で読み込むファイルを明示的に指定することもでき、この場合は探索を行わず、
指定したファイルが存在しなければエラーになります。`--no-config` を指定すると
探索・読み込み自体を行いません。まずはモデルと接続先だけを指定した、次の最小構成から始められます。

エディタで補完・検証を効かせたい場合は [`lait schema config`](./schema.md) が出力する
JSON Schema を使えます。

```yaml
# lait.config.yml
base_url: http://localhost:1234/v1
default:
  model: local-model
  reasoning_effort: medium
  temperature: 0.7
  top_p: 0.9
  max_tokens: 512
  system: あなたは有能なアシスタントです。
```

認証を必要としない LM Studio では `api_key` を省略できます。認証が必要な接続先では、平文のキーを置かず `${VAR_NAME}`（[環境変数参照](#var_name-による環境変数参照)）または `api_key_cmd` を使ってください。

`default:` には `model`/`reasoning_effort` に加えて、サンプリングパラメータ `temperature`
（`0.0`〜`2.0`）・`top_p`（`0.0`〜`1.0`）・`max_tokens`（`1`以上）も指定できます。
`system` はチャットモードの既定システムプロンプトで、CLI の `--system`/`--system-file` が
優先されます（システムプロンプトを自前で持つ agent／workflow はこの値を参照しません）。CLI の
`--temperature`/`--top-p`/`--max-tokens` と同じく、それぞれ独立してフォールバックします
（`reasoning_effort` と同じ仕組みで、`retry` のようなブロック単位のフォールバックではありません）。

`default:` にはこの他、`mcp:`/`skills:`/`subagents:`/`tools:`（後述の各節を参照）、
`max_tool_rounds:`（既定 8）、および次の 2 項目も指定できます。

- `render: true` — `--render` を渡さなくても応答を Markdown として端末表示します。
  チャット・`lait run`・`lait agent run`・`lait prompt run` のいずれにも効きます
  （[出力例](./output.md) を参照）。
- `history: false` — `--no-history` を渡さなくても `lait history` への記録を止めます
  （[実行履歴](./history.md) を参照）。

`history: false` は `--no-history` が指定されていないときの既定値です。`--no-history` は常に履歴を無効にします。`render: true` は `--render` と同じく Markdown 表示を有効にします。現在は `default.render: true` だけを CLI で無効にする `--no-render` はありません。

## グローバル設定ファイル

プロジェクトの `lait.config.yml` に加えて、`$XDG_CONFIG_HOME/lait/config.yml`
（`XDG_CONFIG_HOME` 未設定時は `~/.config/lait/config.yml`）をグローバル設定として
読み込みます。全プロジェクト共通のモデル定義や MCP サーバー、ローカル LLM の接続先などを
ここにまとめておけば、プロジェクトごとに複製する必要がありません。

読み込みはプロジェクト設定と同じ「カレントディレクトリから探索する」経路
（`--config`/`--no-config` のどちらも指定しなかった場合）でのみ行われ、見つかった
プロジェクト設定とマージされます。マージ規則は次のとおりです。

- `models:`/`mcp_servers:`/`skills:`/`agents:`/`prompts:`/`workflows:`/`tools:` は
  キー単位でマージします。同じ名前がグローバルとプロジェクトの両方にあれば
  プロジェクト側が勝ちます。
- `default:` は項目単位でマージします（`default.model` はプロジェクトが指定して
  いればそれを使い、未指定ならグローバルの値にフォールバックします）。
- `base_url` はプロジェクトが指定していればそちらを使い、未指定ならグローバルの値に
  フォールバックします。`api_key` と `api_key_cmd` は API キーの取得方法を表す一組として
  扱い、プロジェクト側でどちらか一方が指定されていれば、もう一方を含めてプロジェクト側の
  組を使います。
- `tool_policy.allow` と `tool_policy.deny` はグローバルとプロジェクトの値を結合します。
  `deny` は安全側の制約として残り、`allow` は追加されます。

`workflows:`/`agents:`/`skills:` の登録エントリの相対パスは、それを定義した設定ファイル
自身のディレクトリを起点に解決されます（グローバル設定内のエントリなら
`$XDG_CONFIG_HOME/lait/` が起点になります）。

`--config <PATH>` を指定した場合はそのファイルだけを読み込み、グローバル設定は一切
参照しません。`--no-config` を指定した場合もグローバル設定を含めて何も読み込みません。

## モデル定義と alias

複数の呼び出しモデルを設定ファイルに定義し、alias で使い回せます。`models` は alias をキー、
モデル定義の配列を値にするマップです。各要素には `provider.base_url` と `model_id` を指定し、
`provider.api_key`（または `provider.api_key_cmd`）・`default_reasoning_effort`・`default_temperature`・`default_top_p`・
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
        api_key: "${CLOUD_API_KEY}"
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

## 複数プロバイダーによるフォールバック

`models:` の alias に配列の 2 番目以降の要素を追加すると、先頭要素への接続が失敗した際に
順番にフォールバックします。ローカルの LM Studio を優先し、落ちていればクラウド API に
切り替える、といった構成が YAML だけで実現できます。

```yaml
# lait.config.yml
models:
  gpt:
    - provider:
        base_url: http://localhost:1234/v1   # 優先: ローカル LM Studio
      model_id: local-model
    - provider:
        base_url: https://api.openai.com/v1  # フォールバック: クラウド
        api_key: "${OPENAI_API_KEY}"
      model_id: gpt-4o
```

- フォールバックする条件は、接続エラー・タイムアウト、または応答が `5xx`・`429`・`408` の
  場合だけです。`4xx`（リクエスト不正・認証エラーなど）はフォールバックせずそのまま
  失敗します — 呼び出し側の設定ミスである可能性が高く、握りつぶすと気づきにくくなるためです。
- どの要素で成功したかは、切り替わった時点で `warning:` として標準エラー出力に、詳細は
  `-v`/`LAIT_LOG`（[verbose ログ](getting-started.md)参照）で確認できます。
- 2 番目以降の要素にも `default_reasoning_effort`/`default_temperature`/`default_top_p`/
  `default_max_tokens` を書けますが、**実際には使用されません**。サンプリングの既定値は
  常に先頭要素のものが使われます（どの要素にリクエストが着地するかは実行時にしか
  分からないため、リクエストごとに既定値が変わらないようにしています）。
- `--base-url`/`--api-key` を明示指定した場合、フォールバック先も含めてすべてのリクエストが
  その 1 つのエンドポイントに固定され、フォールバックは行われません。
- ストリーミング（`--stream`）は、ストリームが確立する前の失敗にのみフォールバックします。
  一度バイトが届き始めたストリームは、途中で別のプロバイダーに切り替えることはできません。
- 各要素は `api_key` の代わりに [`api_key_cmd`](#api_key_cmd-による外部コマンドからのシークレット取得)
  を使えます。フォールバック先のコマンドは、実際にそのフォールバック先が試行されるときにだけ
  実行されます。
- async-openai 側の内部リトライ（3 回・バックオフ付き）を経てから失敗が返るため、
  フォールバックの切り替えには数秒かかることがあります。

## 設定値の優先順位

接続先と API キーは、CLI 引数・環境変数を最優先に、次の順で解決されます。

| 値 | 高い順の解決元 |
| --- | --- |
| `base_url` | `--base-url` → `OPENAI_BASE_URL` → モデル alias の `provider.base_url` → トップレベル `base_url` → `http://localhost:1234/v1` |
| API キー | `--api-key` → `OPENAI_API_KEY` → モデル alias の `provider.api_key` / `provider.api_key_cmd` → トップレベル `api_key` / `api_key_cmd` |

`api_key` と `api_key_cmd` は同じ設定層で同時に指定できません。API キーを指定しない場合、認証なしのサーバーでも実行できるよう内部的にダミー値が使われます。モデル alias を使わない場合は、トップレベルの接続先設定が使われます。

モデル、サンプリングパラメータ、ツール関連の値は、呼び出し方によって次のように解決されます。値は各項目ごとに独立してフォールバックします。

- **通常のチャット（`lait [OPTIONS] PROMPT`）と `-p/--prompt-name`**: CLI 引数（環境変数のフォールバックを含む） → 名前付きプロンプトの値 → モデル alias の既定値 → `lait.config.yml` の `default:` → 組み込み既定値。
- **`lait prompt run <NAME>`**: `prompts.<name>.model` → `lait.config.yml` の `default.model`。この入口は `--model` や `--stream` などのチャット用上書きを受け付けません。
- **`lait agent run`／ワークフローノード**: ノード自身（または agent Markdown の frontmatter） → agent Markdown の値 → ワークフローの `default:` → `lait.config.yml` の `default:` → 組み込み既定値。

`default:` は、呼び出し側で指定していない値を補うための共通の既定値です。ワークフロー固有の設定については [ワークフロー](./workflow.md)、agent 固有の設定については [agent](./agent.md) を参照してください。

`base_url`、`api_key` はトップレベルの項目として、フォールバック用の `model`、`reasoning_effort`、
`temperature`、`top_p`、`max_tokens` は `default:` の配下にまとめて指定します。設定ファイルの自動読込を
無効にする場合は `--no-config` を指定してください。この場合は設定ファイルを読み込まず、CLI
引数、環境変数、既定値だけが使用されます。特定のファイルを明示したい場合は
`--no-config` の代わりに `--config <PATH>` を使ってください（`--no-config` と
`--config` は同時に指定できません）。

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

## `api_key_cmd` による外部コマンドからのシークレット取得

`${VAR_NAME}` 展開はシェル側で環境変数を事前に export しておく前提ですが、
`api_key_cmd` を使うと 1Password・pass・gopass・aws secretsmanager などの
シークレットマネージャーから API キーをその場で取得できます。トップレベルの
`api_key_cmd`、および `models:` の `provider.api_key_cmd` に指定でき、
`api_key`（`provider.api_key`）と同時に指定するとエラーになります。

```yaml
# lait.config.yml
models:
  gpt-4o:
    - provider:
        base_url: https://api.openai.com/v1
        api_key_cmd: "op read op://Personal/OpenAI/api-key"   # 1Password CLI の例
      model_id: gpt-4o
```

- 文字列を指定するとシェル経由（`sh -c`。Windows では `cmd /C`）で実行され、パイプや
  クォート、`$VAR` 展開が使えます。YAML の配列で指定すると、シェルを介さず直接
  実行されます（`api_key_cmd: ["op", "read", "op://Personal/OpenAI/api-key"]`）。
- コマンドは実際にリクエストを送る際に一度だけ実行され、その結果（stdout の末尾改行を
  除いたもの）はプロセスの実行中キャッシュされます。ワークフローのステップや
  `for_each` の反復のたびに再実行されることはありません。
- 標準出力が空、または終了コードが非 0 の場合は、標準エラー出力を含む分かりやすい
  エラーになります。
- `lait models`（`--remote` なし）でのモデル一覧表示ではコマンドは実行されません
  （実際にリクエストを送る経路でのみ実行されます）。`lait lint` は `api_key`/
  `api_key_cmd` の同時指定だけを静的にチェックします。

## レスポンスのディスクキャッシュ（`--cache`）

`--cache`（または `default.cache: true`）を指定すると、レスポンスを `.lait/cache/`
にディスクキャッシュします。ワークフローやプロンプトの反復開発で、変更していない
ステップの LLM 呼び出しが毎回走るのを避けられます。

```yaml
# lait.config.yml
default:
  cache: true
  cache_ttl: 3600   # 省略時は無期限（秒単位）
```

- キャッシュのキーは、base URL・モデル ID・サンプリングパラメータ（`reasoning_effort`/
  `temperature`/`top_p`/`max_tokens`）・メッセージ列・ツール定義・`response_format`
  から計算します。**API キーはキーに含まれません** — 認証情報だけが異なる2つの
  リクエストは同じキャッシュエントリを共有します。
- ヒットした場合は API を呼ばず、その旨を `note:` として標準エラー出力に注記します
  （`--show-usage` の集計にもキャッシュヒット分は含まれません — 実際にはリクエストを
  送っていないためです）。
- MCP・サブエージェント・カスタムシェルツールの呼び出しを含むラウンドも、各ラウンドの
  リクエスト単位でキャッシュされます。ツールループ全体を一つの結果として保存するものでは
  ありません。
- **`--stream` によるストリーミング応答はキャッシュの対象外です。**
- `--no-cache` を指定するとキャッシュの参照・書き込みの両方を無効にします
  （`default.cache: true` を上書きします）。`--cache`/`--no-cache` は同時に指定できません。
- `lait cache clear` で `.lait/cache/` の内容をすべて削除できます。

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
    allowed_tools: [search]  # 省略可。省略時は無制限、[] を指定すると全ツール禁止
```

- `command:`（stdio）と `url:`（streamable HTTP）はどちらか一方だけを指定します。両方または
  どちらも指定しない場合はエラーになります。
- `command`/`args`/`env` の値、`cwd`、`url`、`headers` の値は、いずれも `${VAR_NAME}` 展開の対象
  です（前節と同じ規則）。`allowed_tools` の値は展開対象ではありません。
- 実際に使われるサーバーだけがその場で接続されます（`mcp:` で名前を挙げていないサーバーは
  起動しません）。
- `allowed_tools:` は省略時（フィールドなし）は無制限、`[]`（空リスト）を指定するとそのサーバー
  の全ツールを禁止という意味になり、この2つは区別されます。モデルがそのサーバーの
  `allowed_tools` に無いツールを呼び出そうとすると、サーバーへ接続する前にエラーになります。
  詳しくは [MCP サーバーのツールを使う](./mcp.md#呼び出せるツールを制限するallowed_tools) を
  参照してください。

## `tool_policy`（ツール呼び出しの allow/deny）と `--approve-tools`

トップレベルの `tool_policy:` は、MCP サーバー・サブエージェント・[カスタムシェルツール](#カスタムシェルツール)
を横断してツール呼び出しを名前ベースで許可/拒否します。`--approve-tools` は呼び出し直前に対話的に
確認します。詳しくは
[MCP サーバーのツールを使う](./mcp.md#tool_policyallowdenyと---approve-tools対話的承認) を
参照してください。

```yaml
# lait.config.yml
tool_policy:
  allow: ["fetch_*"]
  deny: ["*__delete_*"]
```

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
- パスは、`mcp_servers:` とは異なり、その場では接続を持ちません。実際に使う最初の時点で
  ファイルを読み込み、その内容は一回の `lait` 実行中（workflow の反復を含む）キャッシュされます。
  実行中にファイルを変更しても、次の実行まで反映されません。

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
- パスは、`skills:` と同じく、その場では接続を持ちません。実際に使う最初の時点で
  Markdown を読み込み、一回の `lait` 実行中は同じ内容をキャッシュします。実行中にファイルを
  変更しても、次の実行まで反映されません。

## カスタムシェルツール

`tools:` にローカルコマンドを登録すると、MCP サーバーを立てずに `--tool`（チャット）・agent
ファイルの `tools:`・ワークフローノードの `tools:` から名前で参照して、モデルが呼び出せる
ツールとして使えます。詳しい使い方は [カスタムシェルツールを使う](./tools.md) を参照してください。

```yaml
# lait.config.yml
default:
  model: local
  tools: [ripgrep]   # 全経路（チャット / agent / workflow）の最終フォールバック

tools:
  ripgrep:
    description: "リポジトリ内をパターン検索する"
    command: ["rg", "--json", "{{ input.pattern }}"]
    parameters:
      type: object
      properties:
        pattern: { type: string }
      required: [pattern]
    timeout: 10   # 秒。省略時は30秒
```

- `command`（必須、空リスト不可）はシェルを介さず直接 exec されます。各要素はモデルの呼び出し
  引数を `input` として handlebars テンプレート展開されます（`{{ input.<field> }}`）。
- `parameters`（省略可、JSON オブジェクトである必要があります）はモデルに渡す JSON Schema です。
  省略時は引数なしのツールとして扱われます。
- ツール名は `tool__<名前>` に修飾されます。[`tool_policy`](#tool_policyツール呼び出しの-allowdenyと---approve-tools)
  や `--approve-tools` の対象です。

## ワークフローの登録と一覧表示

`workflows:` にワークフロー YAML ファイルを登録すると、`lait run <FILE>` の `<FILE>` を
パスの代わりに名前で指定できるようになります。詳しい使い方は
[ワークフロー(workflow.yml)](./workflow.md) を参照してください。

```yaml
# lait.config.yml
workflows:
  summarize: ./workflows/summarize.yml
```

```sh
lait run summarize "本文"
```

- `lait run <ARG>` の `<ARG>` がファイルとして存在する場合はそちらが優先されます(`workflows:`
  に同名のエントリがあっても無視され、その旨が標準エラー出力に注記されます)。ファイルとして
  存在しない場合にだけ `workflows:` から名前解決されます。
- `lait run <NAME>`/`lait workflow list`/`lait agent list`/`lait skill list` と、実際に
  `workflows:`/`agents:`/`skills:` を使う処理は、登録を定義した `lait.config.yml` 自身の
  ディレクトリを基準にパスを解決します。サブディレクトリから実行しても同じファイルが使われます。
- `lait workflow list`/`lait agent list`/`lait skill list` は、それぞれ `workflows:`/
  `agents:`/`skills:` に登録された名前・パス・説明(ワークフローの `description:`/
  エージェント・スキル Markdown ファイルの frontmatter の `description:`)を一覧表示します。
  パスが存在しない、または読み込みに失敗したエントリも(警告付きで)一覧には含まれます —
  登録内容そのものの妥当性チェックは `lait lint` が行います。
- `lait lint` は `workflows:` の各エントリについて、パスが実際に存在するかどうかも
  チェックします(渡されたワークフロー/エージェントファイルの静的チェックとは別に行われます)。
- `${VAR_NAME}` による環境変数展開は、`base_url`/`api_key`/`models[].provider.*`/
  `mcp_servers[]` の各フィールドのみが対象です。`workflows:` のパスは展開されません。
