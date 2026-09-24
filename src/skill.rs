use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use serde::Deserialize;

use crate::{async_cache::AsyncCache, async_io, config, frontmatter, mcp, registry};

/// The single wording used everywhere this module reports a cancelled skill
/// render — both the immediate pre-checks (`bail!(error::cancelled(..))`)
/// and `AsyncCache::get_or_try_init`'s own `cancellation_message` argument,
/// which previously had to be kept in sync with those by hand.
const SKILL_RENDERING_CANCELLED: &str = "skill rendering was cancelled";

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
/// the file directly with `async_io::read_to_string_sync` rather than
/// through `load_skill`/`SkillCache` — this only ever runs once per entry,
/// so none of `load_skill`'s cancellation-aware/FIFO-safe machinery (built
/// for a request that may need to time out) is worth pulling in, but the
/// plain read still goes through the crate's one synchronous entry point so
/// `MAX_READ_BYTES` applies here too. Registry paths are already absolute
/// (resolved once at config-load time, against the directory containing
/// whichever `lait.config.yml`/global `config.yml` defined the entry — not
/// the current working directory — see `config::load_config`). A registry
/// entry whose file is missing or fails to parse is still listed (with a
/// note) rather than aborting the whole command — `lait lint` is where a
/// hard failure on a bad entry belongs.
pub(crate) fn list(file_config: &config::ConfigFile) -> Result<()> {
    registry::list_path_registry("skills", &file_config.skills, |name, configured_path| {
        let path = resolve_skill_file_path(configured_path);
        let loaded = crate::async_io::read_to_string_sync(&path)
            .with_context(|| format!("failed to read skill file '{}'", path.display()))
            .and_then(|contents| parse_skill(name, &contents))
            .map(|skill| skill.description);
        (path, loaded)
    })
}

