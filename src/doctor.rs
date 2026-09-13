//! `lait doctor`: a one-shot diagnosis of environment/configuration/
//! connectivity issues. Checks run in the order documented in
//! `docs/usage/ja/troubleshooting.md`: config parsing, `${VAR}` environment
//! variables, `default.model` resolution, provider connectivity/auth,
//! whether configured model ids exist on the server, `mcp_servers:` startup,
//! and `agents:`/`skills:` file references. Every check that can be run
//! still runs even after an earlier one fails (mirroring `lint::run`), so one
//! invocation reports everything wrong at once instead of stopping at the
//! first problem.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::{
    cli::DoctorArgs,
    config::{self, ApiKeySource, ConfigFile, ConfigSource},
    engine::AppServices,
    llm, mcp,
};

mod report;

use report::{Check, Status, emit};

/// How long one `mcp_servers:` entry is given to start and initialize before
/// being reported as failed. Much shorter than `mcp`'s own internal
/// (5-minute) initialization timeout, which assumes a real run is willing to
/// wait for a slow-starting server — `doctor` is a quick health check and
/// should not hang on a broken one.
const MCP_CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// How long one `GET {base_url}/models` request is given.
const CONNECTIVITY_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn run(
    args: DoctorArgs,
    config_source: ConfigSource,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<()> {
    let cancellation = cancellation.unwrap_or_default();
    crate::signal::spawn_handler(cancellation.clone());
    let mut checks = Vec::new();

    let file_config = check_config_load(&config_source, &cancellation, &mut checks).await?;
    match &file_config {
        Some(file_config) => run_all_checks(file_config, &cancellation, &mut checks).await?,
        None => {
            checks.push(Check::warn(
                "config",
                "checks 2-7",
                "設定が読めないためスキップしました",
                None,
            ));
        }
    }

    emit(&checks, args.json)?;

    let error_count = checks
        .iter()
        .filter(|check| check.status == Status::Error)
        .count();
    if error_count > 0 {
        bail!("lait doctor found {error_count} error(s)");
    }
    Ok(())
}

/// Loads the config file the same way [`run`]'s remaining checks need it
/// (`Arc`-wrapped, `None` when it failed to parse), pushing the "config"
/// check that reports which of the three outcomes happened: found and
/// parsed, absent (falls back to defaults), or found but unparseable. Mirrors
/// `lint::run`'s own "is there a config at all" detection: unlike
/// `config::load_config`, which returns an empty `ConfigFile` both when
/// `lait.config.yml` is absent and when `--no-config` was passed, this check
/// needs to tell those two apart from "found but failed to parse". Propagates
/// (rather than reporting as a check) a `crate::error::Interrupted` error,
/// matching every other cancellable step in [`run`].
async fn check_config_load(
    config_source: &ConfigSource,
    cancellation: &tokio_util::sync::CancellationToken,
    checks: &mut Vec<Check>,
) -> Result<Option<Arc<ConfigFile>>> {
    let config_path =
        config::resolve_config_path_cancellable(config_source, Some(cancellation.clone())).await?;
    let global_config_present = matches!(config_source, ConfigSource::Search)
        && config::global_config_exists_cancellable(Some(cancellation.clone())).await?;
    let config_present = config_path.is_some() || global_config_present;

    match config::load_config_cancellable(config_source, Some(cancellation.clone())).await {
        Ok(file_config) => {
            if config_present {
                checks.push(Check::ok(
                    "config",
                    config::CONFIG_FILE_NAME,
                    "読み込み・パースに成功しました",
                ));
            } else {
                checks.push(Check::warn(
                    "config",
                    config::CONFIG_FILE_NAME,
                    "設定ファイルが見つかりません（デフォルト設定で動作します）",
                    Some(format!(
                        "プロジェクトルートに {} を作成するか `lait init` を実行してください",
                        config::CONFIG_FILE_NAME
                    )),
                ));
            }
            Ok(Some(Arc::new(file_config)))
        }
        Err(error) => {
            if is_interrupted(&error) {
                return Err(error);
            }
            checks.push(Check::error(
                "config",
                config::CONFIG_FILE_NAME,
                format!("{error:#}"),
                Some(format!(
                    "{} の構文を確認してください",
                    config::CONFIG_FILE_NAME
                )),
            ));
            Ok(None)
        }
    }
}

/// Runs checks 2-7 (env placeholders, default model, connectivity, model
/// presence, MCP servers, agent/skill file references) once a config file
/// has successfully loaded — the checks [`run`] skips entirely (with a
/// single "skipped" warning) when [`check_config_load`] returns `None`.
async fn run_all_checks(
    file_config: &Arc<ConfigFile>,
    cancellation: &tokio_util::sync::CancellationToken,
    checks: &mut Vec<Check>,
) -> Result<()> {
    check_env_placeholders(file_config, checks);
    check_default_model(file_config, checks);
    let uses = resolve_endpoint_uses(file_config);
    let services = Arc::new(AppServices::new(Arc::clone(file_config)));
    let server_models = services
        .clone()
        .finish(check_connectivity(
            &uses,
            &services,
            Some(cancellation.clone()),
            checks,
        ))
        .await?;
    check_models_on_server(&uses, &server_models, checks);
    check_mcp_servers(file_config, checks).await;
    check_registry_files(file_config, checks);
    Ok(())
}

/// Checks 1. every `${VAR}` placeholder in the top-level/model-alias
/// `base_url`/`api_key` fields, and 2. every `mcp_servers:` entry's
/// placeholders (via `McpServerConfig::resolve_transport`, which resolves
/// every field that can carry one) — issue #56's "2. `${VAR}` 参照している
/// 環境変数の存在" check. A field with no `${...}` is skipped entirely rather
/// than reported as trivially OK.
fn check_env_placeholders(file_config: &ConfigFile, checks: &mut Vec<Check>) {
    let mut fields: Vec<(String, String)> = Vec::new();
    if let Some(value) = &file_config.base_url {
        fields.push(("top-level base_url".to_owned(), value.clone()));
    }
    if let Some(value) = &file_config.api_key {
        fields.push(("top-level api_key".to_owned(), value.clone()));
    }

    let mut model_names: Vec<&String> = file_config.models.keys().collect();
    model_names.sort_unstable();
    for name in model_names {
        let Ok(Some(resolved)) = config::resolve_model_alias(name, &file_config.models) else {
            continue;
        };
        if let Some(base_url) = &resolved.base_url {
            fields.push((format!("models.{name}.base_url"), base_url.clone()));
        }
        if let Some(api_key) = &resolved.api_key {
            fields.push((format!("models.{name}.api_key"), api_key.clone()));
        }
    }

    for (label, value) in fields {
        if !value.contains("${") {
            continue;
        }
        match config::expand_env_placeholders(&value) {
            Ok(_) => checks.push(Check::ok(
                "env",
                label,
                "参照している環境変数はすべて設定されています",
            )),
            Err(error) => checks.push(Check::error(
                "env",
                label,
                format!("{error:#}"),
                Some("環境変数を export するか .env に追加してください".to_owned()),
            )),
        }
    }

    let mut server_names: Vec<&String> = file_config.mcp_servers.keys().collect();
    server_names.sort_unstable();
    for name in server_names {
        let server = &file_config.mcp_servers[name];
        match server.resolve_transport(name) {
            Ok(_) => checks.push(Check::ok(
                "env",
                format!("mcp_servers.{name}"),
                "参照している環境変数はすべて設定されています",
            )),
            Err(error) => checks.push(Check::error(
                "env",
                format!("mcp_servers.{name}"),
                format!("{error:#}"),
                Some(
                    "環境変数、または mcp_servers の command/args/env/url/headers の設定を確認してください"
                        .to_owned(),
                ),
            )),
        }
    }
}

/// Checks 3. `default.model` resolves against `models:` (or as a raw model
/// id) — issue #56's "3. default.model とモデルエイリアスの解決" check. An
/// unset `default.model` is a warning, not an error: a caller that always
/// passes `--model` explicitly never needs it.
fn check_default_model(file_config: &ConfigFile, checks: &mut Vec<Check>) {
    match &file_config.default.model {
        None => checks.push(Check::warn(
            "model",
            "default.model",
            "未設定です（--model を都度指定していれば問題ありません）",
            Some(
                "よく使うモデルがあれば lait.config.yml の default.model に設定すると便利です"
                    .to_owned(),
            ),
        )),
        Some(name) => match config::resolve_model(name.clone(), file_config) {
            Ok(resolved) => checks.push(Check::ok(
                "model",
                "default.model",
                format!(
                    "'{name}' は model_id='{}' (base_url={}) に解決されます",
                    resolved.model_id,
                    resolved
                        .base_url
                        .as_deref()
                        .unwrap_or("(top-level base_url)"),
                ),
            )),
            Err(error) => checks.push(Check::error(
                "model",
                "default.model",
                format!("{error:#}"),
                Some(
                    "lait.config.yml の default.model / models: の定義を確認してください"
                        .to_owned(),
                ),
            )),
        },
    }
}

/// One `base_url` a real request would actually go to: either the top-level
/// endpoint (used by a raw model id, or an alias with no `provider.base_url`
/// of its own) or a `models:` alias's own endpoint. Built once by
/// `resolve_endpoint_uses` and shared by the connectivity check (4) and the
/// "model id exists on the server" check (5), so both agree on exactly which
/// endpoints a real run would use.
struct EndpointUse {
    label: String,
    /// `Some(model_id)` when this endpoint came from a `models:` alias —
    /// what check 5 looks for in that endpoint's `/v1/models` response.
    /// `None` for the top-level endpoint, which names no specific model.
    model_id: Option<String>,
    base_url: String,
    api_key: ApiKeySource,
}

/// Resolves every endpoint a real request could hit: the top-level
/// `base_url`/`api_key` (as a raw model id would use it) plus each `models:`
/// alias's own resolved endpoint, using the exact same three-layer
/// resolution (`config::resolve_endpoint`) a real request does. An endpoint
/// that fails to resolve (almost always an unset `${VAR}`, already reported
/// by `check_env_placeholders`) is skipped here rather than reported again.
fn resolve_endpoint_uses(file_config: &ConfigFile) -> Vec<EndpointUse> {
    let mut uses = Vec::new();
    if let Ok(endpoint) = config::resolve_endpoint(None, None, None, file_config) {
        uses.push(EndpointUse {
            label: "top-level".to_owned(),
            model_id: None,
            base_url: endpoint.base_url,
            api_key: endpoint.api_key,
        });
    }

    let mut model_names: Vec<&String> = file_config.models.keys().collect();
    model_names.sort_unstable();
    for name in model_names {
        let Ok(Some(resolved)) = config::resolve_model_alias(name, &file_config.models) else {
            continue;
        };
        let Ok(endpoint) = config::resolve_endpoint(None, None, Some(&resolved), file_config)
        else {
            continue;
        };
        uses.push(EndpointUse {
            label: format!("models.{name}"),
            model_id: Some(resolved.model_id),
            base_url: endpoint.base_url,
            api_key: endpoint.api_key,
        });
    }
    uses
}

/// The subset of a `GET /v1/models` response `doctor` reads — the model ids.
/// Deliberately not shared with `models::list_remote`'s own (private) copy:
/// small enough that duplicating it keeps this module self-contained.
#[derive(Deserialize)]
struct RemoteModelsResponse {
    #[serde(default)]
    data: Vec<RemoteModel>,
}

#[derive(Deserialize)]
struct RemoteModel {
    id: String,
}

/// Checks 4. connectivity/auth against every unique `base_url` in `uses` —
/// issue #56's "4. 各プロバイダー base_url への接続" check. Returns each
/// tested `base_url`'s model id set (`None` when the server couldn't be
/// reached, or its response couldn't be read as a model list), for check 5
/// to cross-reference.
async fn check_connectivity(
    uses: &[EndpointUse],
    services: &AppServices,
    cancellation: Option<tokio_util::sync::CancellationToken>,
    checks: &mut Vec<Check>,
) -> Result<HashMap<String, Option<HashSet<String>>>> {
    let mut ordered_base_urls: Vec<(String, ApiKeySource)> = Vec::new();
    let mut seen = HashSet::new();
    for endpoint_use in uses {
        let key = (endpoint_use.base_url.clone(), endpoint_use.api_key.clone());
        if seen.insert(key) {
            ordered_base_urls.push((endpoint_use.base_url.clone(), endpoint_use.api_key.clone()));
        }
    }

    // Every base_url's secret resolution + `/v1/models` fetch is
    // independent of every other's, so they run concurrently instead of
    // one at a time — with two configured endpoints where one is broken,
    // the sequential version paid `CONNECTIVITY_TIMEOUT` twice in a row.
    // Each endpoint accumulates its own `Check`s in `local_checks` rather
    // than pushing straight into the shared `checks` so the ones for a
    // still-running endpoint can never land between two checks from an
    // endpoint that finished first: they're `extend`ed into `checks` after
    // every endpoint has finished, in `ordered_base_urls`'s original
    // order — `emit`'s text renderer groups by category assuming
    // same-category checks are contiguous, and that order is also what
    // existing tests read entries in.
    let per_endpoint = futures_util::future::join_all(ordered_base_urls.into_iter().map(
        |(base_url, api_key_source)| {
            let cancellation = cancellation.clone();
            async move {
                let mut local_checks = Vec::new();
                let outcome = check_one_endpoint(
                    services,
                    &base_url,
                    &api_key_source,
                    cancellation.as_ref(),
                    &mut local_checks,
                )
                .await;
                (base_url, outcome, local_checks)
            }
        },
    ))
    .await;

    let mut results = HashMap::new();
    for (base_url, outcome, local_checks) in per_endpoint {
        checks.extend(local_checks);
        match outcome {
            Ok(model_ids) => {
                results.insert(base_url, model_ids);
            }
            // Matches the previous sequential behavior of aborting the
            // whole check on the first genuine interruption; which
            // endpoint's cancellation is reported first now follows
            // `ordered_base_urls`'s declared order rather than whichever
            // happened to be cancelled first, which is immaterial — the
            // command is exiting either way.
            Err(error) => return Err(error),
        }
    }
    Ok(results)
}

/// One endpoint's share of [`check_connectivity`]'s work: resolve its API
/// key, then fetch its model list. Split out so it can run inside a
/// per-endpoint future without the borrow-checker friction of mutating a
/// shared `checks: &mut Vec<Check>` from several concurrent closures.
async fn check_one_endpoint(
    services: &AppServices,
    base_url: &str,
    api_key_source: &ApiKeySource,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
    checks: &mut Vec<Check>,
) -> Result<Option<HashSet<String>>> {
    let Some(api_key) = connectivity_step(
        services
            .secret_resolver
            .resolve(api_key_source, cancellation.cloned())
            .await,
        base_url,
        |error| format!("API キーの解決に失敗しました: {error:#}"),
        Some("api_key/api_key_cmd の設定と secret manager の状態を確認してください".to_owned()),
        checks,
    )?
    else {
        return Ok(None);
    };
    fetch_models(base_url, api_key.as_deref(), cancellation, checks).await
}

async fn fetch_models(
    base_url: &str,
    api_key: Option<&str>,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
    checks: &mut Vec<Check>,
) -> Result<Option<HashSet<String>>> {
    let url = format!("{base_url}/models");
    let mut request = llm::http_client().get(&url).timeout(CONNECTIVITY_TIMEOUT);
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }

    let Some(response) = connectivity_step(
        await_with_cancellation(request.send(), cancellation).await,
        base_url,
        |error| format!("接続に失敗しました: {error:#}"),
        Some("base_url とサーバーの起動状態を確認してください".to_owned()),
        checks,
    )?
    else {
        return Ok(None);
    };
    let status = response.status();
    let Some(body) = connectivity_step(
        await_with_cancellation(response.text(), cancellation).await,
        base_url,
        |error| format!("応答の読み取りに失敗しました: {error:#}"),
        None,
        checks,
    )?
    else {
        return Ok(None);
    };
    if !status.is_success() {
        checks.push(Check::error(
            "connectivity",
            base_url.to_owned(),
            format!("GET {url} が {status} を返しました: {}", body.trim()),
            Some("base_url・API キー・サーバーの起動状態を確認してください".to_owned()),
        ));
        return Ok(None);
    }

    match serde_json::from_str::<RemoteModelsResponse>(&body) {
        Ok(parsed) => {
            let ids: HashSet<String> = parsed.data.into_iter().map(|model| model.id).collect();
            checks.push(Check::ok(
                "connectivity",
                base_url.to_owned(),
                format!("接続に成功しました（{}個のモデルを確認）", ids.len()),
            ));
            Ok(Some(ids))
        }
        Err(error) => {
            checks.push(Check::warn(
                "connectivity",
                base_url.to_owned(),
                "接続には成功しましたが、応答をモデル一覧として解釈できませんでした",
                Some(format!("{error:#}")),
            ));
            Ok(None)
        }
    }
}

