# 実行トレース（lait run --trace-file / lait trace show）

[ドキュメント目次に戻る](./README.md)

`lait run --trace-file <PATH>` は、そのワークフロー実行が送信した全てのモデル補完リクエストと
ツール呼び出し(MCP・サブエージェント・カスタムシェルツール、`tool_policy`/`--approve-tools` に
より拒否されたものも含む)を、`<PATH>` に JSONL(1行1イベント)として書き出します。`--show-usage`
がトークン使用量を人間向けに要約するのに対して、`--trace-file` はワークフローの**実行軌跡**
(どのステップが・いつ・何回・どのツールを呼び、どのくらい時間がかかったか)を、後から `jq` などで
機械的に検査したり、`lait trace show` で一覧表示したりするためのものです。

## 使い方

```sh
$ lait run workflow.yml "プロンプト" --trace-file trace.jsonl
$ lait trace show trace.jsonl
[    42ms] chat         summarize — 14:03:11.204  (gen_ai.request.model=workflow-model lait.source=live gen_ai.usage.input_tokens=120 gen_ai.usage.output_tokens=48)
[     3ms] execute_tool tool 'tool__ripgrep' — 14:03:11.260  (gen_ai.tool.name=tool__ripgrep lait.tool.round=1 lait.tool.decision=allowed)
```

`<PATH>` の親ディレクトリが存在しない場合は自動的に作成されます。ファイルはワークフロー全体の
実行が終わった後に一括で書き出されます(`--checkpoint` のようにステップごとに追記されるものでは
ありません — 実行がクラッシュ以外の理由で失敗した場合、それまでに記録したイベントは書き出され
ません)。

## イベントの形式

各行は1つの JSON オブジェクトで、次のフィールドを持ちます。

| フィールド | 内容 |
|---|---|
| `seq` | 記録順を表す通番(0始まり)。並行実行(`parallel`/`for_each` の `max_concurrency` > 1)されたイベント同士のタイムスタンプ順序が前後することがあるため、読み出し順序はこちらを基準にします。 |
| `operation` | イベント種別。`"chat"`(モデル補完リクエスト)・`"execute_tool"`(MCP・サブエージェント・シェルツールの呼び出し)・`"compact"`([ツール周回の要約による圧縮](./compaction.md)が発生した箇所)のいずれか。 |
| `label` | そのイベントを引き起こしたもの — ワークフローのステップ id、または `tool '<修飾済みツール名>'`。`--show-usage` が使うラベルと同じ値です。 |
| `start` / `end` | ISO 8601 形式のタイムスタンプ(UTC)。 |
| `duration_ms` | `end - start` をミリ秒に丸めたもの。 |
| `attributes` | イベント種別ごとの追加情報(下記)。 |

`attributes` のキーは、[OpenTelemetry の GenAI Semantic Conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/)
(`gen_ai.*`。2026年現在まだ策定中の規約です)にできるだけ合わせています。これは将来 OTLP
エクスポータを追加する際に属性名をそのまま転用できるようにするための選択で、現時点では
エクスポータ自体は実装されていません。`lait.*` で始まるキーは lait 独自の項目で、GenAI
Semantic Conventions に対応する項目がないものです。

### `"chat"` イベントの `attributes`

| キー | 内容 |
|---|---|
| `gen_ai.request.model` | リクエスト先のモデル id。 |
| `gen_ai.usage.input_tokens` / `gen_ai.usage.output_tokens` | サーバーが `usage` を報告した場合のみ設定されます。 |
| `lait.source` | `"live"`(実際にネットワークへ送信)/`"cache"`(`--cache` のヒット)/`"replay"`(`--replay` のヒット)。 |

### `"execute_tool"` イベントの `attributes`

| キー | 内容 |
|---|---|
| `gen_ai.tool.name` | 修飾済みツール名(`mcp__<server>__<tool>`/`agent__<name>`/`tool__<name>`)。 |
| `gen_ai.tool.arguments` | モデルが渡した生の JSON 引数文字列。 |
| `lait.step.label` | そのツール呼び出しを発生させたモデル呼び出し自身のラベル(`label` フィールドと同じ値 — `tool_called` アサーションの `args_jq` はこの値ではなくこちらの引数を評価します)。 |
| `lait.tool.round` | そのツール呼び出しが何ラウンド目の tool loop で発生したか(1始まり)。 |
| `lait.tool.decision` | `"allowed"`/`"denied"`。`tool_policy` の deny か `--approve-tools` での拒否かは区別されません。 |
| `lait.tool.denial_reason` | `"denied"` の場合のみ設定される、モデルに返された拒否理由の文字列。 |

### `"compact"` イベントの `attributes`

| キー | 内容 |
|---|---|
| `lait.compaction.round` | 圧縮が発生したラウンド番号(1始まり)。 |
| `lait.compaction.messages_before` / `lait.compaction.messages_after` | 圧縮前後のメッセージ件数。 |

圧縮自体が送信する要約用リクエストは、この `"compact"` イベントとは別に、通常の `"chat"`
イベントとしても記録されます([ツール周回の要約による圧縮](./compaction.md)を参照)。

## 記録されないもの

- **span の親子構造は記録されません。** これは意図的な設計です — すべてのイベントは `label` で
  ワークフローステップやサブエージェント名に紐付いているので、`label` でグルーピングすれば
  「どのステップが何回モデルを呼び、何回ツールを呼んだか」は再構成できます。呼び出しの入れ子構造
  そのもの(サブエージェントがさらに別のサブエージェントを呼ぶ、など)を正確な親子関係として
  残したい場合は、現時点ではこの仕組みの対象外です。
- **ストリーミング(`--stream`)応答のイベントは記録されません。** ワークフローの `prompt`/`agent`
  ステップは常に非ストリーミングでモデルを呼ぶため、`lait run --trace-file` の対象になるのはこの
  経路だけです。チャット(`lait "prompt"`)・`lait agent run`・`lait test`/`lait eval` は今のところ
  `--trace-file` を持ちません。
- コスト(金額)は記録されません。トークン数のみです。

## `lait trace show`

```sh
$ lait trace show trace.jsonl            # 人間向けの一覧
$ lait trace show --json trace.jsonl      # パース済みの JSON 配列
```

`--json` を指定すると、各行をパースした `TraceEvent` の配列を整形済み JSON として出力します。
`jq` と組み合わせて機械的に検査する場合は、ファイルを直接読んでも構いません(`--json` は
`lait` 自身の目視確認向けの整形出力です)。

```sh
$ jq -s 'map(select(.operation == "execute_tool" and .attributes["lait.tool.decision"] == "denied"))' trace.jsonl
```
