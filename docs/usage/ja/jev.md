# Jev 互換 API で判定する（decide / lait decide）

[ドキュメント目次に戻る](./README.md)

[Jev](https://typesafe.ai/blog/introducing-system-one-models-and-jev) は TypeSafe AI の
「System One」モデルです。文章を生成する代わりに、1つの状態（`state`）について、あらかじめ
型を決めた質問に確率つきで答えます。lait はこの判定 API（`POST /v1/systemone`）と、それと
互換のサーバー（オープンモデルによる再実装やモックサーバーなど）に対応しています。

- ワークフローの [`decide:` ステップ](#ワークフローの-decide-ステップ) — 流れてきた値を
  `state` として送り、答えをステップの値にします。`switch` と組み合わせたルーティングや
  分類に使えます。
- [`lait decide`](#lait-decide) — 同じ質問定義をコマンドラインから1回だけ投げて、結果を
  JSON で表示します。質問を試しながら作るのに使えます。

## 接続先の設定（`jev:`）

`lait.config.yml` のトップレベルに `jev:` を書きます。チャット用のトップレベル
`base_url`/`api_key` とは独立しています（OpenAI 互換のチャット API は `/systemone` を
提供しないため、フォールバックもしません）。

```yaml
# lait.config.yml
jev:
  base_url: https://api.typesafe.ai/v1   # 省略時の既定値
  api_key: "${TYPESAFE_API_KEY}"
  model: jev-latest                      # 省略時の既定値
```

| フィールド | 既定値 | 内容 |
| --- | --- | --- |
| `base_url` | `https://api.typesafe.ai/v1` | API のベース URL。リクエストは `{base_url}/systemone` に送られます。他の `base_url` と同じく、末尾はバージョン（`/v1`）までです。 |
| `api_key` | なし | `Authorization: Bearer <key>` として送る API キー。未設定ならヘッダーを送りません（認証なしのローカルサーバー向け）。 |
| `api_key_cmd` | なし | API キーを外部コマンドから取得します（[設定ファイル](./config.md#api_key_cmd-による外部コマンドからのシークレット取得)と同じ形式）。`api_key` と同時には指定できません。 |
| `model` | `jev-latest` | リクエストの `model`。ステップの `model:` や `--model` で上書きできます。 |

`base_url`/`api_key` には `${VAR_NAME}` による環境変数参照が使えます。グローバル設定と
プロジェクト設定の両方に `jev:` がある場合はフィールドごとにプロジェクト側が優先され、
`api_key`/`api_key_cmd` はトップレベルと同じく組として扱われます。

## 質問の書き方

質問は「質問 id → 定義」のマッピングです。`decide:` ステップと `lait decide -q` の
ファイルで同じ形を使います。

```yaml
urgent:
  type: noul
  instructions: 今日中に対応が必要か
  criteria:
    true: 今日中に対応が必要
    false: 後回しでよい
team:
  type: choice
  instructions: どのチームの担当か
  criteria:
    billing: 支払い・請求書
    sales: null            # 名前だけで十分なら null
anger:
  type: score
  criteria: [落ち着いている, 不満がある, 非常に怒っている]
```

| `type` | 答え | `criteria` |
| --- | --- | --- |
| `noul` | はい/いいえの確率（`noul`: 0〜1） | 省略可。`true`/`false` の説明（片方だけでも可） |
| `choice` | 選ばれた選択肢（`choice`）と各選択肢の確率（`probabilities`）、`confidence` | 必須。選択肢名 → 説明のマッピング（2〜255個、説明は `null` 可） |
| `score` | 期待値（`score` = Σ i·pᵢ）、各レベルの確率（`probabilities`、キーは `"0"`〜）、`legend`、`confidence` | 必須。低い順に並べたレベルの説明のリスト（2〜10個） |

- `instructions` とそれぞれの説明には、文字列だけでなくマッピングやリストも書けます。
- 未知のフィールド、選択肢やレベルの数の超過、`score` の `criteria` をマッピングで書く、
  といった誤りは、リクエストを送る前（ワークフローの読み込み時・`lait lint` 時）にエラーに
  なります。
- YAML で `true:`/`false:` と書いたキーは真偽値として読まれますが、lait が文字列の
  `"true"`/`"false"` に変換して送ります。

## ワークフローの `decide:` ステップ

```yaml
# triage.yml
inputs:
  ticket: { type: object }
steps:
  - id: triage
    input: $inputs.ticket        # これが state として送られる
    decide:
      urgent:
        type: noul
        instructions: 今日中に対応が必要か
      team:
        type: choice
        criteria:
          billing: 支払い・請求書
          sales: null
  - switch:
      - when: .urgent.noul > 0.8 and .team.confidence > 0.5
        steps:
          - prompt: "{{ steps.triage.team.choice }} チーム向けにエスカレーション文を書いてください: {{ inputs.ticket }}"
    else:
      - jq: '"queue"'
```

| フィールド | 内容 |
| --- | --- |
| `decide` | 質問の定義（前節の形式）。必須です。 |
| `model` | このステップだけで使う Jev のモデル名。`jev.model` より優先されます。`models:` の alias や `default.model`（チャットモデル用）とは無関係です。 |

- ステップへの入力（`input:` で計算した値、省略時は流れてきた値）が `state` になります。
  文字列・オブジェクト・配列はそのまま送り、数値や真偽値はテキスト形式にして、`null` は
  空文字列にして送ります。
- ステップの値は応答の `answers` オブジェクトで、キーの順序は質問の宣言順です。
  `$steps.triage.team.choice` や `.urgent.noul` のように参照できます。
- 共通フィールド（`id`/`when`/`input`/`output`/`retry`/`timeout`/`on_error`）が使えます。
  `decide` はモデル呼び出しなので、`default.retry`/`default.timeout` も適用されます。
- `lait run --dry-run` では質問 id と型、`lait graph` では質問 id がノードに表示されます。

## `lait decide`

```sh
lait decide -q questions.yml "請求が二重に発生しています"
echo '{"subject": "返金", "body": "二重請求"}' | lait decide -q questions.yml --json
```

| オプション | 内容 |
| --- | --- |
| `-q`, `--questions <FILE>` | 質問定義の YAML/JSON ファイル（必須）。 |
| `STATE` | 判定対象。省略するか `-` を渡すと標準入力から読みます（引数と標準入力の両方があれば連結されます）。 |
| `--json` | `STATE` を JSON としてパースして送ります（既定は文字列のまま）。 |
| `--model <MODEL>` | Jev のモデル名（既定は `jev.model`、未設定なら `jev-latest`）。 |
| `--full` | `answers` だけでなく、サーバーの応答全体（`model`/`answers`/`usage`）を表示します。 |
| `--base-url <URL>` / `--api-key <KEY>` | `jev:` の設定を上書きします（`${VAR_NAME}` は展開されません）。 |

出力は整形済みの JSON です（既定では質問の宣言順に並べた `answers`）。

## エラーと応答の検査

- HTTP ステータスが 2xx 以外のときは、応答の `detail.message`（認証エラーなど）や
  `detail[].msg`（422 の検証エラー）をエラーメッセージに含めて失敗します。
- 応答に `answers` がない、質問に対応しない答えがある・足りない、答えの `type` が質問と
  違う、型ごとの値（`noul`/`choice`/`score`）が欠けている場合もエラーになります。
- それ以外のフィールド（`probabilities`、`confidence`、`x_` で始まる拡張フィールドなど）は
  検証も再計算もせず、そのまま渡します。`confidence` の計算式はサーバー実装によって異なる
  ことが知られているので、しきい値はサーバーごとに調整してください。
- タイムアウトは他の LLM リクエストと同じ 1 回 300 秒です。HTTP レベルの自動再試行は
  しないため、再試行が必要ならステップの `retry:` を使ってください。

## 制約（v1）

- `--show-usage`・`lait history`・`--trace-file` には記録されません。応答の `usage` は
  `lait decide --full` で確認できます。
- レスポンスのディスクキャッシュ（`--cache`）の対象外です。
- `--record`/`--replay`・`lait test` のカセットには記録されません。`--replay` 中に
  `decide:` ステップを実行するとネットワークに出ずにエラーになります。`lait test` で
  ワークフローを検証したい場合は、`decide` ステップに `when:` を付けて入力で切り替えるなど
  してください。
- `lait doctor` は `jev.base_url`/`jev.api_key` の環境変数参照だけを検査し、Jev サーバーへの
  接続確認はしません。