/// Unwraps one step of `check_one_endpoint`/`fetch_models`'s fallible
/// pipeline (resolving an API key, sending the request, reading its body):
/// a cancellation propagates unchanged (`?` at the call site), an ordinary
/// failure pushes a `Check::error` under the shared `"connectivity"`
/// category and yields `None` for the caller to `return Ok(None)` on, and
/// success yields `Some(value)` to keep going with. `message`/`hint` are the
/// only things that differ between the three call sites this replaces.
fn connectivity_step<T>(
    result: Result<T>,
    base_url: &str,
    message: impl FnOnce(&anyhow::Error) -> String,
    hint: Option<String>,
    checks: &mut Vec<Check>,
) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
            if is_interrupted(&error) {
                return Err(error);
            }
            checks.push(Check::error(
                "connectivity",
                base_url.to_owned(),
                message(&error),
                hint,
            ));
            Ok(None)
        }
    }
}

fn is_interrupted(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<crate::error::Interrupted>())
}

async fn await_with_cancellation<T, E>(
    future: impl Future<Output = std::result::Result<T, E>>,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    match cancellation {
        Some(cancellation) => {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(crate::error::cancelled(
                    "doctor connectivity check was cancelled",
                )),
                result = future => result.map_err(anyhow::Error::new),
            }
        }
        None => future.await.map_err(anyhow::Error::new),
    }
}

