# サブエージェントを使う

[ドキュメント目次に戻る](./README.md)

`lait` は、エージェント Markdown ファイル（[エージェント Markdown ファイル（agent.md）](./agent.md)）
を「サブエージェント」——モデル自身が実行時に呼び出すかどうかを判断できるツール——として、
チャット・`lait agent run`・`lait run`（workflow）のどの経路からもモデルに渡せます。モデルが
サブエージェントのツール呼び出しを返すと lait がそのエージェントファイルを実行して結果を受け取り、
モデルに返す、というやり取りを最終回答が出るまで自動で繰り返します（[MCP サーバーのツールを使う]
(./mcp.md) と同じ tool loop の仕組みです）。

`agent:`/`workflow:` ワークフローノードが「このステップでは必ずこのエージェント（サブワーク
フロー）を呼ぶ」という静的な配線であるのに対し、サブエージェントは「モデルが必要だと判断した
ときだけ呼ぶ」という動的な委譲です。複数の専門エージェントを用意しておき、オーケストレーター役の
モデルにタスクを振り分けさせる、といった構成に向いています。

```yaml
# lait.config.yml
default:
  model: local
  subagents: [researcher]  # 全経路（チャット / agent / workflow）の最終フォールバック

agents:
  researcher: agents/researcher.md
```

```markdown
<!-- agents/researcher.md -->
---
name: researcher
description: 与えられたトピックについて調査し、要点をまとめる
model: local
---
次のトピックについて調べて、要点を3行でまとめてください。

{{ input }}
```

```sh
# チャット
cargo run -- "量子コンピュータの最新動向についてレポートして" --subagent researcher

# agent
cargo run -- agent run agents/orchestrator.md '{"task":"量子コンピュータの最新動向"}'

# workflow
cargo run -- run workflow.yml "量子コンピュータの最新動向"
```

## サブエージェントの登録（`lait.config.yml`）

