//! The GitHub API client behind `lait deps`'s fetching: resolving a git ref
//! (branch/tag/`None`-for-default-branch) to a commit SHA and downloading a
//! single file's raw bytes at that commit. Both go through the REST API
//! (`GET /repos/{repo}` / `/commits/{ref}` / `/contents/{path}?ref=...`)
//! rather than `raw.githubusercontent.com` so a private repository works
//! the same way once a token is present, and so tests — and GitHub
//! Enterprise — can point the whole client at a different host through
//! `GITHUB_API_URL` (the same variable GitHub Actions sets for GHE).
//!
//! Authentication is `Authorization: Bearer $GITHUB_TOKEN` (or `GH_TOKEN`),
//! read from the process environment — which includes a loaded `.env`, so
//! the token can live there like every other credential this crate reads.
//! Public repositories work without one, subject to GitHub's
//! unauthenticated rate limit (60 requests/hour — enough for `deps add`/
//! `update`/`install` but worth a token in CI).
//!
//! Every request is wrapped in `async_io::await_cancellation` so Ctrl-C
//! interrupts a fetch mid-flight, matching how `llm::complete` treats its
//! own HTTP futures.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::{async_io, error};

/// `async_io::MAX_READ_BYTES`'s 16 MiB bound applies to dependency payloads
/// too — they end up parsed by the same loaders that cap local files, so
/// fetching something larger would only fail later anyway.
const MAX_DEP_BYTES: u64 = async_io::MAX_READ_BYTES as u64;

/// The one HTTP client every deps request goes through, on the same
/// `LazyLock`-shared-client pattern as `llm::HTTP_CLIENT` (see its doc —
/// reqwest pools connections per client). It is a separate client rather
/// than `llm::http_client()` because the settings differ in both
/// directions: a 60 s ceiling here (metadata/fetch calls are small and a
/// hung one should surface quickly, not wait out an LLM-sized 300 s
/// budget) and a `User-Agent`, which the GitHub API requires but an
/// OpenAI-compatible endpoint never asked for.
static HTTP_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("lait/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("a fixed timeout and user agent should produce a valid reqwest client")
});

/// A resolved dependency: the commit `git_ref` pointed at plus the payload
/// bytes at that commit. `add`/`install`/`update` get both in one call so a
/// moving branch cannot be resolved at one commit and downloaded at another
/// (two separate calls would leave a TOCTOU window between them).
pub(crate) struct Fetched {
    pub(crate) commit: String,
    pub(crate) content: Vec<u8>,
}

/// See the module doc. Constructed once per `deps` command; reads
/// `GITHUB_API_URL`/`GITHUB_TOKEN`/`GH_TOKEN` at construction.
pub(crate) struct GitHubClient {
    http: reqwest::Client,
    api_base: String,
    token: Option<String>,
}

impl GitHubClient {
    pub(crate) fn new() -> Self {
        let api_base = std::env::var("GITHUB_API_URL")
            .ok()
            .map(|value| value.trim().trim_end_matches('/').to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "https://api.github.com".to_owned());
        let token = std::env::var("GITHUB_TOKEN")
            .ok()
            .or_else(|| std::env::var("GH_TOKEN").ok())
            .filter(|value| !value.trim().is_empty());
        Self {
            http: HTTP_CLIENT.clone(),
            api_base,
            token,
        }
    }

