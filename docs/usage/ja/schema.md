# JSON Schema でエディタ補完（lait schema）

[ドキュメント目次に戻る](./README.md)

`lait schema workflow|config|agent` サブコマンドで、`workflow.yml`・`lait.config.yml`・
エージェント Markdown ファイルのフロントマターそれぞれの JSON Schema（draft 2020-12）を
標準出力に出力できます。エディタの YAML 補完・検証（[yaml-language-server](https://github.com/redhat-developer/yaml-language-server)
など）に使うことを想定しています。

```sh
lait schema workflow > workflow.schema.json
lait schema config   > config.schema.json
lait schema agent    > agent.schema.json
```

スキーマ本体はリポジトリの [`schemas/`](https://github.com/SuzumiyaAoba/lait/tree/master/schemas)
にもコミットされており、`lait schema <KIND>` は実質的にその内容を整形して出力するだけです
（コマンドを使わず直接ダウンロードして参照することもできます）。

## エディタへの設定

### yaml-language-server（VS Code の YAML 拡張など）

ファイル先頭にモードラインコメントを1行追加すると、そのファイルだけにスキーマが適用されます。

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/SuzumiyaAoba/lait/master/schemas/workflow.json
name: sample-workflow
steps:
  - use: summarize
```

`lait.config.yml`・agent frontmatter でも同様に、それぞれ `schemas/config.json`・
`schemas/agent.json` を指定します（agent ファイルの場合、スキーマが効くのは `---` で
囲まれたフロントマター部分のみで、その後の本文（システムプロンプトのテンプレート）は
対象外です）。

複数ファイルにまとめて適用したい場合は、エディタ側の設定（VS Code の `yaml.schemas`
設定など）でファイル名パターンに対してスキーマ URL を紐付ける方法もあります。

```jsonc
// .vscode/settings.json
{
  "yaml.schemas": {
    "https://raw.githubusercontent.com/SuzumiyaAoba/lait/master/schemas/workflow.json": "workflow.yml",
    "https://raw.githubusercontent.com/SuzumiyaAoba/lait/master/schemas/config.json": "lait.config.yml"
  }
}
```

## 位置づけと限界

- スキーマは手書きで、`lait` 本体の実際のパーサ（`serde` の構造体）と別々に保守されています。
  `lait` を更新するたびに CI 上のテスト（実際のパーサとスキーマの両方に同じ YAML を通し、
  両者の合否が一致することを確認する）で乖離がないか検証していますが、`lait lint` ほど
  厳密で網羅的なチェックではありません。実行前の最終確認には引き続き
  [`lait lint`](./lint.md) を使ってください。
- ドキュメントに載っている主要な語彙はカバーしていますが、将来追加される全フィールドを
  自動的に反映するものではありません（`schemars` のような derive ベースの自動生成は、
  `#[serde(deny_unknown_fields)]`/タグ付き enum を多用するこのプロジェクトの構造とは
  相性が悪いため採用していません)。

関連: [設定ファイル](./config.md)、[ワークフロー（workflow.yml）](./workflow.md)、
[エージェント Markdown ファイル（agent.md）](./agent.md)、[lint](./lint.md)