async fn load_skill(
    name: &str,
    configured_path: &Path,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<SkillFile> {
    let configured_path = configured_path.to_owned();
    let name = name.to_owned();
    let read_error_name = name.clone();
    // Resolve a directory entry and read its SKILL.md on the same bounded,
    // cancellation-aware worker. `Path::is_dir` itself performs metadata I/O
    // and can block on a network/FUSE mount, so doing only the final
    // `read_to_string` off-thread would still leave a timed step stuck before
    // admission to async_io. Waiting for a FIFO writer here is safe as long
    // as `cancellation` (this function's parameter, threaded through from
    // `SkillCache::render`) is an actually-cancellable token — which every
    // production caller passes (the run's own `env.operation_token()`, see
    // `engine::transport::system_prompt_with_skills`) — since
    // `run_blocking`'s guard then trips `cancelled` on drop. A caller that
    // instead passed `cancellation::none()` (see that function's doc) and
    // awaited this call directly would have no such exit: a SKILL.md path
    // that names a FIFO with no writer would block forever.
    let (path, contents) = async_io::run_blocking(
        move |cancelled| {
            let path = resolve_skill_file_path(&configured_path);
            let contents = async_io::read_to_string_wait_for_fifo_writer(
                &path,
                cancelled,
                async_io::MAX_READ_BYTES,
            )
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
    let (frontmatter, body) = frontmatter::parse::<SkillFrontmatter>(contents, "skill file")?;
    Ok(SkillFile {
        name: frontmatter.name.unwrap_or_else(|| name.to_owned()),
        description: frontmatter.description,
        body,
    })
}

/// `deps add`/`install`/`update`'s validation boundary: checks `contents`
/// parse as a skill file under `name` without keeping the parsed body —
/// `SkillFile` itself stays private since only the skill loader consumes it.
pub(crate) fn validate_skill(name: &str, contents: &str) -> Result<()> {
    parse_skill(name, contents).map(|_| ())
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

/// The progressive-disclosure counterpart to `format_skill`: a skill's
/// `name`/`description` only, plus a pointer to the `skill__<name>` tool
/// (see `tools`) that reads the body those two lines don't include. Used
/// instead of `format_skill` only when `default.skill_progressive_disclosure`
/// is `true` — see `SkillCache::render_frontmatter`.
fn format_skill_frontmatter(skill: &SkillFile, qualified_tool_name: &str) -> String {
    let mut section = format!("## Skill: {}\n", skill.name);
    if let Some(description) = &skill.description {
        section.push('\n');
        section.push_str(description);
        section.push('\n');
    }
    section.push('\n');
    section.push_str(&format!(
        "(Call the `{qualified_tool_name}` tool to read this skill's full instructions before following it.)"
    ));
    section
}

/// A skill file's parsed frontmatter and body, cached by name for the
/// `SkillCache`'s lifetime: a skill file's content doesn't change over the
/// course of one `lait run`/`lait agent run`/chat invocation, so every call
/// after the first for a given name reuses this instead of re-reading and
/// re-parsing the file (which a `for_each`/`while`/`until` step with `skills:` set
/// would otherwise do on every iteration, and which
/// `default.skill_progressive_disclosure: true` would otherwise do twice per
/// name — once for the system prompt's frontmatter, once for a
/// `skill__<name>` tool call reading the body). `AsyncCache` gives each name
/// its own `OnceCell`, so concurrent branches requesting the same name share
/// one load while different names can load independently.
pub(crate) struct SkillCache {
    skills_map: config::SkillMap,
    parsed: AsyncCache<String, SkillFile>,
    /// `render`'s own combined-text result, cached by its exact `names` list
    /// (order matters — it's the order sections are joined in). Parsed
    /// skills were already cached per-name, but the `"\n\n"`-joined
    /// combination of their formatted sections was rebuilt from scratch on
    /// every single `render` call for the same `skills:` list — every
    /// `for_each`/`loop` iteration re-walks and re-joins the same
    /// `Vec<Arc<SkillFile>>`. Caching the join result too turns a repeat
    /// `render` call for the same list into a refcount bump.
    joined: AsyncCache<Vec<String>, String>,
    /// `render_frontmatter`'s own combined-text cache — kept separate from
    /// `joined` (rather than sharing one cache keyed only by `names`) so a
    /// run that somehow called both `render` and `render_frontmatter` for the
    /// same `names` (never happens in practice: the effective mode is one
    /// config-wide boolean) could never see one call's cached text returned
    /// for the other's key.
    joined_frontmatter: AsyncCache<Vec<String>, String>,
}

impl SkillCache {
    pub(crate) fn new(skills_map: config::SkillMap) -> Self {
        Self {
            skills_map,
            parsed: AsyncCache::new(),
            joined: AsyncCache::new(),
            joined_frontmatter: AsyncCache::new(),
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
    ///
    /// Returns `Arc<String>` rather than `String` so a cached hit (see
    /// `joined`) is a refcount bump instead of a full copy of the combined
    /// text — `engine::with_skills`'s caller only ever needs to borrow it for
    /// the span of building one request's messages.
    ///
    /// Used unless `default.skill_progressive_disclosure` is `true` — see
    /// `render_frontmatter` for that mode's counterpart.
    pub(crate) async fn render(
        &self,
        names: &[String],
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<Option<Arc<String>>> {
        if names.is_empty() {
            return Ok(None);
        }
        crate::cancellation::check(&cancellation, SKILL_RENDERING_CANCELLED)?;
        let joined = self
            .joined
            .get_or_try_init(
                names.to_vec(),
                cancellation.clone(),
                || async {
                    let skills = futures_util::future::try_join_all(
                        names
                            .iter()
                            .map(|name| self.parsed(name, cancellation.clone())),
                    )
                    .await?;
                    let joined = skills
                        .iter()
                        .map(|skill| format_skill(skill))
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    Ok(Arc::new(joined))
                },
                SKILL_RENDERING_CANCELLED,
            )
            .await?;
        Ok(Some(joined))
    }

    /// The `default.skill_progressive_disclosure: true` counterpart to
    /// `render`: joins each name's `name`/`description` only (see
    /// `format_skill_frontmatter`), never its body. A model that needs a
    /// skill's full instructions calls the matching `skill__<name>` tool
    /// (see `tools`/`skill_body`), which this text points it at by qualified
    /// name.
    pub(crate) async fn render_frontmatter(
        &self,
        names: &[String],
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<Option<Arc<String>>> {
        if names.is_empty() {
            return Ok(None);
        }
        crate::cancellation::check(&cancellation, SKILL_RENDERING_CANCELLED)?;
        let joined = self
            .joined_frontmatter
            .get_or_try_init(
                names.to_vec(),
                cancellation.clone(),
                || async {
                    let skills = futures_util::future::try_join_all(
                        names
                            .iter()
                            .map(|name| self.parsed(name, cancellation.clone())),
                    )
                    .await?;
                    let mut sections = Vec::with_capacity(names.len());
                    for (name, skill) in names.iter().zip(skills.iter()) {
                        let qualified = mcp::qualify_tool_name("skill", "skill", name)?;
                        sections.push(format_skill_frontmatter(skill, &qualified));
                    }
                    Ok(Arc::new(sections.join("\n\n")))
                },
                SKILL_RENDERING_CANCELLED,
            )
            .await?;
        Ok(Some(joined))
    }

    /// The full `## Skill: ...` section for exactly one skill (never
    /// joined with any other), for a `skill__<name>` tool call's result —
    /// see `engine::tool_loop::ToolLoop::dispatch_tool_call`. Reuses the same
    /// per-name `parsed` cache `render`/`render_frontmatter` populate, so a
    /// skill named in both this run's `skills:` list and a tool call it
    /// triggers is only ever read from disk once.
    pub(crate) async fn skill_body(
        &self,
        name: &str,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<Arc<String>> {
        let skill = self.parsed(name, cancellation).await?;
        Ok(Arc::new(format_skill(&skill)))
    }

    async fn parsed(
        &self,
        name: &str,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<Arc<SkillFile>> {
        let init_cancellation = cancellation.clone();
        let skill = self
            .parsed
            .get_or_try_init(
                name.to_owned(),
                cancellation,
                || async {
                    let configured_path = self.skills_map.get(name).ok_or_else(|| {
                        anyhow!(
                            "unknown skill '{name}'; define it under 'skills:' in {}",
                            config::CONFIG_FILE_NAME
                        )
                    })?;
                    let read_cancellation = init_cancellation.clone();
                    let skill = load_skill(name, configured_path, init_cancellation).await?;
                    crate::cancellation::check(&read_cancellation, SKILL_RENDERING_CANCELLED)?;
                    Ok(Arc::new(skill))
                },
                SKILL_RENDERING_CANCELLED,
            )
            .await?;
        Ok(skill)
    }
}

/// The skill-tool half of `mcp::ToolSet`/`subagent::ToolSet`/
/// `shell_tool::ToolSet`: an OpenAI tool list plus an index from each tool's
/// qualified name (`skill__<name>`, see `mcp::qualify_tool_name`) back to the
/// plain `skills:` name `SkillCache::skill_body` needs. Only populated when
/// `default.skill_progressive_disclosure` is `true` — see
/// `engine::transport`'s `assemble_tool_sets` — so a run with progressive
/// disclosure off never offers a `skill__*` tool at all, matching that mode's
/// promise of no extra tool round trip.
#[derive(Debug)]
pub(crate) struct ToolSet {
    pub(crate) tools: Vec<ChatCompletionTools>,
    index: HashMap<String, String>,
}

impl ToolSet {
    /// The `skills:` name `qualified_name` (as returned in this set's
    /// `tools`) refers to, if any.
    pub(crate) fn tool_name(&self, qualified_name: &str) -> Option<&str> {
        self.index.get(qualified_name).map(String::as_str)
    }

    /// Every qualified tool name this set defines, used only to check for a
    /// collision against the other tool sources — see
    /// `engine::RequestSettings::complete`/`complete_stream`.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.index.keys().map(String::as_str)
    }
}

/// Resolves `names` (a request's `skills:` list) against `skills_map`
/// (`file_config.skills`) into a [`ToolSet`] — one no-argument tool per name,
/// each reading that one skill's full body when called (see
/// `SkillCache::skill_body`). Callers only build this when
/// `default.skill_progressive_disclosure` is `true`; with it off, `names`
/// should be `&[]` so this returns an empty set (see `ToolSet`'s doc
/// comment).
pub(crate) fn tools(names: &[String], skills_map: &config::SkillMap) -> Result<ToolSet> {
    let empty_parameters = serde_json::json!({ "type": "object", "properties": {} });
    let mut tools = Vec::with_capacity(names.len());
    let mut index = HashMap::with_capacity(names.len());
    for name in names {
        if !skills_map.contains_key(name) {
            bail!(
                "unknown skill '{name}'; define it under 'skills:' in {}",
                config::CONFIG_FILE_NAME
            );
        }
        let qualified = mcp::qualify_tool_name("skill", "skill", name)?;
        if index.contains_key(&qualified) {
            bail!("duplicate skill name '{name}' in 'skills:'");
        }
        tools.push(ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: qualified.clone(),
                description: Some(format!(
                    "Read the full instructions for the '{name}' skill. Only its name and \
                     description are shown by default; call this before following it."
                )),
                parameters: Some(empty_parameters.clone()),
                strict: None,
            },
        }));
        index.insert(qualified, name.clone());
    }
    Ok(ToolSet { tools, index })
}

#[cfg(test)]
mod tests {
    use super::{SkillCache, parse_skill, tools};
    #[cfg(unix)]
    use std::time::Duration;
    use std::{collections::HashMap, fs, sync::Arc};

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
        assert!(
            cache
                .render(&[], crate::cancellation::none())
                .await
                .unwrap()
                .is_none()
        );
    }

    /// A second `render` call for the same `names` list must reuse the
    /// already-joined text (a refcount bump) instead of re-joining the
    /// per-name sections from scratch — see `SkillCache::joined`'s doc
    /// comment. `Arc::ptr_eq` distinguishes that from two calls merely
    /// producing equal-but-freshly-allocated strings.
    #[tokio::test]
    async fn a_repeat_render_call_for_the_same_names_reuses_the_cached_join() {
        let path = crate::test_support::unique_temp_path("lait-test-skill-join-cache", ".md");
        fs::write(&path, "---\n---\nbody\n").unwrap();
        let mut skills_map = HashMap::new();
        skills_map.insert("s".to_owned(), path.clone());
        let cache = SkillCache::new(Arc::new(skills_map));
        let names = vec!["s".to_owned()];

        let first = cache
            .render(&names, crate::cancellation::none())
            .await
            .unwrap()
            .unwrap();
        let second = cache
            .render(&names, crate::cancellation::none())
            .await
            .unwrap()
            .unwrap();

        assert!(
            Arc::ptr_eq(&first, &second),
            "expected the second call to reuse the cached joined text"
        );
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn render_errors_on_an_unknown_skill_name() {
        let skills_map = HashMap::new();
        let cache = SkillCache::new(Arc::new(skills_map));
        let error = cache
            .render(&["missing".to_owned()], crate::cancellation::none())
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

        let error = cache
            .render(&["large".to_owned()], crate::cancellation::none())
            .await
            .unwrap_err();
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
        let mut render = Box::pin(cache.render(&names, token.clone()));

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
        assert!(
            crate::error::is_interrupted(&result),
            "cancellation should remain typed: {result:#}"
        );
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
        let mut first = Box::pin(cache.render(&names, first_token));
        tokio::select! {
            result = &mut first => panic!("first render unexpectedly returned: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }

        let token = tokio_util::sync::CancellationToken::new();
        let mut second = Box::pin(cache.render(&names, token.clone()));
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
        assert!(
            crate::error::is_interrupted(&result),
            "cancellation should remain typed: {result:#}"
        );

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

    #[tokio::test]
    async fn render_frontmatter_includes_the_description_and_tool_hint_but_not_the_body() {
        let path = crate::test_support::unique_temp_path("lait-test-skill-frontmatter", ".md");
        fs::write(
            &path,
            "---\nname: code-review\ndescription: reviews a diff for bugs\n---\nLook for off-by-one errors.\n",
        )
        .unwrap();
        let mut skills_map = HashMap::new();
        skills_map.insert("code-review".to_owned(), path.clone());
        let cache = SkillCache::new(Arc::new(skills_map));

        let text = cache
            .render_frontmatter(&["code-review".to_owned()], crate::cancellation::none())
            .await
            .unwrap()
            .unwrap();

        assert!(text.contains("## Skill: code-review"));
        assert!(text.contains("reviews a diff for bugs"));
        assert!(text.contains("skill__code-review"));
        assert!(!text.contains("off-by-one"), "text: {text}");
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn render_frontmatter_returns_none_for_an_empty_name_list() {
        let cache = SkillCache::new(Arc::new(HashMap::new()));
        assert!(
            cache
                .render_frontmatter(&[], crate::cancellation::none())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn skill_body_returns_the_same_full_text_render_would_have_shown() {
        let path = crate::test_support::unique_temp_path("lait-test-skill-body", ".md");
        fs::write(
            &path,
            "---\nname: code-review\ndescription: reviews a diff for bugs\n---\nLook for off-by-one errors.\n",
        )
        .unwrap();
        let mut skills_map = HashMap::new();
        skills_map.insert("code-review".to_owned(), path.clone());
        let cache = SkillCache::new(Arc::new(skills_map));
        let names = ["code-review".to_owned()];

        let rendered = cache
            .render(&names, crate::cancellation::none())
            .await
            .unwrap()
            .unwrap();
        let body = cache
            .skill_body("code-review", crate::cancellation::none())
            .await
            .unwrap();

        assert_eq!(rendered.as_str(), body.as_str());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn tools_qualifies_each_name_and_carries_a_description() {
        let mut skills_map = HashMap::new();
        skills_map.insert(
            "code-review".to_owned(),
            std::path::PathBuf::from("skill.md"),
        );
        let names = vec!["code-review".to_owned()];

        let tool_set = tools(&names, &Arc::new(skills_map)).unwrap();

        assert_eq!(tool_set.tools.len(), 1);
        assert_eq!(
            tool_set.tool_name("skill__code-review"),
            Some("code-review")
        );
        assert!(tool_set.names().eq(["skill__code-review"]));
    }

    #[test]
    fn tools_is_empty_for_an_empty_name_list() {
        let tool_set = tools(&[], &Arc::new(HashMap::new())).unwrap();
        assert!(tool_set.tools.is_empty());
        assert_eq!(tool_set.names().count(), 0);
    }

    #[test]
    fn tools_errors_on_an_unknown_skill_name() {
        let error = tools(&["missing".to_owned()], &Arc::new(HashMap::new())).unwrap_err();
        assert!(error.to_string().contains("missing"));
    }
}
