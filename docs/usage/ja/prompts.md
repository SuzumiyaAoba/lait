# 名前付きプロンプトテンプレート（prompts）

[ドキュメント目次に戻る](./README.md)

`lait.config.yml` の `prompts:` に名前付きプロンプトテンプレートを登録し、CLI から呼び出せます。「毎回同じ前置きを付けて呼ぶ」ような用途に、ワークフローファイルを用意するほどではない場合に向いています。

```yaml
prompts:
  translate:
    template: "次の文章を{{ vars.lang }}に翻訳して:\n\n{{ input }}"
    model: gemma-4-12b   # 任意: このプロンプト専用の既定モデル
    vars:
      lang: 日本語        # 任意: {{ vars.<key> }} の既定値
```

> [!NOTE]
> テンプレートエンジンは Handlebars です。Jinja のような `{{ lang | default: "日本語" }}` というフィルタ構文は使えないため、既定値は上記のように `prompts.<name>.vars` に書き、`--var` で上書きする形にしています。

## 実行方法

```sh
lait -p translate "Hello"                 # PROMPT/stdin が {{ input }} になる
lait -p translate "Hello" --var lang=英語  # vars.lang を上書き
lait prompt run translate "Hello"         # サブコマンド形式(同じ結果)
lait prompt list                          # 設定済みプロンプトの一覧
```

2つの実行方法の違いは次のとおりです。

| 実行方法 | 使えるオプション | モデル・エンドポイントの解決元 |
| --- | --- | --- |
| `-p`/`--prompt-name`（通常のチャット呼び出し `lait [OPTIONS] PROMPT` のオプションの一つ） | `--model`/`--stream`/`-o`/`--json` など既存のオプションと自由に組み合わせられます | `--model` > `prompts.<name>.model` > `default.model` |
| `lait prompt run <name> [INPUT]` | `--var`/`--show-usage`/`--no-history`/`-o`/`--render`/`--json` に対応する、より単純な入口です（`--stream`/`--mcp`/`--subagent` はありません） | `prompts.<name>.model`/`default.model`/`lait.config.yml` の `base_url`/`api_key` からのみ |

細かい制御が必要な場合は `-p` を使ってください。

- `--var KEY=VALUE` は繰り返し指定でき、同じキーを複数回指定すると最後の値が使われます。
- 標準入力からの読み込み（パイプ）にも対応しています（`git diff | lait -p commit-message` のような使い方を想定）。
