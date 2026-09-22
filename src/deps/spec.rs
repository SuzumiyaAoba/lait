//! Parsing of the dependency source specifier accepted by `lait deps add`
//! and stored in `lait.deps.yml` entries (`source:`), plus the shared
//! [`DepKind`] classification that decides which `lait.config.yml` registry
//! (`workflows:`/`agents:`/`skills:`) a materialized dependency joins.
//!
//! Accepted spellings (all name the same four pieces — owner, repo, an
//! in-repo file path, and an optional git ref):
//!
//! - `OWNER/REPO/PATH[@REF]` — the shorthand.
//! - `github:OWNER/REPO/PATH[@REF]` — the canonical form written back to
//!   `lait.deps.yml` by `deps add` ([`GitHubSpec::to_source`]).
//! - `https://github.com/OWNER/REPO/(blob|raw|tree)/REF/PATH` — a file page
//!   or raw link pasted from the browser (the scheme may be omitted).
//! - `https://raw.githubusercontent.com/OWNER/REPO/REF/PATH`.
//!
//! For the URL forms the ref is the single path segment after `blob`/`raw`/
//! `tree` — a branch name containing `/` cannot be expressed that way
//! (GitHub itself resolves such URLs greedily); use `@REF` or `--ref` for
//! those.

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// Which `lait.config.yml` registry a dependency's materialized file joins:
/// `workflows:` (`lait run <NAME>`), `agents:` (`--subagent`/`subagents:`/
/// `lait agent run <NAME>`), or `skills:` (`skills:` lists). Derived at
/// `deps add` time from `--kind` or the source path's extension, then kept
/// explicit in both `lait.deps.yml` and `lait.lock` so the classification
/// survives even if the source path later becomes ambiguous.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DepKind {
    Workflow,
    Agent,
    Skill,
}

impl DepKind {
    /// The lowercase YAML/CLI spelling, for messages.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Workflow => "workflow",
            Self::Agent => "agent",
            Self::Skill => "skill",
        }
    }

    /// Infers the kind from a source path's extension: `.yml`/`.yaml` is a
    /// workflow, `.md` is an agent — except a `SKILL.md` basename, which is
    /// a skill (matching `skill::resolve_skill_file_path`'s convention).
    /// `None` for anything else; callers then require an explicit `--kind`.
    pub(crate) fn infer(path: &str) -> Option<Self> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".yml") || lower.ends_with(".yaml") {
            return Some(Self::Workflow);
        }
        if lower.ends_with(".md") {
            return Some(
                if path_basename(path).is_some_and(|base| base.eq_ignore_ascii_case("skill.md")) {
                    Self::Skill
                } else {
                    Self::Agent
                },
            );
        }
        None
    }
}

/// A parsed dependency source: one file in a GitHub repository at an
/// optional git ref (branch, tag, or commit — `None` means the repository's
/// default branch, resolved via the API at fetch time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitHubSpec {
    pub(crate) owner: String,
    pub(crate) repo: String,
    /// In-repo path to the file, `/`-separated, no leading slash.
    pub(crate) path: String,
    pub(crate) git_ref: Option<String>,
}

impl GitHubSpec {
    /// The `"owner/repo"` spelling GitHub's API and `lait.lock` use.
    pub(crate) fn repo_slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// The canonical `github:OWNER/REPO/PATH` form `deps add` writes into
    /// `lait.deps.yml`. The ref is deliberately not embedded — the manifest
    /// records it separately in its own `ref:` field, so the string written
    /// here stays stable when only the ref changes.
    pub(crate) fn to_source(&self) -> String {
        format!("github:{}/{}/{}", self.owner, self.repo, self.path)
    }
}

/// The last `/`-separated segment of a repo path, for name derivation and
/// the materialized file name.
pub(crate) fn path_basename(path: &str) -> Option<&str> {
    path.rsplit('/').find(|segment| !segment.is_empty())
}

/// Derives the registry name `deps add` uses when `--name` is not given:
/// the file basename without its extension (so `workflows/review.yml`
/// becomes `review`), or — for a `SKILL.md` — the containing directory's
/// name (so `skills/review/SKILL.md` still yields `review`, not `SKILL`).
pub(crate) fn derive_name(path: &str) -> Result<String> {
    let basename = path_basename(path).context("the source path has no file name")?;
    let stem = if basename.eq_ignore_ascii_case("skill.md") {
        let parent = path[..path.len() - basename.len()]
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        parent.to_owned()
    } else {
        basename
            .rsplit_once('.')
            .map(|(stem, _)| stem)
            .unwrap_or(basename)
            .to_owned()
    };
    validate_name(&stem)?;
    Ok(stem)
}

/// The character set a dependency name may use. Names double as registry
/// keys (`lait run <NAME>`, `--subagent <NAME>`, `skills: [NAME]`, the
/// `agent__<NAME>`/`skill__<NAME>` qualified tool names), so they are
/// restricted to what every one of those contexts accepts unambiguously.
pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid dependency name '{name}'; use letters, digits, '-' or '_' (or pass --name)");
    }
    Ok(())
}

/// Checks a GitHub owner or repo segment: ASCII alphanumerics plus `-`,
/// `_`, `.` (GitHub allows `.`/`_`/`-` in repo names and `-` in owner
/// names; the union is accepted here — a genuinely wrong name surfaces as a
/// 404 at fetch time rather than a parse error).
fn check_segment(segment: &str, what: &str) -> Result<()> {
    if segment.is_empty()
        || segment == "."
        || segment == ".."
        || !segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("invalid {what} '{segment}' in dependency source");
    }
    Ok(())
}