/// Checks 5. every `models:` alias's `model_id` appears in its endpoint's
/// `/v1/models` response — issue #56's "5. 設定済みモデル ID がサーバーに
/// 存在するか" check. Skipped (with a note, not a failure) for an endpoint
/// whose model list couldn't be fetched at all.
fn check_models_on_server(
    uses: &[EndpointUse],
    server_models: &HashMap<String, Option<HashSet<String>>>,
    checks: &mut Vec<Check>,
) {
    for endpoint_use in uses {
        let Some(model_id) = &endpoint_use.model_id else {
            continue;
        };
        match server_models.get(&endpoint_use.base_url) {
            Some(Some(ids)) if ids.contains(model_id) => checks.push(Check::ok(
                "models_on_server",
                endpoint_use.label.clone(),
                format!("'{model_id}' はサーバーのモデル一覧に存在します"),
            )),
            Some(Some(_)) => checks.push(Check::warn(
                "models_on_server",
                endpoint_use.label.clone(),
                format!("'{model_id}' はサーバーのモデル一覧に見つかりませんでした"),
                Some(
                    "model_id の誤りか、サーバー側でモデルがロードされていないかを確認してください"
                        .to_owned(),
                ),
            )),
            Some(None) | None => checks.push(Check::warn(
                "models_on_server",
                endpoint_use.label.clone(),
                "サーバーのモデル一覧を取得できなかったためスキップしました",
                None,
            )),
        }
    }
}

