# スキルを使う

[ドキュメント目次に戻る](./README.md)

`lait` は、Markdown ファイル1つで定義する「スキル」を、`lait agent run`・`lait run`
（workflow）のどちらの経路からもモデルのシステムプロンプトに追記できます。スキルはレビュー観点
やコーディング規約のような、複数の agent/ノードで使い回したい指示のかたまりを1か所にまとめて
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

- `name`/`description` はどちらも省略可能です。`name` を省略した場合は、`lait.config.yml` の
  `skills:` でそのファイルに付けたエントリ名が使われます。
- 本文は handlebars テンプレートとしてレンダリングされません。エージェントファイルの
  システムプロンプトとは異なり、`{{ }}` を含むコード例などをそのまま書けます。

## スキルの登録（`lait.config.yml`）

`skills:` に、スキルファイルへのパス、またはそのファイルを含むディレクトリへのパスを指定します。

```yaml
skills:
  code-review: skills/code-review.md
  style-guide: skills/style-guide/       # ディレクトリを指定すると SKILL.md を読む
```

ディレクトリを指定した場合は、その直下の `SKILL.md` が使われます。これは Anthropic の Agent
Skills の慣習（`<name>/SKILL.md`）に合わせたもので、既存の `.claude/skills/<name>/` のような
ディレクトリをそのまま指せます。

パスは、`agent:`/`file_path:` と同じく、常にコマンドを実行したディレクトリ（カレントディレクトリ）
からの相対パスとして解決されます。未登録の名前を `skills:`（agent ファイル／ノード／`default:`）に
書くと、`lait.config.yml` の `skills:` を案内するエラーになります。

## 各経路での指定方法

`skills:`（使うスキル名のリスト）は、経路ごとに次のようにフォールバックします。

| 経路 | 優先順位 |
|---|---|
| `lait agent run` | agent ファイルの frontmatter `skills:` → `lait.config.yml` の `default.skills` |
| `lait run`（workflow） | ノードの `skills:` → （`agent:` ノードなら）agent ファイルの `skills:` → ワークフローの `default.skills` → `lait.config.yml` の `default.skills` |
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
nodes:
  review:
    type: prompt
    prompt: "{{ input }} をレビューしてください。"
    skills: [code-review, style-guide]
```

それぞれの詳細は [エージェント Markdown ファイル（agent.md）](./agent.md#スキルの利用) と
[ワークフロー（workflow.yml）](./workflow.md#スキルの利用skills) にもあります。

## システムプロンプトへの追記のされ方

スキルの内容は、ノードの `prompt`（レンダリング後）や agent ファイルのシステムプロンプトの
後ろに `---` 区切りで追記されます。ノード/agent 自身の指示が常に先頭にくるようにするための
順序です。`skills:` に複数のスキルを指定した場合は、指定順に連結されます。

```
<ノード/agent 自身のシステムプロンプト>

---

## Skill: <スキル名 1>

<description があれば>

<スキル 1 の本文>

## Skill: <スキル名 2>

...
```

`prompt:` を使う通常のノードやチャットのように、もともとシステムプロンプトを持たない呼び出しでは、
スキルの内容だけがシステムプロンプトになります。

## `mcp`/`--stream`/`structured_output` との違い

スキルは、MCP ツールのようにモデルへ「呼び出し可能な機能」として渡されるのではなく、リクエスト
前にシステムプロンプトへ静的に追記されるだけです（[MCP サーバーのツールを使う](./mcp.md)
とは別の仕組みです）。そのため:

- `--stream` との併用に制限はありません。
- `structured_output`（`output_schema`）との併用にも追加のラウンドトリップは発生しません。
- `max_tool_rounds` の消費対象にはなりません。
