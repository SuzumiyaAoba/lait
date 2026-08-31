use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::{async_io, config, frontmatter};

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

async fn load_skill(
    name: &str,
    configured_path: &Path,
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<SkillFile> {
    let configured_path = configured_path.to_owned();
    let name = name.to_owned();
    let read_error_name = name.clone();
    let wait_for_fifo_writer = cancellation.is_some();
    // Resolve a directory entry and read its SKILL.md on the same bounded,
    // cancellation-aware worker. `Path::is_dir` itself performs metadata I/O
    // and can block on a network/FUSE mount, so doing only the final
    // `read_to_string` off-thread would still leave a timed step stuck before
    // admission to async_io.
    let (path, contents) = async_io::run_blocking(
        move |cancelled| {
            let path = resolve_skill_file_path(&configured_path);
            let contents = if wait_for_fifo_writer {
                async_io::read_to_string_wait_for_fifo_writer(
                    &path,
                    cancelled,
                    async_io::MAX_READ_BYTES,
                )
            } else {
                async_io::read_to_string(&path, cancelled, async_io::MAX_READ_BYTES)
            }
            .map_err(|error| {
                anyhow!(
                    "failed to read skill file '{}' (skill '{read_error_name}'): {error}",
                    path.display()
                )
            })?;
            Ok((path, contents))
        },
        cancellation,
    )
    .await?;
    parse_skill(&name, &contents).with_context(|| {
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

fn format_skill(skill: &SkillFile) -> String {
    let mut section = format!("## Skill: {}\n", skill.name);
    if let Some(description) = &skill.description {
        section.push('\n');
        section.push_str(description);
        section.push('\n');
    }
    section.push('\n');
    section.push_str(&skill.body);
    section
}

/// A skill's rendered `## Skill: ...` section, cached by name for the
/// `SkillCache`'s lifetime: a skill file's content doesn't change over the
/// course of one `lait run`/`lait agent run`/chat invocation, so every
/// `render()` call after the first for a given name reuses this instead of
/// re-reading and re-parsing the file (which a `for_each`/`loop` node with
/// `skills:` set would otherwise do on every iteration). The first miss now
/// awaits a cancellable async_io worker. Cache borrows are copied into an
/// `Option` before that await, and insertion happens only afterward, so a
/// plain `RefCell` remains safe when `parallel:`/`for_each:` branches race
/// within the same task. Two simultaneous misses may both read the same file,
/// but neither can observe a borrow across an await; the later result simply
/// replaces the equivalent cached section. The cached value is an `Rc<String>`
/// rather than a bare `String` so a cache hit is a refcount bump, not a clone of
/// the skill's Markdown body.
pub(crate) struct SkillCache<'a> {
    skills_map: &'a config::SkillMap,
    sections: RefCell<HashMap<String, Rc<String>>>,
}

impl<'a> SkillCache<'a> {
    pub(crate) fn new(skills_map: &'a config::SkillMap) -> Self {
        Self {
            skills_map,
            sections: RefCell::new(HashMap::new()),
        }
    }

    /// Renders `names` (a resolved `skills:` list, already merged through
    /// every fallback layer) into the block of text appended to a completion
    /// request's system prompt — see `app::with_skills`. Returns `None` when
    /// `names` is empty, so a request that never turns on skills pays no
    /// cost. Each name is resolved against `skills_map` (`lait.config.yml`'s
    /// top-level `skills:`) here, at request time, not at workflow/agent-file
    /// parse time: parsing never sees the config file, the same reason
    /// `mcp::McpRegistry::connection` resolves `mcp_servers:` names lazily
    /// (see its doc comment).
    ///
    /// A skill's body is appended literally, never rendered as a handlebars
    /// template: unlike an agent's own `system_prompt_template`, a skill's
    /// Markdown body may legitimately contain `{{`/`}}` (e.g. in a code
    /// sample), and `template::render` treats an undefined variable as a
    /// hard error.
    pub(crate) async fn render(
        &self,
        names: &[String],
        cancellation: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Result<Option<String>> {
        if names.is_empty() {
            return Ok(None);
        }
        if cancellation
            .as_ref()
            .is_some_and(|receiver| *receiver.borrow())
        {
            anyhow::bail!("skill rendering was cancelled");
        }
        let mut sections = Vec::with_capacity(names.len());
        for name in names {
            if cancellation
                .as_ref()
                .is_some_and(|receiver| *receiver.borrow())
            {
                anyhow::bail!("skill rendering was cancelled");
            }
            sections.push(self.section(name, cancellation.clone()).await?);
        }
        let joined = sections
            .iter()
            .map(|section| section.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(Some(joined))
    }

    async fn section(
        &self,
        name: &str,
        cancellation: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Result<Rc<String>> {
        if cancellation
            .as_ref()
            .is_some_and(|receiver| *receiver.borrow())
        {
            anyhow::bail!("skill rendering was cancelled");
        }
        let cached = self.sections.borrow().get(name).cloned();
        if let Some(cached) = cached {
            return Ok(cached);
        }
        let configured_path = self.skills_map.get(name).ok_or_else(|| {
            anyhow!(
                "unknown skill '{name}'; define it under 'skills:' in {}",
                config::CONFIG_FILE_NAME
            )
        })?;
        let read_cancellation = cancellation.clone();
        let skill = load_skill(name, configured_path, cancellation).await?;
        if read_cancellation
            .as_ref()
            .is_some_and(|receiver| *receiver.borrow())
        {
            anyhow::bail!("skill rendering was cancelled");
        }
        let section = Rc::new(format_skill(&skill));
        self.sections
            .borrow_mut()
            .insert(name.to_owned(), Rc::clone(&section));
        Ok(section)
    }
}

#[cfg(test)]
mod tests {
    use super::{SkillCache, parse_skill};
    use std::{collections::HashMap, fs, time::Duration};

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

    #[tokio::test]
    async fn render_returns_none_for_an_empty_name_list() {
        let skills_map = HashMap::new();
        let cache = SkillCache::new(&skills_map);
        assert!(cache.render(&[], None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn render_errors_on_an_unknown_skill_name() {
        let skills_map = HashMap::new();
        let cache = SkillCache::new(&skills_map);
        let error = cache
            .render(&["missing".to_owned()], None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("missing"));
    }

    #[tokio::test]
    async fn render_rejects_a_skill_larger_than_the_file_read_limit() {
        let path = crate::test_support::unique_temp_path("lait-test-large-skill", ".md");
        let mut contents = b"---\n---\n".to_vec();
        contents.resize(crate::async_io::MAX_READ_BYTES + 1, b'x');
        fs::write(&path, contents).unwrap();
        let mut skills_map = HashMap::new();
        skills_map.insert("large".to_owned(), path.clone());
        let cache = SkillCache::new(&skills_map);

        let error = cache.render(&["large".to_owned()], None).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("read limit"),
            "error: {error:#}"
        );
        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn render_can_cancel_while_waiting_for_a_fifo_writer() {
        let path = crate::test_support::unique_temp_path("lait-test-skill-fifo", "");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let mut skills_map = HashMap::new();
        skills_map.insert("blocked".to_owned(), path.clone());
        let cache = SkillCache::new(&skills_map);
        let names = ["blocked".to_owned()];
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let mut render = Box::pin(cache.render(&names, Some(receiver)));

        tokio::select! {
            result = &mut render => panic!("FIFO skill unexpectedly returned: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                sender.send(true).unwrap();
            }
        }
        let result = tokio::time::timeout(Duration::from_secs(1), render)
            .await
            .expect("FIFO skill cancellation should finish promptly")
            .unwrap_err();
        assert!(result.to_string().contains("cancel"), "error: {result}");
        let _ = fs::remove_file(path);
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
