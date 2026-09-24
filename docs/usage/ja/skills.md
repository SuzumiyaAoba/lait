# スキルを使う

[ドキュメント目次に戻る](./README.md)

`lait` は、Markdown ファイル1つで定義する「スキル」を、`lait agent run`・`lait run`
（workflow）のどちらの経路からもモデルのシステムプロンプトに追記できます。スキルはレビュー観点
やコーディング規約のような、複数のエージェント/ステップで使い回したい指示のかたまりを1か所にまとめて
おくためのものです。

```yaml
# lait.config.yml
default:
  model: local
  skills: [code-review]     # 全経路（agent / workflow）の最終フォールバック

skills:
  code-review: skills/code-review.md
```

```markdown
<!-- skills/code-review.md -->
---
name: code-review
description: 差分レビューの観点
---
- 境界値・off-by-one エラーを疑う
- エラー処理が握りつぶされていないか確認する
```

```sh
# agent
cargo run -- agent run agents/reviewer.md '{"diff":"..."}'

# workflow
cargo run -- run workflow.yml "レビューしてほしい差分"
```

## スキルファイルの形式

スキルファイルは、エージェント Markdown ファイル（[エージェント Markdown ファイル
（agent.md）](./agent.md)）と同じく、1行目が必ず `---` で始まり、次に現れる `---` 行までが
frontmatter（YAML）、それ以降が本文（Markdown）になります。

```markdown
---
name: code-review
description: 差分レビューの観点
---
- 境界値・off-by-one エラーを疑う
```

frontmatter のフィールドは次のとおりです。

| フィールド | 説明 |
| --- | --- |
| `name` | 任意。省略した場合は、`lait.config.yml` の `skills:` でそのファイルに付けたエントリ名が使われます。 |
| `description` | 任意。スキルの説明。 |

本文は handlebars テンプレートとしてレンダリングされません。エージェントファイルの
システムプロンプトとは異なり、`{{ }}` を含むコード例などをそのまま書けます。

## スキルの登録（`lait.config.yml`）

`skills:` に、スキルファイルへのパス、またはそのファイルを含むディレクトリへのパスを指定します。

```yaml
skills:
  code-review: skills/code-review.md
  style-guide: skills/style-guide/       # ディレクトリを指定すると SKILL.md を読む
```

| 値 | 解決 |
| --- | --- |
| ファイルへのパス | そのファイルがスキル Markdown として使われます。 |
| ディレクトリへのパス | その直下の `SKILL.md` が使われます（Anthropic の Agent Skills の慣習（`<name>/SKILL.md`）に合わせたもので、既存の `.claude/skills/<name>/` のようなディレクトリをそのまま指せます）。 |

パスは `lait.config.yml` のあるディレクトリからの相対パスとして解決されます。未登録の名前を
`skills:`（agent ファイル／ワークフローのステップ／`default:`）に書くと、`lait.config.yml` の
`skills:` を案内するエラーになります。

## 各経路での指定方法

`skills:`（使うスキル名のリスト）は、経路ごとに次のようにフォールバックします。

| 経路 | 優先順位 |
|---|---|
| `lait agent run` | agent ファイルの frontmatter `skills:` → `lait.config.yml` の `default.skills` |
| `lait run`（workflow） | ステップの `skills:` → （`agent:` ステップなら）エージェント定義の `skills:` → ワークフローの `default.skills` → `lait.config.yml` の `default.skills` |
| チャット（`lait "prompt"`） | `lait.config.yml` の `default.skills` のみ（CLI フラグはありません） |

```markdown
---
model: local
skills: [code-review]
---
{{ input.diff }} をレビューしてください。
```

```yaml
# workflow.yml
steps:
  - prompt: "{{ input }} をレビューしてください。"
    skills: [code-review, style-guide]
```