/// Validates an in-repo path: `/`-separated, no empty/`.`/`..` segments
/// (so materializing it under `.lait/deps/` can never escape its directory),
/// no leading or trailing slash, and no characters that would need URL
/// encoding or be rejected by GitHub (`\`, whitespace, `?`, `#`, `%`,
/// control characters). Other characters — including non-ASCII file names —
/// are accepted; `github.rs` percent-encodes path segments when building
/// the request URL.
fn check_repo_path(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("the dependency source must name a file inside the repository");
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            bail!("invalid path '{path}' in dependency source (empty, '.' or '..' segment)");
        }
        if !segment
            .chars()
            .all(|c| !c.is_control() && !c.is_whitespace() && !matches!(c, '\\' | '?' | '#' | '%'))
        {
            bail!("invalid path '{path}' in dependency source (unsupported character)");
        }
    }
    Ok(())
}

fn check_ref(git_ref: &str) -> Result<()> {
    if git_ref.is_empty()
        || git_ref.starts_with('-')
        || git_ref.contains("..")
        || !git_ref
            .chars()
            .all(|c| !c.is_control() && !c.is_whitespace() && c != '\\')
    {
        bail!("invalid git ref '{git_ref}' in dependency source");
    }
    Ok(())
}

/// Splits a shorthand `TAIL[@REF]` on its last `@`. A `@` inside the path
/// (e.g. `icons/gear@2x.yml`) is genuinely ambiguous; the last-`@` rule
/// matches `go get`/`npm`-style convention, and `--ref` remains the
/// unambiguous escape for such file names.
fn split_ref(tail: &str) -> Result<(String, Option<String>)> {
    match tail.rsplit_once('@') {
        Some((head, git_ref)) if !head.is_empty() => {
            check_ref(git_ref)?;
            Ok((head.to_owned(), Some(git_ref.to_owned())))
        }
        _ => Ok((tail.to_owned(), None)),
    }
}

fn finish(owner: &str, repo: &str, path: &str, git_ref: Option<String>) -> Result<GitHubSpec> {
    check_segment(owner, "repository owner")?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    check_segment(repo, "repository name")?;
    check_repo_path(path)?;
    Ok(GitHubSpec {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        path: path.to_owned(),
        git_ref,
    })
}

/// Parses a `github:`-prefixed or bare `OWNER/REPO/PATH[@REF]` shorthand.
fn parse_shorthand(body: &str) -> Result<GitHubSpec> {
    let (body, git_ref) = split_ref(body)?;
    let mut segments = body.split('/');
    let owner = segments.next().unwrap_or_default();
    let repo = segments.next().unwrap_or_default();
    let path = segments.collect::<Vec<_>>().join("/");
    if path.is_empty() {
        bail!(
            "the dependency source '{body}' must include a path inside the repository \
             (OWNER/REPO/PATH)"
        );
    }
    finish(owner, repo, &path, git_ref)
}

/// Parses a `github.com` or `raw.githubusercontent.com` URL (scheme
/// optional). Returns `None` when `source` does not start like either host,
/// so [`parse`] can fall through to the shorthand forms.
fn parse_url(source: &str) -> Result<Option<GitHubSpec>> {
    let body = source
        .strip_prefix("https://")
        .or_else(|| source.strip_prefix("http://"))
        .unwrap_or(source);
    if let Some(rest) = body.strip_prefix("github.com/") {
        let segments: Vec<&str> = rest.split('/').collect();
        // /OWNER/REPO/(blob|raw|tree)/REF/PATH...
        if segments.len() >= 5
            && matches!(segments[2], "blob" | "raw" | "tree")
            && !segments[4..].is_empty()
        {
            let git_ref = segments[3];
            check_ref(git_ref)?;
            let path = segments[4..].join("/");
            return finish(segments[0], segments[1], &path, Some(git_ref.to_owned())).map(Some);
        }
        bail!(
            "unsupported GitHub URL '{source}'; expected \
             https://github.com/OWNER/REPO/blob/REF/PATH"
        );
    }
    if let Some(rest) = body
        .strip_prefix("raw.githubusercontent.com/")
        .or_else(|| body.strip_prefix("media.githubusercontent.com/"))
    {
        let segments: Vec<&str> = rest.splitn(4, '/').collect();
        if segments.len() == 4 {
            let git_ref = segments[2];
            check_ref(git_ref)?;
            return finish(
                segments[0],
                segments[1],
                segments[3],
                Some(git_ref.to_owned()),
            )
            .map(Some);
        }
        bail!(
            "unsupported GitHub URL '{source}'; expected \
             https://raw.githubusercontent.com/OWNER/REPO/REF/PATH"
        );
    }
    if body.starts_with("github.com") || body.contains("githubusercontent.com") {
        bail!("unsupported GitHub URL '{source}'");
    }
    Ok(None)
}

/// Parses any accepted dependency source spelling into a [`GitHubSpec`].
/// See the module doc for the accepted forms.
pub(crate) fn parse(source: &str) -> Result<GitHubSpec> {
    let source = source.trim();
    if let Some(spec) = parse_url(source)? {
        return Ok(spec);
    }
    let body = source.strip_prefix("github:").unwrap_or(source);
    parse_shorthand(body).with_context(|| format!("invalid dependency source '{source}'"))
}
