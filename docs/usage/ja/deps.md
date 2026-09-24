# GitHub 上のファイルを依存として取り込む（lait deps）

[ドキュメント目次に戻る](./README.md)

`lait deps` サブコマンド群で、GitHub リポジトリに公開されたワークフロー YAML・エージェント
Markdown・スキル Markdown を、プロジェクトの依存として取り込めます。`Cargo.toml`/`package.json`
と同じ発想で、宣言したい依存は `lait.deps.yml`（手編集可能なマニフェスト）に、実際に解決された
コミットと内容のハッシュは `lait.lock`（生成ファイル）に記録されます。取り込んだファイルは
`.lait/deps/` に実体化され、`workflows:`/`agents:`/`skills:` の登録エントリと同じように名前で
参照できます。

```sh
# 依存の追加: ref（ブランチ/タグ/コミット）は @ で指定
lait deps add owner/repo/workflows/review.yml@main

# 名前解決された workflow としてそのまま実行できる
lait run review "対象のテキスト"

# CI などで lock どおりに再現したいとき
lait deps install --frozen

# 依存の内容が lock と一致するか検査
lait deps verify
```

## コマンド

| コマンド | 説明 |
| --- | --- |
| `lait deps add <SPEC> [--name NAME] [--ref REF] [--kind KIND]` | GitHub 上のファイルを取得し、`lait.deps.yml` に要求を、`lait.lock` に解決結果（コミットと SHA-256）を記録します。`NAME` 省略時はファイル名（拡張子なし）から、`SKILL.md` の場合は親ディレクトリ名から導出します。 |
| `lait deps install [--frozen]` | `lait.deps.yml` の全エントリを `lait.lock` の内容どおりに `.lait/deps/` へ実体化します。lock が要求をカバーしていない（manifest が編集された等）場合は再解決して lock を更新します。`--frozen` を付けるとその場合にエラーになります。 |
| `lait deps update [NAME...]` | 指定した依存（省略時はすべて）の ref を再解決し、指すコミットが変わっていれば lock と実体を更新します。 |
| `lait deps remove <NAME>` | 依存を `lait.deps.yml`・`lait.lock`・`.lait/deps/` から取り除きます。 |
| `lait deps list` | 宣言された依存とその状態（installed / not installed / modified / not locked / stale）を一覧します。 |
| `lait deps verify` | `.lait/deps/` の各ファイルの SHA-256 が `lait.lock` と一致するか検査します。不一致・未インストール・lock 未収録のエントリがあると非ゼロで終了します。 |

`lait.deps.yml` は `lait.config.yml` と同じく、カレントディレクトリから親ディレクトリへ向かって
探索されます。`lait deps add` はマニフェストがなければカレントディレクトリに作成します。

## 依存ソースの指定（SPEC）

`lait deps add` の `<SPEC>` と `lait.deps.yml` の `source:` フィールドは、次のいずれかの形式を
受け付けます。

| 形式 | 例 |
| --- | --- |
| 短縮形 `OWNER/REPO/PATH[@REF]` | `lait deps add octo-org/lait-files/workflows/review.yml@v1` |
| `github:` プレフィックス（`deps add` がマニフェストに書き戻す正規形） | `github:octo-org/lait-files/workflows/review.yml` |
| github.com のファイル URL（`blob`/`raw`/`tree`） | `https://github.com/octo-org/lait-files/blob/main/workflows/review.yml` |
| raw.githubusercontent.com の URL | `https://raw.githubusercontent.com/octo-org/lait-files/main/workflows/review.yml` |

`REF`（ブランチ・タグ・コミット）を省略するとリポジトリのデフォルトブランチを使います。
URL 形式の ref スラッシュ区切りは1セグメントだけなので、`feature/x` のような `/` を含む
ブランチ名は `@REF` または `--ref` で指定してください。`PATH` 中の `@`（例:
`icons/gear@2x.yml`）との衝突は「末尾の `@`」ルールで解決されるため、曖昧な場合も `--ref` が
確実です。

`--kind`（`workflow`/`agent`/`skill`）を省略した場合、パスの拡張子から推定します。

| パス | 推定される kind |
| --- | --- |
| `*.yml`・`*.yaml` | `workflow` |
| `*.md`（`SKILL.md` 以外） | `agent` |
| `*/SKILL.md` | `skill` |
| その他 | 推定不能（`--kind` が必須） |

## ファイルの構成

| ファイル/ディレクトリ | 役割 | コミット対象 |
| --- | --- | --- |
| `lait.deps.yml` | 依存の宣言（何を・どの ref で取り込むか）。手編集可能です。 | はい |
| `lait.lock` | `deps add`/`install`/`update` が生成する解決結果（コミット SHA・SHA-256・実体化パス）。 | はい |
| `.lait/deps/<name>/<file>` | 実体化された依存ファイル。`.lait/` は `.gitignore` 済みの実行時領域です。 | いいえ |

