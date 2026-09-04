# 実行履歴（lait history）

[ドキュメント目次に戻る](./README.md)

単発チャット・`lait chat` の各ターン・エージェント・ワークフロー・名前付きプロンプトの実行結果を、
成功したものだけ自動的に記録します。プロンプトだけでなく応答も残るため、後から検索できます。

## 記録先

履歴はユーザー共通の JSON Lines ファイルに保存されます。

- `XDG_DATA_HOME` が設定されている場合: `$XDG_DATA_HOME/lait/history.jsonl`
- `XDG_DATA_HOME` が未設定の場合: `$HOME/.local/share/lait/history.jsonl`

実行ごとに1行の JSON を追記します。会話セッション（`.lait/sessions/`）とは保存場所と用途が異なります。

## 一覧・確認・検索

```sh
lait history                 # 直近の実行を新しい順に一覧表示(既定20件)
lait history --limit 50      # 表示件数を変更
lait history show 1          # 1番(最新)の実行の全文を表示
lait history search 翻訳      # プロンプト・応答の全文検索(大小文字を区別しない)
```

`show`/`search` の番号は一覧に表示される番号（`1` が最新）と対応しています。

出力は次のような形式です（日時・モデル名・内容は例です）。一覧と検索ではプロンプトの先頭だけが
表示され、全文は `show` で確認します。

```text
$ lait history
1    2026-09-04T10:20:30+00:00    chat    local-model    Rustについて説明して...

$ lait history show 1
timestamp: 2026-09-04T10:20:30+00:00
kind: chat
model: local-model

prompt:
Rustについて説明して

response:
Rustは安全性と性能を両立した言語です。
```

`lait history search <QUERY>` も一覧と同じ形式で、プロンプトまたは応答に一致した実行を表示します。

## 記録の無効化

- `--no-history`: その回だけ記録しません。チャット・`lait chat`・`lait agent run`・`lait run`・
  `lait prompt run` のいずれでも指定できます。
- `lait.config.yml` の `default.history: false`: 既定で記録しないようにします。

## 記録される内容についての注意

### 記録される項目

各行には次の項目が含まれます。

- `timestamp`: 実行が完了した時刻（UTC）
- `kind`: `chat`、`agent`、`workflow`、`prompt` のいずれか
- `model`: 使用したモデル。ワークフローでは複数ステップで異なるモデルを使えるため記録されません
- `prompt` / `response`: 送信したプロンプトとモデルの応答
- `usage`: サーバーが報告したトークン使用量（取得できた場合のみ）

記録は実行が成功した場合のみ行われます。`lait chat` のストリーミングターンで
`--show-usage` を指定していない場合も、usage は記録されません。トークン数を取得するための
追加リクエストは行わないためです。

### 機密情報の注意

履歴にはプロンプトと応答が平文で保存されます。API キーや個人情報などを含む内容を扱う場合は、
その実行に `--no-history` を付けるか、`default.history: false` を設定してください。設定を変更しても、
すでに保存された履歴は削除されません。
