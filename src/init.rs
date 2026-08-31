//! The `lait init` subcommand: writes commented starter files
//! (`lait.config.yml`, a workflow, an agent) so a new user can begin from a
//! valid scaffold instead of a blank page. An existing file is never
//! overwritten.

use std::{io::Write, path::PathBuf};

use anyhow::{Context, Result, bail};

use crate::{
    cli::{InitArgs, InitKind},
    config,
};

/// The `lait init` scaffold for `lait.config.yml`. Kept a valid
/// `config::ConfigFile` by the tests in this module.
const CONFIG_TEMPLATE: &str = r#"# lait の設定ファイル。カレントディレクトリに置くと自動で読み込まれます。
# 詳細: https://github.com/SuzumiyaAoba/lait/blob/master/docs/usage/ja/config.md

# すべてのリクエストに適用される既定値。
default:
  model: local
  # reasoning_effort: medium
  # system: あなたは有能なアシスタントです。

# モデル alias の定義。`--model local` や `default.model: local` で参照できます。
models:
  local:
    - provider:
        base_url: http://localhost:1234/v1
        # api_key は ${VAR_NAME} で環境変数を参照できます（.env も自動で読まれます）。
        # api_key: ${OPENAI_API_KEY}
      model_id: your-model-id
      # default_reasoning_effort: medium
      # default_temperature: 0.7

# MCP サーバー・スキル・サブエージェントの登録例:
# mcp_servers:
#   filesystem:
#     command: npx
#     args: ["-y", "@modelcontextprotocol/server-filesystem", "."]
# skills:
#   style-guide: ./skills/style-guide
# agents:
#   researcher: ./agents/researcher.md
"#;

/// The `lait init workflow` scaffold. Kept loadable by
/// `workflow::load_workflow` by the tests in this module.
const WORKFLOW_TEMPLATE: &str = r#"# lait のワークフロー定義。`lait run workflow.yml "入力"` で実行します。
# nodes:(何をするか)と steps:(どう繋ぐか)を分けて書きます。
# 詳細: https://github.com/SuzumiyaAoba/lait/blob/master/docs/usage/ja/workflow.md
name: sample-workflow
description: 入力を要約するサンプルワークフロー

# このワークフロー内だけで使う既定値(lait.config.yml の default: より優先)。
# default:
#   model: local

nodes:
  summarize:
    # model: local  # 省略時は default.model にフォールバック
    prompt: |
      次の文章を3行で要約してください。

      {{ input }}
  # JSON 出力を jq で加工するノードの例:
  # extract:
  #   prompt: "..."
  #   output_schema: ./schema.json
  #   jq: '.summary'

steps:
  - id: summarize
    use: summarize
  # 条件分岐 (when/switch)・並列 (parallel)・ループ (loop/for_each)・
  # サブワークフロー (workflow:) も使えます。詳細は docs を参照してください。
  # - use: extract
  #   when: '. != ""'
"#;

/// The `lait init agent` scaffold. Kept parsable by `agent::load_agent` by
/// the tests in this module.
const AGENT_TEMPLATE: &str = r#"---
# lait のエージェント定義。`lait agent run agent.md "入力"` で実行します。
# frontmatter が設定、本文がシステムプロンプトのテンプレートになります。
# 詳細: https://github.com/SuzumiyaAoba/lait/blob/master/docs/usage/ja/agent.md
name: sample-agent
description: 文章を要約するエージェント
# model: local  # 省略時は lait.config.yml の default.model にフォールバック
# reasoning_effort: medium
# temperature: 0.7
# 入力/出力を JSON Schema で縛る場合:
# input_schema:
#   schema:
#     type: object
#     required: [text]
# structured_output: true
# output_schema:
#   schema:
#     type: object
#     properties:
#       summary: { type: string }
#     required: [summary]
---

あなたは要約の専門家です。与えられた文章の要点を保ったまま、簡潔に要約してください。

<!-- 本文は handlebars テンプレートです。入力が JSON なら {{ input.field }} で参照できます。 -->
"#;

pub(crate) fn run(args: InitArgs) -> Result<()> {
    let (path, contents) = match args.kind {
        None => (PathBuf::from(config::CONFIG_FILE_NAME), CONFIG_TEMPLATE),
        Some(InitKind::Workflow) => (
            args.path.unwrap_or_else(|| PathBuf::from("workflow.yml")),
            WORKFLOW_TEMPLATE,
        ),
        Some(InitKind::Agent) => (
            args.path.unwrap_or_else(|| PathBuf::from("agent.md")),
            AGENT_TEMPLATE,
        ),
    };

    // `exists()` followed by `write()` has a TOCTOU window: another process
    // (or a symlink swap) could make us overwrite a file after the check.  A
    // single `create_new` open is atomic and enforces init's promise never to
    // overwrite an existing path.
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!(
                "'{}' already exists; refusing to overwrite it (move it aside or pass another PATH)",
                path.display()
            )
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to create '{}'", path.display()));
        }
    };
    file.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write '{}'", path.display()))?;
    println!("created '{}'", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AGENT_TEMPLATE, CONFIG_TEMPLATE, WORKFLOW_TEMPLATE};

    #[test]
    fn the_config_template_parses_as_a_config_file() {
        let config: crate::config::ConfigFile = serde_yaml::from_str(CONFIG_TEMPLATE)
            .expect("the lait init config template must stay valid");
        assert_eq!(config.default.model.as_deref(), Some("local"));
        assert!(config.models.contains_key("local"));
    }

    #[test]
    fn the_workflow_template_parses_as_a_workflow() {
        // `workflow::load_workflow` reads from disk, so the parse/validate
        // pair it wraps is exercised through a temp file.
        let path = std::env::temp_dir().join(format!(
            "lait-init-workflow-template-{}.yml",
            std::process::id()
        ));
        std::fs::write(&path, WORKFLOW_TEMPLATE).expect("temp workflow should be writable");
        let result = crate::workflow::load_workflow(&path);
        std::fs::remove_file(&path).ok();
        let workflow = result.expect("the lait init workflow template must stay valid");
        assert_eq!(workflow.name.as_deref(), Some("sample-workflow"));
        assert!(!workflow.steps.is_empty());
    }

    #[test]
    fn the_agent_template_parses_as_an_agent() {
        let path = std::env::temp_dir().join(format!(
            "lait-init-agent-template-{}.md",
            std::process::id()
        ));
        std::fs::write(&path, AGENT_TEMPLATE).expect("temp agent should be writable");
        let result = crate::agent::load_agent(&path);
        std::fs::remove_file(&path).ok();
        let agent = result.expect("the lait init agent template must stay valid");
        assert_eq!(agent.name.as_deref(), Some("sample-agent"));
        assert!(!agent.system_prompt_template.is_empty());
    }
}