    /// Builds an absolute API URL from path segments (each already split on
    /// `/`, so `/`-containing refs are passed pre-split by the caller —
    /// GitHub's `commits` endpoint accepts branch names containing slashes
    /// when the whole rest of the path is the ref) plus an optional query
    /// pair. `Url`'s segment-wise push percent-encodes `?`/`#`/UTF-8 in file
    /// paths, which a plain `format!` would smuggle into the query/raw path.
    fn api_url(&self, segments: &[&str], query: Option<(&str, &str)>) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.api_base).with_context(|| {
            format!(
                "invalid GitHub API base URL '{}' (set via GITHUB_API_URL)",
                self.api_base
            )
        })?;
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| anyhow!("GITHUB_API_URL '{}' cannot hold a path", self.api_base))?;
            path.pop_if_empty();
            path.extend(segments.iter().copied());
        }
        if let Some((key, value)) = query {
            url.query_pairs_mut().append_pair(key, value);
        }
        Ok(url)
    }

    /// Sends a GET with the shared auth/version headers and `accept` (the
    /// raw media type for file downloads, JSON elsewhere — the caller picks
    /// because a second `Accept` header would just append, not override),
    /// then maps failures to the same error wording for every caller:
    /// GitHub's `{"message": ...}` error body is surfaced, 404s name the
    /// requested thing, and 401/403 hint at the token environment variables
    /// (a 403 with an exhausted rate limit is the common unauthenticated
    /// failure).
    async fn get(
        &self,
        url: reqwest::Url,
        accept: &str,
        what: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<reqwest::Response> {
        let mut request = self
            .http
            .get(url)
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = match async_io::await_cancellation(request.send(), cancel.clone()).await {
            async_io::CancellationResult::Completed(result) => {
                result.with_context(|| format!("failed to reach the GitHub API for {what}"))?
            }
            async_io::CancellationResult::Cancelled => {
                return Err(error::cancelled("dependency fetch was cancelled"));
            }
        };
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|json| {
                json.get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_owned)
            });
        let detail = message.as_deref().unwrap_or(body.trim());
        let hint = match status.as_u16() {
            // GitHub answers 404 (not 403) for private repositories an
            // unauthenticated request can't see — a missing-token hint
            // belongs on it too, not only on the explicit auth failures.
            401 | 403 | 404 if self.token.is_none() => {
                "; set GITHUB_TOKEN (or GH_TOKEN) if the repository is private or the rate limit is exhausted"
            }
            401 | 403 => "; check GITHUB_TOKEN's permissions",
            _ => "",
        };
        bail!("GitHub API returned {status} for {what}: {detail}{hint}");
    }

    /// Reads a successful response's full body as JSON, bounded by
    /// [`MAX_DEP_BYTES`] and cancellable like the request itself.
    async fn json_body<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
        what: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<T> {
        let bytes = Self::bounded_body(response, what, cancel).await?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse the GitHub response for {what}"))
    }

    /// Reads a successful response's body with the size cap and
    /// cancellation wrapping shared by [`Self::json_body`] and
    /// [`Self::fetch_file`].
    async fn bounded_body(
        response: reqwest::Response,
        what: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<bytes::Bytes> {
        if let Some(length) = response.content_length()
            && length > MAX_DEP_BYTES
        {
            bail!(
                "{what} is {length} bytes, over the {} MiB dependency limit",
                MAX_DEP_BYTES / (1024 * 1024)
            );
        }
        let bytes = match async_io::await_cancellation(response.bytes(), cancel.clone()).await {
            async_io::CancellationResult::Completed(result) => {
                result.with_context(|| format!("failed to download {what}"))?
            }
            async_io::CancellationResult::Cancelled => {
                return Err(error::cancelled("dependency fetch was cancelled"));
            }
        };
        if bytes.len() as u64 > MAX_DEP_BYTES {
            bail!(
                "{what} is over the {} MiB dependency limit",
                MAX_DEP_BYTES / (1024 * 1024)
            );
        }
        Ok(bytes)
    }

    /// Resolves `git_ref` (or the repository's default branch when `None`)
    /// to a commit SHA. `None` costs one extra `GET /repos/{repo}` to learn
    /// `default_branch` — deliberately re-resolved on every `update` rather
    /// than recorded, so a default-branch rename upstream is followed.
    pub(crate) async fn resolve_commit(
        &self,
        repo: &str,
        git_ref: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<String> {
        let (owner, name) = repo
            .split_once('/')
            .context("internal error: dep repo is not 'owner/repo'")?;
        let effective_ref: String;
        let git_ref = match git_ref {
            Some(git_ref) => git_ref,
            None => {
                let url = self.api_url(&["repos", owner, name], None)?;
                let response = self
                    .get(url, "application/vnd.github+json", repo, cancel)
                    .await?;
                #[derive(serde::Deserialize)]
                struct RepoInfo {
                    default_branch: String,
                }
                let info: RepoInfo = Self::json_body(response, repo, cancel).await?;
                effective_ref = info.default_branch;
                effective_ref.as_str()
            }
        };
        // `commits/{ref}` takes the rest of the path as the ref, so a
        // branch like `feature/x` is sent as literal extra segments — see
        // `api_url`'s doc.
        let mut segments = vec!["repos", owner, name, "commits"];
        segments.extend(git_ref.split('/'));
        let url = self.api_url(&segments, None)?;
        let what = format!("{repo}@{git_ref}");
        let response = self
            .get(url, "application/vnd.github+json", &what, cancel)
            .await?;
        #[derive(serde::Deserialize)]
        struct CommitInfo {
            sha: String,
        }
        let info: CommitInfo = Self::json_body(response, &what, cancel).await?;
        Ok(info.sha)
    }

    /// Downloads one file's raw bytes at `commit` via the contents API's
    /// raw media type. A directory path answers with a JSON listing even
    /// under the raw accept, which is how "the spec names a directory" is
    /// detected — directory (multi-file) dependencies are intentionally not
    /// supported yet, so that case gets its own clear error instead of a
    /// parse failure.
    pub(crate) async fn fetch_file(
        &self,
        repo: &str,
        commit: &str,
        path: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Vec<u8>> {
        let (owner, name) = repo
            .split_once('/')
            .context("internal error: dep repo is not 'owner/repo'")?;
        let mut segments = vec!["repos", owner, name, "contents"];
        segments.extend(path.split('/'));
        let url = self.api_url(&segments, Some(("ref", commit)))?;
        let what = format!("{repo}/{path}");
        let response = self
            .get(url, "application/vnd.github.raw+json", &what, cancel)
            .await?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let bytes = Self::bounded_body(response, &what, cancel).await?;
        let looks_like_json_listing = content_type
            .as_deref()
            .is_some_and(|value| value.contains("json"))
            && bytes.first() == Some(&b'[');
        if looks_like_json_listing {
            bail!("'{what}' is a directory; dependencies name a single file");
        }
        Ok(bytes.to_vec())
    }

    /// `resolve_commit` + `fetch_file` in one call — see [`Fetched`]'s doc
    /// for why they are not exposed separately to the ops layer.
    pub(crate) async fn fetch(
        &self,
        spec: &super::spec::GitHubSpec,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Fetched> {
        let commit = self
            .resolve_commit(&spec.repo_slug(), spec.git_ref.as_deref(), cancel)
            .await?;
        let content = self
            .fetch_file(&spec.repo_slug(), &commit, &spec.path, cancel)
            .await?;
        Ok(Fetched { commit, content })
    }
}
