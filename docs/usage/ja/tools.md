# カスタムシェルツールを使う

[ドキュメント目次に戻る](./README.md)

`lait.config.yml` の `tools:` に、ローカルのコマンドをそのままモデルから呼び出せるツールとして
定義できます。MCP サーバーを書いたり立てたりしなくても、`rg`/`jq`/`gh` のような 1 コマンドだけを
モデルに使わせたいときに使います。[MCP サーバーのツール](./mcp.md)・[サブエージェント](./subagents.md)
と同列の tool loop で扱われ、tool_policy/`--approve-tools` の対象にもなります。

```yaml
# lait.config.yml
default:
  model: local
  tools: [ripgrep]        # 全経路（チャット / agent / workflow）の最終フォールバック

tools:
  ripgrep:
    description: "リポジトリ内をパターン検索する"
    command: ["rg", "--json", "{{ input.pattern }}"]
    parameters:
      type: object
      properties:
        pattern: { type: string, description: "検索パターン" }
      required: [pattern]
    timeout: 10
```

```sh
cargo run -- "TODO を検索して" --tool ripgrep
```

## `tools:` の登録（`lait.config.yml`）

各エントリは次のフィールドを持ちます。

- `command`（必須）: 実行するコマンドの argv。`command[0]` がプログラム、`command[1..]` が引数です。
  **シェルは介さず直接 exec されます**（`;`/`|`/バッククォートなどでコマンドを注入することはできません）。
  空リストは設定エラーです。
- `description`（任意）: モデルに見せるツールの説明。省略もできますが、モデルが正しく使うためには
  書いておくことを推奨します。
- `parameters`（任意）: モデルの呼び出し引数を表す JSON Schema。OpenAI のツール定義にそのまま渡され
  ます。省略時は引数なしのツール（`{"type":"object","properties":{}}`）として扱われます。JSON
  オブジェクトである必要があります。
- `timeout`（任意、秒）: コマンドの実行時間の上限。省略時は 30 秒です。超過すると呼び出しはタイム
  アウトエラーとして扱われます。

## 引数のテンプレート展開

`command` の各要素は、モデルが渡した JSON 引数を `input` として、[ワークフロー](./workflow.md)の
`prompt:`/`command:` と同じ handlebars テンプレートで展開されます。モデルの呼び出し引数が
`{"pattern": "TODO"}` なら、`{{ input.pattern }}` は `TODO` に展開されます。`steps`/`vars` は
空のまま渡されるため参照できません。

## 各経路での指定方法

`tools:`（使うツール名のリスト）は、`mcp:`/`subagents:` と全く同じ経路でフォールバックします。

| 経路 | 優先順位 |
|---|---|
| チャット（`lait "prompt"`） | `--tool`（CLI フラグ、複数指定可）→ `lait.config.yml` の `default.tools` |
| `lait agent run` | agent ファイルの frontmatter `tools:` → `lait.config.yml` の `default.tools` |
| `lait run`（workflow） | ノードの `tools:` → （`agent:` ノードなら）agent ファイルの `tools:` → ワークフローの `default.tools` → `lait.config.yml` の `default.tools` |

```sh
lait "prompt" --tool ripgrep --tool jq
```

```markdown
---
model: local
tools: [ripgrep]
---
{{ input.task }} を実行してください。
```

```yaml
# workflow.yml
nodes:
  research:
    type: prompt
    prompt: "{{ input }} について調べてください。"
    tools: [ripgrep]
```

## ツール名の扱い

モデルに渡すツール名は `tool__<名前>`（例: `tool__ripgrep`）の形に修飾されます。MCP の
`<サーバー名>__<ツール名>`、サブエージェントの `agent__<名前>` と同じ規則です。MCP・サブエージェント・
シェルツールの間で修飾後の名前が衝突した場合はエラーになります。

## `tool_policy`/`--approve-tools` との関係

シェルツールも [`tool_policy`（allow/deny）と `--approve-tools`（対話的承認）](./mcp.md#tool_policyallowdeny-と---approve-tools対話的承認)
の対象です。修飾済みツール名（`tool__ripgrep` など）でパターンを書いてください。

```yaml
tool_policy:
  deny: ["tool__*"]   # シェルツールを一律禁止する例
```

`--approve-tools` の確認プロンプトでは、シェルツールに限り「モデルが渡した JSON 引数」に加えて
「`command:` テンプレートを実際に展開した後の argv」も表示されます。`command:` は引数をそのまま
渡すとは限らず、より大きなシェルコマンドの一部に埋め込むといった変換をしうるため、JSON 引数だけ
見て承認すると実際に実行される内容と食い違うおそれがあるからです。

## エラー処理

コマンドの終了コードが非 0、またはタイムアウトした場合、**ツール呼び出し全体は失敗しません**。
標準エラー出力を含むエラー文字列がツールの実行結果としてモデルに返され、tool loop は継続します
（MCP ツールの失敗と同じ扱いです）。一方、モデルが渡した引数が JSON オブジェクトとして解釈できない
場合や、未知のツール名を呼び出した場合はリクエスト全体がエラーになります。

## `lint`

`lait lint` は次を静的にチェックします。

- `tools:` を参照する `default.tools`/ノードの`tools:`/agent frontmatter の `tools:` が
  `lait.config.yml` の `tools:` に実在するか
- 各 `tools:` エントリの `command` が空でないか、`parameters` が JSON オブジェクトかどうか
  （参照されているかどうかに関わらず、定義されている全エントリをチェックします）