/// Checks 6. every `mcp_servers:` entry actually starts and initializes —
/// issue #56's "6. mcp_servers: の各サーバーの起動・初期化" check. Every
/// server is connected to (and its tools listed) concurrently rather than
/// one at a time — `McpRegistry::tools` already connects concurrently
/// internally when given every name in one call, but that call fails the
/// whole batch on the first server's error, which would lose the
/// per-server diagnostic this check exists to produce; checking servers
/// one future per name, each still bounded by `MCP_CHECK_TIMEOUT` so a
/// broken server can't make `doctor` hang, keeps that diagnostic while
/// still connecting every server in parallel. Every connection this opens
/// is shut down before returning, whether it succeeded or not.
async fn check_mcp_servers(file_config: &ConfigFile, checks: &mut Vec<Check>) {
    if file_config.mcp_servers.is_empty() {
        return;
    }
    let servers = Arc::new(file_config.mcp_servers.clone());
    let registry = mcp::McpRegistry::new(Arc::clone(&servers));

    let mut names: Vec<&String> = servers.keys().collect();
    names.sort_unstable();
    let server_checks = futures_util::future::join_all(names.into_iter().map(|name| {
        let registry = &registry;
        async move {
            let outcome = tokio::time::timeout(
                MCP_CHECK_TIMEOUT,
                registry.tools(std::slice::from_ref(name), None),
            )
            .await;
            match outcome {
                Ok(Ok(tool_set)) => Check::ok(
                    "mcp",
                    name.clone(),
                    format!(
                        "起動・初期化に成功しました（{}個のツール）",
                        tool_set.tools.len()
                    ),
                ),
                Ok(Err(error)) => Check::error(
                    "mcp",
                    name.clone(),
                    format!("{error:#}"),
                    Some(
                        "command/args/env、または url/headers の設定を確認してください".to_owned(),
                    ),
                ),
                Err(_) => Check::error(
                    "mcp",
                    name.clone(),
                    format!("{}秒でタイムアウトしました", MCP_CHECK_TIMEOUT.as_secs()),
                    Some("サーバーが正しく起動・応答するか手動で確認してください".to_owned()),
                ),
            }
        }
    }))
    .await;
    checks.extend(server_checks);

    registry.shutdown().await;
}

