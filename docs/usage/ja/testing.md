# 決定的テスト（record & replay / lait test）

[ドキュメント目次に戻る](./README.md)

ワークフローが複雑化(分岐・ループ・サブワークフロー)すると、リグレッション検知の手段が [lint](./lint.md) だけでは足りなくなります。`lait run --record`/`--replay` と `lait test` は、実際のモデル API を呼ばずにワークフローの制御フローを決定的に検証する仕組みです。

## `lait run --record <DIR>`

ワークフロー実行中に送信した全ての LLM リクエスト/レスポンスを、`<DIR>` 以下にカセットファイルとして保存します。

```sh
$ lait run workflow.yml "プロンプト" --record ./cassettes
```

カセットファイルは、ベース URL・モデル・サンプリングパラメータ・メッセージ履歴・ツール定義・response format から計算したハッシュをファイル名にして保存されます(`--cache` が使うキーと同じ計算方法)。API キーはハッシュにもファイル内容にも含まれません。

## `lait run --replay <DIR>`

`--record` で作った `<DIR>` を指定すると、API を一切呼ばずに、記録済みレスポンスをリクエスト内容に基づいて返します。

```sh
$ lait run workflow.yml "プロンプト" --replay ./cassettes
```

- リクエストの内容(ベース URL・モデル・メッセージ履歴・ツール定義・response format)が記録時と完全に一致した場合のみカセットがヒットします。ワークフローや `--var` を変更してリクエスト内容が変わると、以前のカセットはヒットしなくなります。
- **未記録のリクエストはエラーになります**。ネットワークへは一切アクセスしません — 「記録し忘れたリクエストが静かに実サーバーへ飛んでしまう」ことはありません。
- `--record`/`--replay` は同時に指定できません。

## `lait test <FILE_OR_DIR>...`

テスト定義 YAML を一括実行し、pass/fail を報告します。ファイル/ディレクトリの両方を指定でき、ディレクトリは再帰的に `.yml`/`.yaml` を探索します(隠しファイル・隠しディレクトリはスキップ)。

### テスト定義ファイルのスキーマ

```yaml
# tests/summarize-case1.yml
workflow: ../summarize.yml   # このファイルからの相対パス
input: "要約したい長文..."     # 省略可(既定は空文字列)
vars:                         # 省略可。{{ vars.<key> }} に渡される
  lang: ja
replay: ./cassettes/case1     # `lait run --record` で事前に作ったディレクトリへの相対パス(必須)
assert:
  - type: equals              # 完全一致
    value: "期待する完全な出力"
  - type: jq                  # jq 式が真になること
    expr: 'contains("結論")'
```

- `workflow:`/`replay:` はこのテスト定義ファイル自身のディレクトリからの相対パスとして解決されます。
- `assert:` の各項目は上から順にすべて評価されます。`type: jq` の `expr` は、出力テキストがそのまま有効な JSON であればその値に対して、そうでなければ JSON 文字列としてラップした値に対して評価されるので、`contains("...")` のような文字列アサーションと `.title | length > 0` のような構造化アサーションのどちらも書けます。
- 実行はワークフロー全体を `--replay` 相当で走らせるので、記録されていないリクエストに当たった場合はそのテストファイル自体が失敗として報告されます(他のテストファイルの実行は継続します)。

### 実行例

```sh
$ lait test tests/
tests/summarize-case1.yml: PASS
tests/summarize-case2.yml: FAIL
  assertion 1: jq expression `contains("結論")` was false for output "まだ途中です"
1 passed, 1 failed, 2 total
```

### `--format json`

```sh
$ lait test --format json tests/ | jq '.[] | {file, status}'
```

各テストファイルにつき `file`/`status`(`"pass"`/`"fail"`)/`failures`(失敗理由の配列。pass なら空)を持つ配列を返します。

### 終了コード

1件でも fail があれば非ゼロで終了します(CI 向け)。

## 典型的なワークフロー

1. 実際のモデルに対して一度 `lait run workflow.yml "入力" --record tests/cassettes/case1` を実行し、期待する応答をカセットとして記録する。
2. `tests/case1.yml` にそのカセットを参照するテスト定義を書き、`assert:` で期待する出力を固定する。
3. 以降はモデルを呼ばずに `lait test tests/` で、ワークフローの変更が制御フロー・テンプレート・jq 加工などを壊していないかを検証する(CI で毎回実行できる)。
4. モデル自体の出力品質(要約が的確か等)の回帰検知は、record & replay とは別の観点として、別途 `lait eval` 相当の仕組みで扱います。
