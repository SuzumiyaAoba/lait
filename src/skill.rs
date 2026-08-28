use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::{config, frontmatter};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
}

/// A skill Markdown file: YAML frontmatter (an optional display `name`/
/// `description`) followed by a Markdown body, appended verbatim (never
/// rendered as a handlebars template — see `render`) to a completion
/// request's system prompt.
struct SkillFile {
    name: String,
    description: Option<String>,
    body: String,
}

/// Resolves a `skills:` entry's configured path to the actual file to read:
/// the path itself if it names a file, or `<path>/SKILL.md` if it names a
/// directory — matching the Anthropic Agent Skills convention of a
/// `SKILL.md` per skill directory, so an existing skills directory (e.g.
/// `.claude/skills/<name>/`) can be pointed at directly.
fn resolve_skill_file_path(configured_path: &Path) -> PathBuf {
    if configured_path.is_dir() {
        configured_path.join("SKILL.md")
    } else {
        configured_path.to_path_buf()
    }
}

fn load_skill(name: &str, configured_path: &Path) -> Result<SkillFile> {
    let path = resolve_skill_file_path(configured_path);
    let contents = fs::read_to_string(&path).with_context(|| {
        format!(
            "failed to read skill file '{}' (skill '{name}')",
            path.display()
        )
    })?;
    parse_skill(name, &contents).with_context(|| {
        format!(
            "failed to parse skill file '{}' (skill '{name}')",
            path.display()
        )
    })
}

fn parse_skill(name: &str, contents: &str) -> Result<SkillFile> {
    let (frontmatter, body) = frontmatter::split(contents, "skill file")?;
    let frontmatter: SkillFrontmatter =
        serde_yaml::from_str(frontmatter).context("failed to parse frontmatter")?;
    Ok(SkillFile {
        name: frontmatter.name.unwrap_or_else(|| name.to_owned()),
        description: frontmatter.description,
        body: body.trim().to_owned(),
    })
}

/// Renders `names` (a resolved `skills:` list, already merged through every
/// fallback layer) into the block of text appended to a completion request's
/// system prompt — see `app::with_skills`. Returns `None` when `names` is
/// empty, so a request that never turns on skills pays no cost. Each name is
/// resolved against `skills_map` (`lait.config.yml`'s top-level `skills:`)
/// here, at request time, not at workflow/agent-file parse time: parsing
/// never sees the config file, the same reason `mcp::McpRegistry::connection`
/// resolves `mcp_servers:` names lazily (see its doc comment).
///
/// A skill's body is appended literally, never rendered as a handlebars
/// template: unlike an agent's own `system_prompt_template`, a skill's
/// Markdown body may legitimately contain `{{`/`}}` (e.g. in a code sample),
/// and `template::render` treats an undefined variable as a hard error.
pub(crate) fn render(skills_map: &config::SkillMap, names: &[String]) -> Result<Option<String>> {
    if names.is_empty() {
        return Ok(None);
    }

    let mut rendered = String::new();
    for name in names {
        let configured_path = skills_map.get(name).ok_or_else(|| {
            anyhow!(
                "unknown skill '{name}'; define it under 'skills:' in {}",
                config::CONFIG_FILE_NAME
            )
        })?;
        let skill = load_skill(name, configured_path)?;

        if !rendered.is_empty() {
            rendered.push_str("\n\n");
        }
        rendered.push_str("## Skill: ");
        rendered.push_str(&skill.name);
        rendered.push('\n');
        if let Some(description) = &skill.description {
            rendered.push('\n');
            rendered.push_str(description);
            rendered.push('\n');
        }
        rendered.push('\n');
        rendered.push_str(&skill.body);
    }
    Ok(Some(rendered))
}

#[cfg(test)]
mod tests {
    use super::{parse_skill, render};
    use std::collections::HashMap;

    #[test]
    fn parses_frontmatter_and_body() {
        let skill = parse_skill(
            "fallback-name",
            "---\nname: code-review\ndescription: reviews a diff for bugs\n---\nLook for off-by-one errors.\n",
        )
        .expect("skill should parse");

        assert_eq!(skill.name, "code-review");
        assert_eq!(
            skill.description.as_deref(),
            Some("reviews a diff for bugs")
        );
        assert_eq!(skill.body, "Look for off-by-one errors.");
    }

    #[test]
    fn falls_back_to_the_configured_name_when_frontmatter_has_none() {
        let skill = parse_skill("code-review", "---\n---\nbody\n").expect("skill should parse");
        assert_eq!(skill.name, "code-review");
        assert!(skill.description.is_none());
    }

    #[test]
    fn rejects_a_file_without_a_leading_frontmatter_delimiter() {
        assert!(parse_skill("x", "no frontmatter here\n").is_err());
    }

    #[test]
    fn render_returns_none_for_an_empty_name_list() {
        let skills_map = HashMap::new();
        assert!(render(&skills_map, &[]).unwrap().is_none());
    }

    #[test]
    fn render_errors_on_an_unknown_skill_name() {
        let skills_map = HashMap::new();
        let error = render(&skills_map, &["missing".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("missing"));
    }

    #[test]
    fn parse_skill_preserves_handlebars_looking_text_in_the_body_verbatim() {
        let skill = parse_skill(
            "templated",
            "---\nname: templated\n---\nUse {{ input.field }} literally.\n",
        )
        .expect("skill should parse");
        assert_eq!(skill.body, "Use {{ input.field }} literally.");
    }
}
