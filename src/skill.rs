use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use tokio::sync::OnceCell;

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

/// Runs `lait skill list`: prints every configured `skills:` entry's name,
/// path, and (when the file loads cleanly) its own `description:`. Reads
/// the file directly with a plain, synchronous `fs::read_to_string` rather
/// than through `load_skill`/`SkillCache` — this only ever runs once per
/// entry, so none of `load_skill`'s cancellation-aware/FIFO-safe machinery
/// (built for a request that may need to time out) is worth pulling in.
/// `config_dir` resolves each entry's path relative to the directory
/// containing the `lait.config.yml` that defined it, not the current working
/// directory — see `config::resolve_registry_path`. A registry entry whose file
/// is missing or fails to parse is still listed (with a note) rather than
/// aborting the whole command — `lait lint` is where a hard failure on a bad
/// entry belongs.
pub(crate) fn list(file_config: &config::ConfigFile, config_dir: Option<&Path>) -> Result<()> {
    if file_config.skills.is_empty() {
        println!(
            "no skills defined in {}; add a 'skills:' entry to define one",
            config::CONFIG_FILE_NAME
        );
        return Ok(());
    }
    let mut names: Vec<&String> = file_config.skills.keys().collect();
    names.sort_unstable();
    for name in names {
        let raw_path = &file_config.skills[name];
        let configured_path = config::resolve_registry_path(raw_path, config_dir);
        let path = resolve_skill_file_path(&configured_path);
        let loaded = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read skill file '{}'", path.display()))
            .and_then(|contents| parse_skill(name, &contents))
            .map(|skill| skill.description);
        config::print_registry_entry(name, &path, loaded);
    }
    Ok(())
}

async fn load_skill(
    name: &str,
    configured_path: &Path,
    cancellation: Option<tokio_util::sync::CancellationToken>,
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
/// `skills:` set would otherwise do on every iteration). Each name gets its
/// own `OnceCell`: the outer `Mutex` is only ever held long enough to fetch
/// or insert that cell (never across an await), and the cell's own
/// `get_or_try_init` — not a check-then-insert on the map — is what makes
/// concurrent `parallel:`/`for_each:` branches requesting the same name for
/// the first time share one load instead of two racing ones. The cached
/// value is an `Arc<String>` rather than a bare `String` so a cache hit is a
/// refcount bump, not a clone of the skill's Markdown body; `Arc` (not `Rc`)
/// because a `SkillCache` is shared across `tokio::spawn`ed tasks (`parallel`/
/// concurrent `for_each` — see `engine::AppContext`), which requires `Send`.
pub(crate) struct SkillCache {
    skills_map: Arc<config::SkillMap>,
    sections: Mutex<HashMap<String, Arc<OnceCell<Arc<String>>>>>,
}

impl SkillCache {
    pub(crate) fn new(skills_map: Arc<config::SkillMap>) -> Self {
        Self {
            skills_map,
            sections: Mutex::new(HashMap::new()),
        }
    }

    /// Renders `names` (a resolved `skills:` list, already merged through
    /// every fallback layer) into the block of text appended to a completion
    /// request's system prompt — see `engine::with_skills`. Returns `None` when
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
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Option<String>> {
        if names.is_empty() {
            return Ok(None);
        }
        if cancellation
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            anyhow::bail!("skill rendering was cancelled");
        }
        let sections = futures_util::future::try_join_all(
            names
                .iter()
                .map(|name| self.section(name, cancellation.clone())),
        )
        .await?;
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
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Arc<String>> {
        if cancellation
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            anyhow::bail!("skill rendering was cancelled");
        }
        let cell = Arc::clone(
            self.sections
                .lock()
                .expect("skill cache lock should not be poisoned")
                .entry(name.to_owned())
                .or_insert_with(|| Arc::new(OnceCell::new())),
        );
        // Only the caller that actually wins the race to initialize `cell`
        // runs this closure — every other concurrent caller for the same
        // name just awaits its result, never reaching its own cancellation
        // checks below. Race the whole `get_or_try_init` against this call's
        // own `cancellation` too, so a losing (non-initializing) caller can
        // still bail out promptly on its own token instead of being stuck
        // waiting on a load it isn't driving.
        let init_cancellation = cancellation.clone();
        let init = cell.get_or_try_init(|| async {
            let configured_path = self.skills_map.get(name).ok_or_else(|| {
                anyhow!(
                    "unknown skill '{name}'; define it under 'skills:' in {}",
                    config::CONFIG_FILE_NAME
                )
            })?;
            let read_cancellation = init_cancellation.clone();
            let skill = load_skill(name, configured_path, init_cancellation).await?;
            if read_cancellation
                .as_ref()
                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
            {
                anyhow::bail!("skill rendering was cancelled");
            }
            Ok::<_, anyhow::Error>(Arc::new(format_skill(&skill)))
        });
        let section = match async_io::await_cancellation(init, cancellation).await {
            async_io::CancellationResult::Completed(result) => result?,
            async_io::CancellationResult::Cancelled => {
                anyhow::bail!("skill rendering was cancelled");
            }
        };
        Ok(Arc::clone(section))
    }
}

