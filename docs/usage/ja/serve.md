# ワークフロー/エージェントを MCP サーバーとして公開する（lait serve --mcp）

[ドキュメント目次に戻る](./README.md)

`lait serve --mcp` は、`lait.config.yml` の `agents:`/`workflows:` に登録した各エントリを、
1つずつ呼び出し可能な MCP ツールとして公開する、標準入出力（stdio）ベースの MCP サーバーです。
Claude Code や Claude Desktop のような MCP クライアントの設定に `lait serve --mcp` を子プロセス
として登録すれば、それらのエージェント/ワークフローを他のツールの部品として使わせられます。

```yaml
# lait.config.yml
base_url: "http://localhost:1234/v1"
default:
  model: local
agents:
  reviewer: agents/reviewer.md
workflows:
  release_notes: workflows/release_notes.yml
```

```sh
lait serve --mcp
```

MCP クライアント側の設定例（Claude Desktop の `claude_desktop_config.json` 相当）:

```json
{
  "mcpServers": {
    "lait": {
      "command": "lait",
      "args": ["serve", "--mcp"],
      "cwd": "/path/to/your/project"
    }
  }
}
```

## 公開されるツール

| 登録元 | ツール名 | 引数スキーマ |
| --- | --- | --- |
| `agents:` の各エントリ | `agent__<名前>`（例: `agent__reviewer`） | そのエージェントファイル自身の `input_schema:`（[エージェント Markdown ファイル](./agent.md#入力の検証input_schema)を参照）がそのまま使われます。`input_schema:` を持たないエージェントは、[サブエージェント](./subagents.md)と同じ汎用スキーマ（`{"input": "..."}`、文字列または JSON）になります。 |
| `workflows:` の各エントリ | `workflow__<名前>`（例: `workflow__release_notes`） | 常に `{"input": "..."}` 固定です（`lait run <名前> <INPUT>` の `INPUT` に対応する、ワークフロー自身は `input_schema:` の概念を持ちません）。 |

- ツールの `description` は、エージェントファイルの `description:`／ワークフローファイルの
  トップレベル `description:` から取られます（未設定なら `Run the '<名前>' agent/workflow.`
  という既定文）。
- ツール呼び出しの実行結果（最終出力テキスト）がそのままツール結果として返ります。実行の途中
  経過（`report::note` などの進捗表示）は出しません — 後述のとおり標準出力/標準エラー出力は
  プロトコル用途のためです。

`agents:`/`workflows:` に登録されていても、名前が OpenAI/MCP のツール名制約（64文字以内、
英数字/`_`/`-`のみ）に収まらない場合や、ファイルの読み込み・パースに失敗する場合は、そのエント
リだけを飛ばして起動を続けます（`lait workflow list`/`lait skill list` と同じ寛容さです）。
どのエントリが飛ばされたかは標準エラー出力の `note:` 行に出ます。

## 制約・スコープ外（v1）

- **`lait serve` は起動時に一度だけ `lait.config.yml` を読み込みます。** サーバー起動後に
  `agents:`/`workflows:` を追記しても、再起動するまでは反映されません（他のすべての `lait` コ
  マンドと同じ「1回の起動につき1回の設定読み込み」という前提です）。
- **`ask:` ノードを含むワークフローは公開されません。** `lait serve --mcp` では標準入出力が
  MCP の JSON-RPC 通信そのものに使われているため、`ask:` ノード（[ワークフロー](./workflow.md
  #対話的ユーザー入力type-ask)）が対話端末からの回答を待とうとすると、MCP の通信と衝突しかねません。
  `ask:` ノードは非対話的な標準入力（実際の MCP クライアントが `lait serve` を子プロセスとして
  起動する場合は常にこちら）に対してはもともと `default:` へフォールバックする安全策を持って
  いますが、`lait serve --mcp` を対話端末から直接手動で試す場合はその安全策が効かず、標準入力
  の奪い合いになり得ます。このため、公開対象を組み立てる際に `ask:` ノードを1つでも含む
  ワークフローは無条件に除外し、`note:` 行でその理由を表示します（このチェックはそのワーク
  フローファイル自身の `nodes:` だけを見ます — `workflow:` ノードで参照する別ファイル側の
  `ask:` ノードまでは検出しません）。
- **`--approve-tools`（対話的なツール承認）は使えません。** 同じ理由（標準入力が MCP 通信専用）
  により、served なエージェント/ワークフロー自身の tool loop は常に非対話的に実行されます。
  `tool_policy`（allow/deny）は通常どおり効きます。
- **`lait history`・`--show-usage` には記録されません。** served な呼び出しは、通常の
  `lait run`/`lait agent run` が経由する出力・履歴記録の経路（`report::emit_run_output` など）
  を通らず、結果テキストを MCP クライアントへ直接返すだけです。
- **ワークフローの `WorkflowFile`/`WorkflowScope` はツール呼び出し間でキャッシュされません。**
  `workflow__<名前>` ツールが呼ばれるたびに、そのワークフロー YAML を毎回読み直します（モデル
  呼び出し自体のレイテンシに比べれば YAML の再パースは無視できるコストと判断しています）。
  `agents:` 側は既存の `AgentRegistry` のキャッシュをそのまま再利用します。
- 対応しているのは stdio 経由のみです。HTTP 経由の MCP サーバー（`transport-streamable-http-
  server`）は未対応です。
- `--cache`/`--record`/`--replay`/`--trace-file` など、他の `lait` コマンドが持つ実行時オプ
  ションは `lait serve` にはありません。
