# ワークフロー/エージェントを MCP サーバーとして公開する（lait serve --mcp）

[ドキュメント目次に戻る](./README.md)

`lait serve --mcp` は、`lait.config.yml` の `agents:`/`workflows:` に登録した各エントリを、
1つずつ呼び出し可能な MCP ツールとして公開する MCP サーバーです。既定では標準入出力（stdio）で
通信し、`--http` を指定すると Streamable HTTP で待ち受けます。
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

## HTTP で公開する（`--http`）

`--http <ADDR>` を指定すると、標準入出力の代わりに MCP の Streamable HTTP トランスポートで
`ADDR` を待ち受けます。起動すると、接続先 URL が標準エラー出力に表示されます（ポートに `0` を
指定すると空いているポートが自動で選ばれます）。

```sh
lait serve --mcp --http 127.0.0.1:8765
# note: lait serve --mcp: ready at http://127.0.0.1:8765/mcp with 2 tool(s): ...
```

MCP クライアントには `http://127.0.0.1:8765/mcp` を登録します（パスは問いませんが、`/mcp` を
推奨します）。複数のクライアントが同時に接続でき、それぞれが独立したセッションになります。

**認証はありません。** 既定のループバックアドレス（`127.0.0.1` など）で待ち受け、外部に公開する
場合は認証を行うリバースプロキシなどを前段に置いてください。DNS リバインディング対策として、
`Host` ヘッダーが `localhost`/`127.0.0.1`/`::1` 以外のリクエストは拒否します。別のホスト名で
アクセスさせる場合は `--allowed-host <HOST>`（`example.com` または `example.com:8765`、複数指定可）
で許可してください。

## 実行時オプション

| オプション | 説明 |
| --- | --- |
| `--http <ADDR>` | 標準入出力の代わりに Streamable HTTP で `ADDR` を待ち受けます（上記参照）。 |
| `--allowed-host <HOST>` | `--http` で受け付ける `Host` ヘッダーの値を追加します。複数指定できます。 |
| `--cache`/`--no-cache`（グローバル） | 各ツール呼び出しのモデルリクエストでレスポンスのディスクキャッシュを使う／使わない。省略時は `default.cache` に従います。 |
| `--record <DIR>` | ツール呼び出しのモデルリクエスト/レスポンスを `DIR` にカセットとして記録します（`lait run --record` と同じ。[決定的テスト](./testing.md)参照）。 |
| `--replay <DIR>` | モデルリクエストを `DIR` のカセットから再生し、ネットワークには接続しません（`lait run --replay` と同じ）。 |
| `--trace-file <PATH>` | 各ツール呼び出しのモデル呼び出し・ツール呼び出しを JSONL のトレースとして書き出します（[実行トレース](./trace.md)参照）。サーバーは起動し続けるため、ツール呼び出しが終わるたびにそれまでの全イベントでファイルを書き直します。各イベントの `attributes` には、どの MCP ツールの呼び出しによるものかを示す `lait.serve.tool` が付きます。 |

## 公開されるツール

| 登録元 | ツール名 | 引数スキーマ |
| --- | --- | --- |
| `agents:` の各エントリ | `agent__<名前>`（例: `agent__reviewer`） | そのエージェントファイル自身の `input_schema:`（[エージェント Markdown ファイル](./agent.md#入力の検証input_schema)を参照）がそのまま使われます。`input_schema:` を持たないエージェントは、[サブエージェント](./subagents.md)と同じ汎用スキーマ（`{"input": "..."}`、文字列または JSON）になります。 |
| `workflows:` の各エントリ | `workflow__<名前>`（例: `workflow__release_notes`） | `{"input": "...", "inputs": {...}}` です。`input` は `lait run <名前> <PROMPT>` の `PROMPT` に対応する文字列で（ワークフローの `input_schema:` に従って解釈されます）、ワークフローが `inputs:` を宣言していなければ必須です。`inputs` は宣言された `inputs:` の値のオブジェクトで、各入力のスキーマがそのままプロパティのスキーマになり、`default` を持たない入力は必須になります。 |

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

## 制約・スコープ外

- **`lait serve` は起動時に一度だけ `lait.config.yml` を読み込みます。** サーバー起動後に
  `agents:`/`workflows:` を追記しても、再起動するまでは反映されません（他のすべての `lait` コ
  マンドと同じ「1回の起動につき1回の設定読み込み」という前提です）。
- **`ask:` ステップを含むワークフローは公開されません。** `lait serve --mcp` では標準入出力が
  MCP の JSON-RPC 通信そのものに使われているため、`ask:` ステップ（[ワークフロー](./workflow.md
  #ask--人間に尋ねる)）が対話端末からの回答を待とうとすると、MCP の通信と衝突しかねません。
  `ask:` ステップは非対話的な標準入力（実際の MCP クライアントが `lait serve` を子プロセスとして
  起動する場合は常にこちら）に対してはもともと `default:` へフォールバックする安全策を持って
  いますが、`lait serve --mcp` を対話端末から直接手動で試す場合はその安全策が効かず、標準入力
  の奪い合いになり得ます。このため、公開対象を組み立てる際に `ask:` ステップを1つでも含む
  （制御ステップの内側に入れ子になったものも含む）ワークフローは無条件に除外し、`note:` 行で
  その理由を表示します（このチェックはそのワークフローファイル自身のステップだけを見ます —
  `workflow:` ステップで呼び出す別ファイル側の `ask:` ステップまでは検出しません）。
- **`--approve-tools`（対話的なツール承認）は使えません。** 同じ理由（標準入力が MCP 通信専用。
  `--http` の場合も、承認に応答する人間がいない）により、served なエージェント/ワークフロー自身の tool loop は常に非対話的に実行されます。
  `tool_policy`（allow/deny）は通常どおり効きます。
- **`lait history`・`--show-usage` には記録されません。** served な呼び出しは、通常の
  `lait run`/`lait agent run` が経由する出力・履歴記録の経路（`report::emit_run_output` など）
  を通らず、結果テキストを MCP クライアントへ直接返すだけです。
- **ワークフローの `WorkflowFile`/`WorkflowScope` はツール呼び出し間でキャッシュされません。**
  `workflow__<名前>` ツールが呼ばれるたびに、そのワークフロー YAML を毎回読み直します（モデル
  呼び出し自体のレイテンシに比べれば YAML の再パースは無視できるコストと判断しています）。
  `agents:` 側は既存の `AgentRegistry` のキャッシュをそのまま再利用します。
