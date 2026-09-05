# モデル比較（`lait compare`）

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

## サンプリングパラメータの一律適用

`--reasoning-effort`/`--temperature`/`--top-p`/`--max-tokens` を指定すると、各モデル自身の設定（`lait.config.yml` の `models:` エントリが持つ既定値）を上書きして、比較する全モデルに同じ値が一律適用されます。指定しない場合は各モデルの既定値がそのまま使われます。

```sh
$ lait compare --model gemma-4-12b --model qwen-3-14b --temperature 0 "厳密に比較したいプロンプト"
```

## `--json`

機械可読な出力が必要な場合は `--json` を付けます。各モデルの結果を1要素とする配列が返り、成功時は `error` が `null`、失敗時は `content`/`usage` が `null` になります。

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

- 初期版では `--stream` に対応していません（複数モデルのストリームを同時に表示する仕組みが複雑になるため）。
- `--mcp`/`--tool`/`--subagent`/`--system` など、単発チャットが持つツール呼び出し系オプションは今回のスコープ外です。単純なプロンプト送信の比較のみに対応します。