`agents:` に、エージェント Markdown ファイルへのパスを登録します。詳しい書式は
[設定ファイル](./config.md#サブエージェント) を参照してください。

```yaml
agents:
  researcher: agents/researcher.md
  fact-checker: agents/fact-checker.md
```

実際に `subagents:` で参照されたエージェントだけが、その呼び出しのツール一覧に加わります。
未登録の名前を `subagents:` に書くと、`lait.config.yml` の `agents:` を案内するエラーになります。

## 各経路での指定方法

`subagents:`（使うサブエージェント名のリスト）は、経路ごとに次のようにフォールバックします。
各フィールドは独立してフォールバックします（`retry` のようにブロック単位でフォールバックする
ものではありません）。

| 経路 | 優先順位 |
|---|---|
| チャット（`lait "prompt"`） | `--subagent`（CLI フラグ、複数指定可）→ `lait.config.yml` の `default.subagents` |
| `lait agent run` | agent ファイルの frontmatter `subagents:` → `lait.config.yml` の `default.subagents` |
| `lait run`（workflow） | ノードの `subagents:` → （`agent:` ノードなら）agent ファイルの `subagents:` → ワークフローの `default.subagents` → `lait.config.yml` の `default.subagents` |

```sh
lait "prompt" --subagent researcher --subagent fact-checker
```

```markdown
---
model: local
subagents: [researcher]
---
{{ input.task }} について、必要であれば researcher に調査を任せてください。
```

```yaml
# workflow.yml
nodes:
  triage:
    type: prompt
    prompt: "{{ input }} について調べてください。"
    subagents: [researcher, fact-checker]
```

それぞれの詳細は [エージェント Markdown ファイル（agent.md）](./agent.md#サブエージェントの利用) と
[ワークフロー（workflow.yml）](./workflow.md#サブエージェントの利用subagents) にもあります。

## ツール名とツール引数の扱い

モデルに渡すツール名は `agent__<サブエージェント名>`（例: `agent__researcher`）の形に修飾されます。
これは MCP ツール（`<サーバー名>__<ツール名>`）と衝突しないようにするためです。OpenAI の function
名の制約（`^[a-zA-Z0-9_-]{1,64}$`）に合わせて、それ以外の文字は `_` に置き換えられ、修飾後の名前が
64文字を超える場合はエラーになります。

サブエージェントのツール引数（`parameters`）は、そのエージェントファイルの `input_schema` の
有無で変わります。

- **`input_schema` がある場合**: モデルの引数はそのスキーマがそのまま `parameters` になり、
  引数オブジェクト全体がサブエージェントの `{{ input }}` になります（`{{ input.field }}` で
  各フィールドにアクセスできます）。
- **`input_schema` がない場合**: `{ "input": ... }` という汎用の1フィールドのツール引数になり、
  その `input` の値（文字列であればそのままプレーンテキストとして、それ以外（オブジェクト・配列
  など）であれば JSON テキストとして）がサブエージェントの `{{ input }}` になります。これは
  `lait agent run <FILE> <INPUT>` の `INPUT` と同じ規則です。

エージェントファイルの `description`（frontmatter）は、モデルに見せるツールの説明文としてそのまま
使われます。省略した場合は `Runs the '<名前>' subagent for a delegated task.` という既定の説明文に
なります。モデルが適切にサブエージェントを使い分けられるよう、`description` は具体的に書くことを
おすすめします。

## サブエージェントの入れ子

サブエージェント自身の frontmatter にも `subagents:` を書けます。つまりサブエージェントが、
さらに別のサブエージェントを呼び出すような入れ子の委譲も可能です。ただし、循環参照
（あるサブエージェントが巡り巡って自分自身を呼ぶ）や、過度に深い入れ子は、`workflow:` の
入れ子と同じ考え方でエラーになります（現在の上限は16段）。

サブエージェント自身の `model`/`reasoning_effort`/`mcp`/`skills`/`subagents` などは、そのエージェント
ファイル自身の frontmatter → `lait.config.yml` の `default:` の順に解決されます。呼び出し元の
CLI フラグ（`--base-url`/`--api-key` など）や `--model` は、サブエージェント自身の呼び出しには
引き継がれません。複数の呼び出し元から同じサブエージェントを使い回す場合は、`lait.config.yml`
（あるいはエージェントファイル自身の frontmatter）に必要な設定をまとめておいてください。

## `agent:`/`workflow:` ワークフローノードとの違い

| | `agent:`/`workflow:` ノード | サブエージェント（`subagents:`） |
|---|---|---|
| いつ呼ばれるか | そのステップで必ず呼ばれる（静的な配線） | モデルが必要だと判断したときだけ呼ばれる（動的な委譲） |
| 呼び出し単位 | ワークフローの1ステップ | tool loop の中の1回のツール呼び出し（複数回・0回もありうる） |
| 入力の渡し方 | 前のステップの出力（`{{ input }}`） | モデルが組み立てたツール引数 |

同じエージェント Markdown ファイルを、あるワークフローでは `agent:` ノードとして固定的に呼びつつ、
別の呼び出しでは `subagents:` に登録してモデルに判断を委ねる、という両方の使い方ができます。

## `mcp`/`--stream`/`structured_output` との関係

サブエージェントは MCP ツールと同じ tool loop の仕組みに乗るため、制約もほぼ同じです。

- `output_schema`/`structured_output: true` と `subagents` は併用できます。`mcp` と同様、ツールを
  呼び出している間は `response_format` を送らず、モデルがツール呼び出しを止めた最後のラウンドだけ
  `response_format` を付けて再送します。
- `--stream` と `subagents` は併用できます。`mcp` と同様、ストリームの `tool_calls` は
  ラウンドごとに再組み立てしてから実行されます。
- `max_tool_rounds`（既定 8）に達してもモデルがツール呼び出しを止めない場合はエラーになります。
  サブエージェント自身の呼び出しも、この回数のうち1回のツール呼び出しとしてカウントされます
  （サブエージェント自身の内部の tool loop は、サブエージェント自身の `max_tool_rounds` で
  別途管理されます）。
- 同じ名前が `mcp`（MCP サーバーのツール）と `subagents`（サブエージェント）の両方で修飾後に
  衝突した場合はエラーになります（通常は起こりません。「ツール名の扱い」を参照）。

関連: [エージェント Markdown ファイル（agent.md）](./agent.md)、
[ワークフロー（workflow.yml）](./workflow.md)、[MCP サーバーのツールを使う](./mcp.md)、
[設定ファイル](./config.md#サブエージェント)