それぞれの詳細は [エージェント Markdown ファイル（agent.md）](./agent.md#スキルの利用) と
[ワークフロー（workflow.yml）](./workflow.md#ツール連携mcp--skills--subagents--tools) にもあります。

## システムプロンプトへの追記のされ方

スキルの内容は、ステップの `system`（レンダリング後）やエージェントのシステムプロンプトの
後ろに `---` 区切りで追記されます。ステップ/エージェント自身の指示が常に先頭にくるようにするための
順序です。`skills:` に複数のスキルを指定した場合は、指定順に連結されます。

```
<ステップ/エージェント自身のシステムプロンプト>

---

## Skill: <スキル名 1>

<description があれば>

<スキル 1 の本文>

## Skill: <スキル名 2>

...
```

`system` を持たない `prompt` ステップやチャットのように、もともとシステムプロンプトを持たない呼び出しでは、
スキルの内容だけがシステムプロンプトになります。

## progressive disclosure（`skill_progressive_disclosure`）

既定では、スキルの本文は常にシステムプロンプトへ全文追記されます。スキルを多く使う・本文が
長いなどの理由でコンテキストを節約したい場合は、`lait.config.yml` の `default:` に
`skill_progressive_disclosure: true` を指定してください。有効にすると:

- システムプロンプトに追記されるのは各スキルの `name`/`description`（frontmatter）だけになり、
  本文は追記されません。
- 代わりに、`skill__<スキル名>`（例: `skill__code-review`）という引数なしのツールがモデルに
  渡されます。モデルがこのツールを呼び出すと、その1件だけの本文（`## Skill: ...` の完全な形）が
  ツール結果として返ります。
- frontmatter のテキストには、どのツールを呼べばよいかの案内（`skill__<name>` の名前）が
  含まれます。

```yaml
# lait.config.yml
default:
  skill_progressive_disclosure: true
  skills: [code-review]
skills:
  code-review: skills/code-review.md
```

**config ファイル全体で1つの真偽値**です（`default.compaction` と同じ位置づけ）。CLI フラグや
agent ファイル/ワークフローステップ単位の上書きはありません。

### 有効化の影響

- **ツール往復が発生します。** 本文が必要なスキルごとに、モデルが `skill__<name>` を呼び出す
  ラウンドが最低1回増えます。下の「`mcp`/`--stream`/`output_schema` との違い」の内容は、
  この設定を有効にした場合には当てはまらなくなります（`max_tool_rounds` を消費し、
  `output_schema` との併用で追加のラウンドトリップが発生します）。
- **ツール呼び出しの弱いモデルでは、本文が読まれないまま無視される可能性があります。** ローカル
  LLM など、ツール呼び出しの精度が低いモデルを使う場合は、既定（常時全文追記）のままにする
  ことを推奨します。
- **`tool_policy`/`--approve-tools` の対象になります。** `skill__*` も
  [`tool_policy`（allow/deny）と `--approve-tools`](./mcp.md#tool_policyallowdenyと---approve-tools対話的承認)
  で他のツールと同じように扱われます。`deny: ["*"]` のような包括的な deny を設定していると、
  スキルの本文が一切読めなくなる点に注意してください。
- **`--record`/`--replay` のカセットに互換性がありません。** この設定を有効にする前に記録した
  カセットは、`skill__<name>` の呼び出しラウンドを含まないため、有効化後の `--replay` では
  再生に失敗します。設定を切り替えたら、影響するワークフロー/エージェントのカセットを録り直して
  ください。

## `mcp`/`--stream`/`output_schema` との違い（既定: `skill_progressive_disclosure` 未設定）

スキルは、MCP ツールのようにモデルへ「呼び出し可能な機能」として渡されるのではなく、リクエスト
前にシステムプロンプトへ静的に追記されるだけです（[MCP サーバーのツールを使う](./mcp.md)
とは別の仕組みです）。そのため:

- `--stream` との併用に制限はありません。
- `output_schema`（Structured Outputs）との併用にも追加のラウンドトリップは発生しません。
- `max_tool_rounds` の消費対象にはなりません。

これらはすべて既定（`skill_progressive_disclosure` を有効にしない場合）の話です。有効にした
場合の挙動は上の「progressive disclosure」の節を参照してください。
