# 決定的テスト（record & replay / lait test）

[ドキュメント目次に戻る](./README.md)

ワークフローが複雑化(分岐・ループ・サブワークフロー)すると、リグレッション検知の手段が [lint](./lint.md) だけでは足りなくなります。`lait run --record`/`--replay` と `lait test` は、記録済みの LLM 応答を再利用してモデル API へのリクエストを再送せず、ワークフローの制御フローを検証する仕組みです。

テスト定義と、それが参照する workflow・設定ファイルの読み込み中も Ctrl-C で実行を中断できます。
実行時に読み込む各ファイルは最大16 MiBです。

## `lait run --record <DIR>`

ワークフロー実行中に送信した全ての LLM リクエスト/レスポンスを、`<DIR>` 以下にカセットファイルとして保存します。

```sh
$ lait run workflow.yml "プロンプト" --record ./cassettes
```

カセットファイルは、ベース URL・モデル・サンプリングパラメータ・メッセージ履歴・ツール定義・response format から計算したハッシュをファイル名にして保存されます(`--cache` が使うキーと同じ計算方法)。API キーはハッシュにもファイル内容にも含まれません。

## `lait run --replay <DIR>`

`--record` で作った `<DIR>` を指定すると、LLM API を呼ばずに、記録済みレスポンスをリクエスト内容に基づいて返します。ワークフロー内の MCP サーバー、command ノード、shell tool、ファイル操作などは通常どおり実行されるため、それらも含めて外部 I/O を止めたい場合はテスト用の fixture/mock を用意してください。

```sh
$ lait run workflow.yml "プロンプト" --replay ./cassettes
```

- リクエストの内容(ベース URL・モデル・メッセージ履歴・ツール定義・response format)が記録時と完全に一致した場合のみカセットがヒットします。ワークフローや `--var` を変更してリクエスト内容が変わると、以前のカセットはヒットしなくなります。
- **未記録の LLM リクエストはエラーになります**。LLM API へのネットワーク fallback はありません — 「記録し忘れたリクエストが静かに実モデルへ飛んでしまう」ことはありません。MCP などワークフロー側の外部 I/O はこの制限の対象外です。
- `--record`/`--replay` は同時に指定できません。

## `lait test <FILE_OR_DIR>...`

テスト定義 YAML を一括実行し、pass/fail を報告します。ファイル/ディレクトリの両方を指定でき、ディレクトリは再帰的に `.yml`/`.yaml` を探索します(隠しファイル・隠しディレクトリはスキップ)。探索結果はパス順に並び、同じ実体を複数の引数(ディレクトリとファイルの重複など)から指定しても1回だけ実行されます。

探索時は通常ファイルだけを対象にし、シンボリックリンクをたどりません。明示したファイルは拡張子に関係なくテスト定義として扱いますが、明示したパスがシンボリックリンクまたは特殊ファイルの場合はエラーになります。ディレクトリ内で見つかったシンボリックリンクと特殊ファイルは無視されるため、循環リンクによって探索が終わらなくなったり、FIFO・ソケットなどを誤ってテストとして読み込んだりすることはありません。

複数のテストファイルは最大8件まで同時に実行されます。各ファイルはそれぞれ独立したワークフロー実行なので、あるファイルの `workflow:` に `write_file:` を持つノードがあり、かつ別のテストファイルの `workflow:` も**同じ相対パス**に書き込む場合、両者が同時に走ると競合します(`for_each` の `max_concurrency` が2以上の本体で `write_file` を禁止しているのと同種の問題ですが、こちらは静的検査の対象外です)。テスト定義ごとに書き込み先を分けるか、`ScratchDir` 相当の使い捨てディレクトリを使ってください。

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
- 実行はワークフロー全体を `--replay` 相当で走らせるので、記録されていない LLM リクエストに当たった場合はそのテストファイル自体が失敗として報告されます(他のテストファイルの実行は継続します)。MCP や command などの外部 I/O を使うワークフローは、決定的にするために fixture/mock を参照する構成にしてください。

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
3. 以降はモデルを呼ばずに `lait test tests/` で、ワークフローの変更が制御フロー・テンプレート・jq 加工などを壊していないかを検証する(CI で毎回実行できる)。MCP や command などを含む場合は、外部 I/O を fixture/mock に固定する。
4. モデル自体の出力品質(要約が的確か等)の回帰検知は、record & replay とは別の観点として、別途 `lait eval` 相当の仕組みで扱います。
