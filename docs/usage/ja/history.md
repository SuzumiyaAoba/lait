# 実行履歴（lait history）

[ドキュメント目次に戻る](./README.md)

単発チャット・`lait chat` の各ターン・エージェント・ワークフロー・名前付きプロンプトの実行結果を、成功したものだけ自動的に記録します。「さっき良い応答が出たプロンプトをもう一度見たい」という場合に、シェル履歴では残らない応答側も含めて後から探せます。

## 記録先

既定で `$XDG_DATA_HOME/lait/history.jsonl`（`XDG_DATA_HOME` が未設定なら `$HOME/.local/share/lait/history.jsonl`）に、実行ごとに1行の JSON（日時・種別・モデル・プロンプト・応答・usage）を追記します。

## 一覧・確認・検索

```sh
lait history                 # 直近の実行を新しい順に一覧表示(既定20件)
lait history --limit 50      # 表示件数を変更
lait history show 1          # 1番(最新)の実行の全文を表示
lait history search 翻訳      # プロンプト・応答の全文検索(大小文字を区別しない)
```

`show`/`search` の番号は一覧に表示される番号（`1` が最新）と対応しています。

## 記録の無効化

- `--no-history`: その回だけ記録しない（チャット・`lait chat`・`lait agent run`・`lait run`・`lait prompt` のいずれでも指定できます）。
- `lait.config.yml` の `default.history: false`: 既定で記録しないようにする。秘匿情報を扱う場合などに使ってください。

## 記録される内容についての注意

- 記録は実行が成功した場合のみ行われます。
- `model` はワークフロー実行では記録されません（複数ステップで異なるモデルを使う可能性があるため）。
- `usage` はサーバーが usage を報告した場合のみ記録されます。`lait chat` のストリーミングターンで `--show-usage` を指定していない場合も usage は記録されません（トークン数を都度取得するための追加リクエストは行わないため）。
