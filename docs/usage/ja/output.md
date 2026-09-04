# 出力例

[ドキュメント目次に戻る](./README.md)

リクエストが成功すると、モデルの応答テキストが標準出力に表示されます。応答内容はロードしたモデルによって異なります。

```text
こんにちは。今日はどのようなお手伝いができますか？
```

## `--stream`（ストリーミング応答）

`--stream` を指定すると、モデルの応答をまとめて待たずに、生成された分から順に標準出力へ書き出します。API へは `stream: true` を送信し、サーバーが返す SSE（Server-Sent Events）のチャンクを逐次読み取って表示します。

```sh
cargo run -- --stream --model "モデル ID" "Rustについて一文で説明してください。"
```

`--show-reasoning` と併用すると、推論内容のチャンクを先に（`Reasoning:` ヘッダー付きで）、回答本文のチャンクをその後に書き出します。`--show-reasoning` を指定しない場合、推論内容のチャンクは表示されません。

応答全体を一つの JSON オブジェクトとして出力する `--json` は、逐次出力とは両立しないため `--stream` と同時には指定できません。`--json-schema` との併用は可能ですが、Structured Outputs の JSON はチャンク単位の断片として届くため、完成した JSON として読みたい場合は `--stream` を外してください。

`--mcp`/`--subagent`（[MCP サーバー](./mcp.md)/[サブエージェント](./subagents.md)）とも併用できます。ツールを呼び出すラウンドを含め、各ラウンドのテキストは届いた分から順に表示され、ツール呼び出しが尽きたラウンドの内容が最終応答になります。

## `--json-schema`（Structured Outputs）

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

## `--json`（CLI応答のJSON出力）

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

## `--render`（TTY 向け Markdown レンダリング）

`--render` を指定すると、応答の Markdown（見出し・リスト・強調・コードブロック・テーブルなど）を端末向けに装飾して表示します（`termimad` クレートを使用）。`lait.config.yml` の `default.render: true` で既定を変更できます。

```sh
lait --render --model "モデル ID" "Markdown で回答して"
```

- 標準出力が端末（TTY）でない場合（パイプ・リダイレクト）は自動的に生テキストにフォールバックします。
- `--json` の出力には影響しません（機械可読な JSON はそのまま）。
- `--stream` と併用した場合、現状は装飾されずストリーミングの生テキスト表示のままになります。