/// Checks 7. every `agents:`/`skills:` entry's path actually exists — issue
/// #56's "7. agents:/skills: が参照するファイルの存在" check. A `skills:`
/// entry may name either a file or a directory (containing a `SKILL.md`),
/// see `config::SkillMap`.
fn check_registry_files(file_config: &ConfigFile, checks: &mut Vec<Check>) {
    let mut agent_names: Vec<&String> = file_config.agents.keys().collect();
    agent_names.sort_unstable();
    for name in agent_names {
        let path = &file_config.agents[name];
        if path.is_file() {
            checks.push(Check::ok(
                "files",
                format!("agents.{name}"),
                format!("{} が存在します", path.display()),
            ));
        } else {
            checks.push(Check::error(
                "files",
                format!("agents.{name}"),
                format!("{} が見つかりません", path.display()),
                Some("パスを確認するか、ファイルを作成してください".to_owned()),
            ));
        }
    }

    let mut skill_names: Vec<&String> = file_config.skills.keys().collect();
    skill_names.sort_unstable();
    for name in skill_names {
        let path = &file_config.skills[name];
        if path.is_file() || path.is_dir() {
            checks.push(Check::ok(
                "files",
                format!("skills.{name}"),
                format!("{} が存在します", path.display()),
            ));
        } else {
            checks.push(Check::error(
                "files",
                format!("skills.{name}"),
                format!("{} が見つかりません", path.display()),
                Some("パスを確認するか、ファイル/ディレクトリを作成してください".to_owned()),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EndpointUse, Status, check_connectivity, check_default_model, check_env_placeholders,
        check_registry_files, resolve_endpoint_uses,
    };
    use crate::config::{ApiKeySource, CommandSpec, ConfigFile};
    use crate::engine::AppServices;
    use std::sync::Arc;

    fn parse_config(yaml: &str) -> ConfigFile {
        serde_yaml::from_str(yaml).expect("test config should parse")
    }

    #[test]
    fn default_model_unset_is_a_warning() {
        let config = parse_config("{}");
        let mut checks = Vec::new();
        check_default_model(&config, &mut checks);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Warn);
    }

    #[test]
    fn default_model_resolving_to_an_alias_is_ok() {
        let config = parse_config(
            r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: http://localhost:1234/v1
      model_id: test-model-id
"#,
        );
        let mut checks = Vec::new();
        check_default_model(&config, &mut checks);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Ok);
        assert!(checks[0].message.contains("test-model-id"));
    }

    #[test]
    fn default_model_naming_an_empty_alias_is_an_error() {
        let config = parse_config(
            r#"
default:
  model: broken
models:
  broken: []
"#,
        );
        let mut checks = Vec::new();
        check_default_model(&config, &mut checks);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Error);
    }

    #[test]
    fn env_placeholder_check_skips_fields_without_a_placeholder() {
        let config = parse_config(
            r#"
base_url: http://localhost:1234/v1
"#,
        );
        let mut checks = Vec::new();
        check_env_placeholders(&config, &mut checks);
        assert!(checks.is_empty());
    }

    #[test]
    fn env_placeholder_check_reports_an_unset_variable() {
        let config = parse_config(
            r#"
base_url: ${LAIT_DOCTOR_TEST_DEFINITELY_UNSET_VAR}
"#,
        );
        let mut checks = Vec::new();
        check_env_placeholders(&config, &mut checks);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Error);
        assert!(
            checks[0]
                .message
                .contains("LAIT_DOCTOR_TEST_DEFINITELY_UNSET_VAR")
        );
    }

    #[test]
    fn env_placeholder_check_reports_a_broken_mcp_server_definition() {
        let config = parse_config(
            r#"
mcp_servers:
  broken: {}
"#,
        );
        let mut checks = Vec::new();
        check_env_placeholders(&config, &mut checks);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Error);
        assert_eq!(checks[0].name, "mcp_servers.broken");
    }

    #[test]
    fn registry_files_check_reports_a_missing_agent_path() {
        let config = parse_config(
            r#"
agents:
  missing: /nonexistent/path/agent.md
"#,
        );
        let mut checks = Vec::new();
        check_registry_files(&config, &mut checks);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Error);
        assert_eq!(checks[0].name, "agents.missing");
    }

    #[test]
    fn registry_files_check_accepts_an_existing_file() {
        let path = std::env::current_dir().expect("cwd").join("Cargo.toml");
        let config = parse_config(&format!(
            "agents:\n  cargo: {:?}\n",
            path.to_str().expect("utf-8 path")
        ));
        let mut checks = Vec::new();
        check_registry_files(&config, &mut checks);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Ok);
    }

    #[test]
    fn resolve_endpoint_uses_includes_the_top_level_endpoint_and_every_alias() {
        let config = parse_config(
            r#"
base_url: http://localhost:1234/v1
models:
  local:
    - provider:
        base_url: http://localhost:5678/v1
      model_id: test-model-id
"#,
        );
        let uses = resolve_endpoint_uses(&config);
        assert_eq!(uses.len(), 2);
        assert!(
            uses.iter()
                .any(|u| u.label == "top-level" && u.base_url == "http://localhost:1234/v1")
        );
        assert!(uses.iter().any(|u| u.label == "models.local"
            && u.base_url == "http://localhost:5678/v1"
            && u.model_id.as_deref() == Some("test-model-id")));
    }

    #[tokio::test]
    async fn connectivity_propagates_cancelled_secret_resolution() {
        let services = AppServices::new(Arc::new(ConfigFile::default()));
        let uses = vec![
            EndpointUse {
                label: "command".to_owned(),
                model_id: None,
                base_url: "http://127.0.0.1:1/v1".to_owned(),
                api_key: ApiKeySource::Command(CommandSpec::Argv(vec![
                    "command-must-not-run".to_owned(),
                ])),
            },
            EndpointUse {
                label: "never-reached".to_owned(),
                model_id: None,
                base_url: "http://127.0.0.1:2/v1".to_owned(),
                api_key: ApiKeySource::Absent,
            },
        ];
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();
        let mut checks = Vec::new();
        let error = check_connectivity(&uses, &services, Some(cancellation), &mut checks)
            .await
            .unwrap_err();

        assert!(
            error
                .chain()
                .any(|cause| cause.is::<crate::error::Interrupted>())
        );
        assert!(
            checks.is_empty(),
            "cancellation must not be absorbed as a check"
        );
    }
}
