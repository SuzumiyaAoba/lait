# MCP サーバーのツールを使う

[ドキュメント目次に戻る](./README.md)

`lait` は [MCP (Model Context Protocol)](https://modelcontextprotocol.io/) サーバーのクライアント
として動作し、`lait.config.yml` に登録した MCP サーバーのツールを、チャット・`lait agent run`・
`lait run`（workflow）のどの経路からもモデルに渡せます。モデルがツール呼び出しを返すと lait が
MCP サーバーへ転送して結果を受け取り、モデルに返す、というやり取りを最終回答が出るまで自動で
繰り返します（tool loop）。

```yaml
# lait.config.yml
default:
  model: local
  mcp: [filesystem]        # 全経路（チャット / agent / workflow）の最終フォールバック
  max_tool_rounds: 8

mcp_servers:
  filesystem:
    command: npx
    args: ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
```

```sh
# チャット
cargo run -- "/tmp のファイルを一覧して" --mcp filesystem

# agent
cargo run -- agent run agents/fs-agent.md '{"task":"/tmp のファイルを一覧して"}'

# workflow
cargo run -- run workflow.yml "調べたいこと"
```

## MCP サーバーの登録（`lait.config.yml`）

`mcp_servers:` に、`command:`（子プロセスとして起動する stdio サーバー）または `url:`
（streamable HTTP でリモート接続するサーバー）のどちらか一方を指定します。詳しい書式は
[設定ファイル](./config.md#mcp-サーバー) を参照してください。

```yaml
mcp_servers:
  filesystem:
    command: npx
    args: ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    env:
      SOME_TOKEN: ${SOME_TOKEN}
    cwd: ./work

  remote-search:
    url: https://mcp.example.com/mcp
    headers:
      Authorization: "Bearer ${SEARCH_TOKEN}"
```

実際に `mcp:` で参照されたサーバーだけがその場で接続されます。未登録の名前を `mcp:` に書くと、
`lait.config.yml` の `mcp_servers:` を案内するエラーになります。

## 呼び出せるツールを制限する（`allowed_tools`）

`mcp_servers:` の各サーバーは `mcp_servers.<name>.allowed_tools` の設定を持てます。省略時は
無制限（そのサーバーが持つ全ツールをモデルが呼び出せる、この設定が存在する前と同じ挙動）です。
一方、空リスト `[]` を明示すると「そのサーバーの全ツールを禁止」という意味になり、`null`/省略とは
区別されます。

```yaml
mcp_servers:
  filesystem:
    command: npx
    args: ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    allowed_tools: [read_file, list_directory]   # write_file 等はモデルから呼び出せない
```

この制限はツール一覧（`tools/list`）ではなく実行前（`tools/call` の直前）で強制されます。つまり
`allowed_tools` に含まれないツールもモデルには見えたままですが、実際に呼び出そうとするとサーバー
へ接続する前にエラーになり、そのモデルからの呼び出しは拒否されます。`lait.config.yml` の
`mcp_servers` は AGENTS.md が言うとおり「信頼されたコード」として扱われますが、実際にどのツール
を実行するかはモデルが選ぶため、`allowed_tools` はその選択に対するユーザー側のガードレールです。

`lait lint` は、あるノード/エージェントファイルが `mcp:` で参照しているサーバーの
`allowed_tools` が空リストの場合、警告を出します（そのサーバーへのツール呼び出しは実行前に
必ず拒否されるため、参照していること自体がほぼ設定ミスだからです）。`allowed_tools` が空でない
リストの場合は、モデルが実際にどのツールを呼び出すかを事前には知りようがないため、警告は出ません
（実行時にそのつど拒否されるだけです）。

## 各経路での指定方法

`mcp:`（使うサーバー名のリスト）と `max_tool_rounds:`（そのツール周回の上限回数、既定 8）は、
経路ごとに次のようにフォールバックします。各フィールドは独立してフォールバックします
（`retry` のようにブロック単位でフォールバックするものではありません）。

| 経路 | 優先順位 |
|---|---|
| チャット（`lait "prompt"`） | `--mcp`（CLI フラグ、複数指定可）→ `lait.config.yml` の `default.mcp` |
| `lait agent run` | agent ファイルの frontmatter `mcp:` → `lait.config.yml` の `default.mcp` |
| `lait run`（workflow） | ノードの `mcp:` → （`agent:` ノードなら）agent ファイルの `mcp:` → ワークフローの `default.mcp` → `lait.config.yml` の `default.mcp` |

```sh
lait "prompt" --mcp filesystem --mcp remote-search
```

```markdown
---
model: local
mcp: [filesystem]
max_tool_rounds: 8
---
{{ input.task }} を実行してください。
```

```yaml
# workflow.yml
nodes:
  research:
    type: prompt
    prompt: "{{ input }} について調べてください。"
    mcp: [filesystem, remote-search]
```

それぞれの詳細は [エージェント Markdown ファイル（agent.md）](./agent.md#mcp-ツールの利用) と
[ワークフロー（workflow.yml）](./workflow.md#mcp-ツールの利用mcp) にもあります。

## ツール名の扱い

モデルに渡すツール名は `<サーバー名>__<ツール名>`（例: `filesystem__read_file`）の形に修飾されます。
これは複数のサーバーが同名のツール（例: 両方に `search`）を持っていても衝突しないようにするため
です。OpenAI の function 名の制約（`^[a-zA-Z0-9_-]{1,64}$`）に合わせて、それ以外の文字は `_` に
置き換えられ、修飾後の名前が64文字を超える場合や、2つのツールが同じ修飾名になる場合はエラーに
なります。

## `structured_output` との併用

`output_schema`/`structured_output: true` と `mcp` は併用できます。ただし、多くの OpenAI
互換サーバーは厳密な `json_schema` の `response_format` を渡されると、スキーマ準拠の出力を強制し
`tool_calls` を一切返さなくなります。そのため lait は、ツールを呼び出している間は
`response_format` を送らず、モデルがツール呼び出しを止めた最後のラウンドだけ `response_format`
を付けて再送します（リクエストが1回増えます）。

## `--stream` との併用は未対応

`--stream`（または `mcp:` を指定したノード・agent の `complete_stream` 相当）と `mcp` を同時に
使うとエラーになります。ストリームの `tool_calls` は index 付きの断片として届くため、lait 側で
再組み立てする実装がまだありません。

## 上限とエラー

- `max_tool_rounds`（既定 8）に達してもモデルがツール呼び出しを止めない場合はエラーになり、
  ワークフロー/agent の実行がそこで止まります。無限ループを避けるための上限です。
- ワークフローの `retry` はノードの「ツール周回全体」を1つの単位として包みます。リトライが
  発生すると、副作用のあるツール呼び出し（ファイル書き込みなど）も含めてそのノードのツール
  周回全体がやり直されるため、副作用のあるツールをリトライ対象のノードで使う場合は注意して
  ください。

## 動作確認について

lait は LM Studio のローカルモデルを主な想定用途としていますが、ローカルモデルはツール呼び出し
（tool calling）への対応度が様々です。ツール呼び出しに対応していないモデルでは `tool_calls` が
一切返らず、`mcp:` を指定していてもツールは呼ばれません。ツール対応モデル（`qwen3` 系など）を
使って手元で確認してください。