`lait.deps.yml` の各フィールドは次のとおりです。

| フィールド | 説明 |
| --- | --- |
| `version` | マニフェスト形式のバージョン（現在 `1`）。省略時は `1` として読みます。 |
| `deps.<name>` | 依存名 = 取り込み後の登録名。`lait run <name>` などに使う文字（英数字・`-`・`_`）のみ。 |
| `deps.<name>.source` | 必須。上表のいずれかの形式の GitHub ソース指定。 |
| `deps.<name>.ref` | 省略可。ブランチ・タグ・コミット。省略時はデフォルトブランチを追跡します。 |
| `deps.<name>.kind` | 省略可。`workflow`/`agent`/`skill`。省略時は `source` のパスから推定します。 |

`lait.lock` の各エントリのフィールドは次のとおりです（手編集は想定していません）。

| フィールド | 説明 |
| --- | --- |
| `kind` | `deps add` 時に決定した kind（workflow/agent/skill）。 |
| `repo` | `"owner/repo"` 形式のリポジトリ。 |
| `path` | リポジトリ内のファイルパス。 |
| `ref` | 要求された ref（省略 = デフォルトブランチ）。 |
| `commit` | `ref` が解決されたコミット SHA。`install` は ref を再解決せず、このコミットから取得します。 |
| `sha256` | 実体化したファイル内容の SHA-256（小文字 hex）。`verify`/`install` が照合します。 |
| `file` | 実体化ファイルのマニフェストからの相対パス（`.lait/deps/<name>/<file>`）。 |

## 名前解決と優先順位

取り込まれた依存は、kind に応じて既存のレジストリに登録されたのと同じ扱いになります。

| kind | 使い方 |
| --- | --- |
| `workflow` | `lait run <name>`、`lait workflow list`、workflow 内からの参照。 |
| `agent` | `lait agent run <name>`、`subagents: [name]`、`lait agent list`。 |
| `skill` | `skills: [name]`（agent frontmatter・workflow ステップ・`default.skills`）、`lait skill list`。 |

`workflows:`/`agents:`/`skills:` のマージ順は `グローバル設定 < lait.deps.yml < プロジェクトの
lait.config.yml` です。同名のエントリが `lait.config.yml` にあればそちらが勝つため、依存を
ローカルファイルで一時的に差し替える（シャドウする）こともできます。`lait run`/`lait agent run`
の引数が実在するファイルならレジストリ名よりファイルが優先される既存の規則もそのままです
（`lait agent run` も今回から `workflows:` と同じ名前解決を行います）。

`--no-config` を指定した実行では `lait.deps.yml` のマージも行われません。

## GitHub へのアクセス

依存の取得には GitHub REST API を使い、`ref` をコミット SHA に解決してから、そのコミットで
ファイルをダウンロードします。パブリックリポジトリはトークンなしで利用できますが
（GitHub の未認証レート制限あり）、プライベートリポジトリにはトークンが必要です。

| 環境変数 | 説明 |
| --- | --- |
| `GITHUB_TOKEN` | `Authorization: Bearer` として送信するアクセストークン。 |
| `GH_TOKEN` | `GITHUB_TOKEN` が未設定のときの代替。 |
| `GITHUB_API_URL` | API のベース URL（既定 `https://api.github.com`）。GitHub Enterprise ではこの値を設定します。 |

いずれも `.env` に書けます（`.env` は `.gitignore` 済みです）。依存として取り込まれる
ファイルは信頼できない入力として扱われ、`kind` に応じたパーサー（workflow/agent/skill それぞれの
既存の検証）を通過しない内容は登録されません。エージェントやワークフローの依存は、ツール・
サブエージェント・MCP サーバー起動といった権限を持ちうる実行設定を含み得るため、公開元を
確認してから追加してください。

## 典型的な運用

- ローカルでは `lait deps add`/`update` で依存を宣言し、`lait.deps.yml` と `lait.lock` を
  コミットします。
- CI や新しいクローンでは `lait deps install --frozen` で lock どおりに再現し、
  `lait deps verify` で内容を検査します。
- 上流の `main` が動いたときに追従するには `lait deps update`（特定の依存だけなら
  `lait deps update <NAME>`）を実行し、lock の差分をコミットします。

`lait.deps.yml` の補完・検証には [`lait schema deps`](./schema.md) が出力する JSON Schema
（`schemas/deps.json`）を使えます。

関連: [設定ファイル](./config.md)、[ワークフロー（workflow.yml）](./workflow.md)、
[エージェント Markdown ファイル（agent.md）](./agent.md)、[スキルを使う](./skills.md)、
[JSON Schema でエディタ補完（lait schema）](./schema.md)
