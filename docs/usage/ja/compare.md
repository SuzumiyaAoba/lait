# モデル比較（lait compare）

[ドキュメント目次に戻る](./README.md)

`lait compare` は同じプロンプトを複数のモデルへ並行に送信し、応答・所要時間・usage をモデルごとに区切って表示します。ローカル LLM の選定で「同じプロンプトでどのモデルが良いか」を確認したいときに使います。

## 使い方

```sh
$ lait compare --model gemma-4-12b --model qwen-3-14b "日本の首都はどこ?"
=== gemma-4-12b (gemma-4-12b-it) ===
time: 812ms
usage: prompt=12 completion=8 total=20
日本の首都は東京です。

=== qwen-3-14b (qwen3-14b-instruct) ===
time: 1340ms
usage: prompt=12 completion=15 total=27
日本の首都は東京（Tokyo）です。
```

- `--model` は2回以上指定する必要があります（`lait.config.yml` の `models:` エイリアス名、またはサーバーが受け付けるモデル ID をそのまま指定できます）。
- PROMPT は省略して標準入力から渡すこともできます（`git diff | lait compare --model a --model b "このdiffをレビューして"` のように、他のPROMPT系サブコマンドと同じ規約です）。
- リクエストは並行に送信されます。1つのモデルが失敗しても他のモデルの結果は表示され、いずれか1つでも失敗すると終了コードは非ゼロになります。
- `--model` に指定したエイリアスに[`pricing:`](./config.md#コスト概算pricing)が設定されていれば、`usage:` の行に概算USDコストが併記されます(例: `usage: prompt=12 completion=8 total=20 ($0.0001)`)。設定されていないモデルはコストを表示しません。

## サンプリングパラメータの一律適用

`--reasoning-effort`/`--temperature`/`--top-p`/`--max-tokens` を指定すると、各モデル自身の設定（`lait.config.yml` の `models:` エントリが持つ既定値）を上書きして、比較する全モデルに同じ値が一律適用されます。指定しない場合は各モデルの既定値がそのまま使われます。

```sh
$ lait compare --model gemma-4-12b --model qwen-3-14b --temperature 0 "厳密に比較したいプロンプト"
```

## システムプロンプトとツール

次のオプションは、比較する全モデルに同じ値が一律適用されます。

| オプション | 説明 |
| --- | --- |
| `--system <TEXT>` | 全モデルに送るシステムプロンプト。 |
| `--system-file <FILE>` | システムプロンプトをファイルから読み込みます（`--system` とは同時に指定できません）。 |
| `--mcp <NAME>` | 全モデルが呼び出せる `mcp_servers:` のエントリ。複数指定できます。 |
| `--subagent <NAME>` | 全モデルがサブエージェントとして呼び出せる `agents:` のエントリ。複数指定できます。 |
| `--tool <NAME>` | 全モデルが呼び出せる `tools:` のエントリ。複数指定できます。 |

`--system`/`--system-file` を省略した場合は、チャットと同じく `lait.config.yml` の
`default.system` が使われます。`--mcp`/`--subagent`/`--tool` を省略した場合も、それぞれ
`default.mcp`/`default.subagents`/`default.tools` にフォールバックします。ツールを指定すると、
各モデルが独立に tool loop を回すため、`time`/`usage` はツール呼び出しを含む全ラウンドの合計です。

```sh
$ lait compare --model a --model b --system "日本語で簡潔に" --tool ripgrep "TODO を数えて"
```

## `--markdown`

`--markdown` を指定すると、モデルごとの所要時間・usage・コスト・成否をまとめた表の後に、各モデルの
応答をモデルごとの見出しの下に並べた Markdown を出力します。比較結果を PR やドキュメントに
貼り付けたいときに使います（`--json` とは同時に指定できません）。

```markdown
| model | model_id | time | prompt | completion | total | cost | status |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| gemma-4-12b | gemma-4-12b-it | 812ms | 12 | 8 | 20 | - | ok |
| qwen-3-14b | qwen3-14b-instruct | 1340ms | 12 | 15 | 27 | - | ok |

## gemma-4-12b (gemma-4-12b-it)

日本の首都は東京です。
```

## `--json`

機械可読な出力が必要な場合は `--json` を付けます。各モデルの結果を1要素とする配列が返り、成功時は `error` が `null`、失敗時は `content`/`usage`/`cost_usd` が `null` になります。`cost_usd` は該当モデルに `pricing:` が設定されていない場合も `null` です(コスト0ではなく「不明」を表します)。

```sh
$ lait compare --model gemma-4-12b --model qwen-3-14b --json "..." | jq '.[].model'
```

```json
[
  {
    "model": "gemma-4-12b",
    "model_id": "gemma-4-12b-it",
    "duration_ms": 812,
    "usage": {"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20},
    "cost_usd": null,
    "content": "日本の首都は東京です。",
    "error": null
  },
  {
    "model": "qwen-3-14b",
    "model_id": "qwen3-14b-instruct",
    "duration_ms": 1340,
    "usage": {"prompt_tokens": 12, "completion_tokens": 15, "total_tokens": 27},
    "content": "日本の首都は東京（Tokyo）です。",
    "error": null
  }
]
```

## 制限事項

- `--stream` には対応していません（複数モデルのストリームを同時に表示する仕組みが複雑になるため）。
- `--approve-tools`（対話的なツール承認）には対応していません。`tool_policy` は通常どおり効きます。
- `--file`/`--image` による添付には対応していません。