#[cfg(test)]
mod tests {
    use super::{SkillCache, parse_skill};
    use std::{collections::HashMap, fs, sync::Arc, time::Duration};

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
        let cache = SkillCache::new(Arc::new(skills_map));
        assert!(cache.render(&[], None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn render_errors_on_an_unknown_skill_name() {
        let skills_map = HashMap::new();
        let cache = SkillCache::new(Arc::new(skills_map));
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
        let cache = SkillCache::new(Arc::new(skills_map));

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
        let cache = SkillCache::new(Arc::new(skills_map));
        let names = ["blocked".to_owned()];
        let token = tokio_util::sync::CancellationToken::new();
        let mut render = Box::pin(cache.render(&names, Some(token.clone())));

        tokio::select! {
            result = &mut render => panic!("FIFO skill unexpectedly returned: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                token.cancel();
            }
        }
        let result = tokio::time::timeout(Duration::from_secs(1), render)
            .await
            .expect("FIFO skill cancellation should finish promptly")
            .unwrap_err();
        assert!(result.to_string().contains("cancel"), "error: {result}");
        let _ = fs::remove_file(path);
    }

    /// Regression test for the `OnceCell`-per-name cache (see `SkillCache`'s
    /// doc comment): only the first caller for a given name actually runs
    /// the load and its own cancellation checks — every other concurrent
    /// caller just awaits that result. Confirms a losing (non-initializing)
    /// caller still returns promptly on *its own* cancellation rather than
    /// being stuck until the winning caller's load finishes (which, here,
    /// never happens — the FIFO is never written to).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_second_waiter_can_cancel_while_the_first_is_still_loading() {
        let path = crate::test_support::unique_temp_path("lait-test-skill-fifo-2", "");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let mut skills_map = HashMap::new();
        skills_map.insert("blocked".to_owned(), path.clone());
        let cache = SkillCache::new(Arc::new(skills_map));
        let names = ["blocked".to_owned()];

        // A token of its own that's never cancelled: this caller becomes the
        // cell's initializer and blocks on the FIFO for the rest of the test
        // (passing `None` here would skip `load_skill`'s wait-for-a-writer
        // path entirely and return immediately instead).
        let first_token = tokio_util::sync::CancellationToken::new();
        let mut first = Box::pin(cache.render(&names, Some(first_token)));
        tokio::select! {
            result = &mut first => panic!("first render unexpectedly returned: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }

        let token = tokio_util::sync::CancellationToken::new();
        let mut second = Box::pin(cache.render(&names, Some(token.clone())));
        tokio::select! {
            result = &mut second => panic!("second render unexpectedly returned: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                token.cancel();
            }
        }
        let result = tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .expect("a losing waiter's own cancellation should finish promptly")
            .unwrap_err();
        assert!(result.to_string().contains("cancel"), "error: {result}");

        drop(first);
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
