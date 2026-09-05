# 出力品質の評価（lait eval）

[ドキュメント目次に戻る](./README.md)

`lait eval` は、ワークフローまたはモデル+プロンプトテンプレートを実際のモデルに対して実行し、その出力がテストケースの期待(文字列包含・jq式・LLMによる採点)を満たすかどうかを評価します。

[決定的テスト（`lait test`）](./testing.md)が「制御フローが壊れていないか」を replay で確認する（API を一切呼ばない）のに対して、`lait eval` は「今のモデル/プロンプトの出力品質が保たれているか」を確認するための仕組みで、常に実際のモデルへ接続します。プロンプトやワークフローの変更が品質を下げていないかを確認する回帰テストとして、モデル更新が頻繁なローカル LLM 運用で特に有用です。

## 使い方

```sh
$ lait eval eval.yml
case 1: 3/3 (100%) PASS
3 of 3 case(s) fully passed
```

## eval.yml のスキーマ

```yaml
target:
  workflow: ./summarize.yml   # または model + prompt テンプレート
cases:
  - input: "..."
    assert:
      - type: contains        # 文字列包含
        value: "結論"
      - type: jq               # jq 式が真偽値として真であること
        expr: '.title | length > 0'
      - type: llm_judge        # LLM による採点
        criteria: "要約が原文の主旨を保っているか"
        model: gemma-4-12b
        threshold: 0.7
```

- `target:` は次のいずれか一方:
  - `workflow: <path>` — このeval.ymlファイルからの相対パスにあるワークフローファイルを、各ケースの`input`を初期入力として実行する。
  - `model: <name>`/`prompt: <template>` — `prompt:` は`{{ input }}`を参照できるテンプレートで、ケースごとにレンダリングして単発のモデル呼び出しを行う。
- `cases[].input`: そのケースの入力文字列。
- `cases[].assert`: そのケースの出力に対するアサーションのリスト。`lait test`の`assert:`と語彙を共有しており、`equals`(完全一致)も使えます。
  - `contains`: 出力に`value`が部分文字列として含まれること。
  - `jq`: `expr`を出力に対して評価し、真(jqの真偽値ルール — `false`/`null`以外はすべて真)であること。出力がJSONとしてパース可能ならそのJSON値、そうでなければ生文字列をJSON文字列値として評価対象にする。
  - `llm_judge`: `criteria`をもとにLLMに0.0〜1.0でスコアを付けさせ、`threshold`(省略時0.7)以上ならpass。`model`を省略した場合はevalの`target`が使っているモデル(`target.model`、またはワークフローの`default.model`)にフォールバックする。判定はStructured Outputsで`{"score": number, "reasoning": string}`を1回のモデル呼び出しで取得して行う。

## `--repeat N`

モデルの非決定性を考慮し、各ケースを`N`回実行してケースごとの成功率を表示します(省略時は1回)。

```sh
$ lait eval --repeat 5 eval.yml
case 1: 4/5 (80%) FAIL
  run 2: assertion 1: jq expression `contains("結論")` was false for output "..."
0 of 1 case(s) fully passed
```

いずれかのケースの成功率が100%未満の場合、`lait eval`全体の終了コードは非ゼロになります。

## `--format json`

```sh
$ lait eval --format json eval.yml
```

ケースごとに`case`/`input`/`passed`/`total`/`success_rate`/`runs`(各実行の`passed`/`failures`)を持つ配列を出力します。CI での自動判定に利用できます。
