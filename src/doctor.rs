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
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    cli::DoctorArgs,
    config::{self, ConfigFile, ConfigSource},
    llm, mcp,
};

/// How long one `mcp_servers:` entry is given to start and initialize before
/// being reported as failed. Much shorter than `mcp`'s own internal
/// (5-minute) initialization timeout, which assumes a real run is willing to
/// wait for a slow-starting server — `doctor` is a quick health check and
/// should not hang on a broken one.
const MCP_CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// How long one `GET {base_url}/models` request is given.
const CONNECTIVITY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Warn,
    Error,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Status::Ok => "OK",
            Status::Warn => "WARN",
            Status::Error => "NG",
        })
    }
}

/// One diagnostic finding. `category` groups related checks in the report
/// (`config`/`env`/`model`/`connectivity`/`models_on_server`/`mcp`/`files`);
/// `name` identifies what was checked within that category (a config key, a
/// base URL, a server name, ...).
#[derive(Debug, Serialize)]
struct Check {
    category: String,
    name: String,
    status: Status,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

impl Check {
    fn ok(category: &str, name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            category: category.to_owned(),
            name: name.into(),
            status: Status::Ok,
            message: message.into(),
            hint: None,
        }
    }

    fn warn(
        category: &str,
        name: impl Into<String>,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self {
            category: category.to_owned(),
            name: name.into(),
            status: Status::Warn,
            message: message.into(),
            hint,
        }
    }

    fn error(
        category: &str,
        name: impl Into<String>,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self {
            category: category.to_owned(),
            name: name.into(),
            status: Status::Error,
            message: message.into(),
            hint,
        }
    }
}

pub(crate) async fn run(args: DoctorArgs, config_source: ConfigSource) -> Result<()> {
    let mut checks = Vec::new();

    // Mirrors `lint::run`'s own "is there a config at all" detection: unlike
    // `config::load_config`, which returns an empty `ConfigFile` both when
    // `lait.config.yml` is absent and when `--no-config` was passed, this
    // check needs to tell those two apart from "found but failed to parse".
    let config_path = config::resolve_config_path(&config_source)?;
    let global_config_present =
        matches!(config_source, ConfigSource::Search) && config::global_config_path()?.is_file();
    let config_present = config_path.is_some() || global_config_present;

    let file_config = match config::load_config(&config_source) {
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
            Some(file_config)
        }
        Err(error) => {
            checks.push(Check::error(
                "config",
                config::CONFIG_FILE_NAME,
                format!("{error:#}"),
                Some(format!(
                    "{} の構文を確認してください",
                    config::CONFIG_FILE_NAME
                )),
            ));
            None
        }
    };

    match &file_config {
        Some(file_config) => {
            check_env_placeholders(file_config, &mut checks);
            check_default_model(file_config, &mut checks);
            let uses = resolve_endpoint_uses(file_config);
            let server_models = check_connectivity(&uses, &mut checks).await;
            check_models_on_server(&uses, &server_models, &mut checks);
            check_mcp_servers(file_config, &mut checks).await;
            check_registry_files(file_config, &mut checks);
        }
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

fn emit(checks: &[Check], json: bool) -> Result<()> {
    let ok = checks.iter().filter(|c| c.status == Status::Ok).count();
    let warn = checks.iter().filter(|c| c.status == Status::Warn).count();
    let error = checks.iter().filter(|c| c.status == Status::Error).count();

    if json {
        let output = serde_json::json!({
            "checks": checks,
            "summary": {"ok": ok, "warn": warn, "error": error},
        });
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    let mut last_category: Option<&str> = None;
    for check in checks {
        if last_category != Some(check.category.as_str()) {
            println!("== {} ==", check.category);
            last_category = Some(&check.category);
        }
        println!("  [{}] {}: {}", check.status, check.name, check.message);
        if let Some(hint) = &check.hint {
            println!("        hint: {hint}");
        }
    }
    println!("\n{ok} OK, {warn} WARN, {error} NG");
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
    api_key: Option<String>,
}

/// Resolves every endpoint a real request could hit: the top-level
/// `base_url`/`api_key` (as a raw model id would use it) plus each `models:`
/// alias's own resolved endpoint, using the exact same three-layer
/// resolution (`config::resolve_endpoint`) a real request does. An endpoint
/// that fails to resolve (almost always an unset `${VAR}`, already reported
/// by `check_env_placeholders`) is skipped here rather than reported again.
fn resolve_endpoint_uses(file_config: &ConfigFile) -> Vec<EndpointUse> {
    let mut uses = Vec::new();
    if let Ok((base_url, api_key)) =
        config::resolve_endpoint(None, None, None, None, None, file_config)
    {
        uses.push(EndpointUse {
            label: "top-level".to_owned(),
            model_id: None,
            base_url,
            api_key,
        });
    }

    let mut model_names: Vec<&String> = file_config.models.keys().collect();
    model_names.sort_unstable();
    for name in model_names {
        let Ok(Some(resolved)) = config::resolve_model_alias(name, &file_config.models) else {
            continue;
        };
        let Ok((base_url, api_key)) = config::resolve_endpoint(
            None,
            None,
            resolved.base_url.as_deref(),
            resolved.api_key.as_deref(),
            resolved.api_key_cmd.as_ref(),
            file_config,
        ) else {
            continue;
        };
        uses.push(EndpointUse {
            label: format!("models.{name}"),
            model_id: Some(resolved.model_id),
            base_url,
            api_key,
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
    checks: &mut Vec<Check>,
) -> HashMap<String, Option<HashSet<String>>> {
    let mut ordered_base_urls: Vec<(String, Option<String>)> = Vec::new();
    let mut seen = HashSet::new();
    for endpoint_use in uses {
        if seen.insert(endpoint_use.base_url.clone()) {
            ordered_base_urls.push((endpoint_use.base_url.clone(), endpoint_use.api_key.clone()));
        }
    }

    let mut results = HashMap::new();
    for (base_url, api_key) in ordered_base_urls {
        let model_ids = fetch_models(&base_url, api_key.as_deref(), checks).await;
        results.insert(base_url, model_ids);
    }
    results
}

async fn fetch_models(
    base_url: &str,
    api_key: Option<&str>,
    checks: &mut Vec<Check>,
) -> Option<HashSet<String>> {
    let url = format!("{base_url}/models");
    let mut request = llm::http_client().get(&url).timeout(CONNECTIVITY_TIMEOUT);
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            checks.push(Check::error(
                "connectivity",
                base_url.to_owned(),
                format!("接続に失敗しました: {error:#}"),
                Some("base_url とサーバーの起動状態を確認してください".to_owned()),
            ));
            return None;
        }
    };
    let status = response.status();
    let body = match response.text().await {
        Ok(body) => body,
        Err(error) => {
            checks.push(Check::error(
                "connectivity",
                base_url.to_owned(),
                format!("応答の読み取りに失敗しました: {error:#}"),
                None,
            ));
            return None;
        }
    };
    if !status.is_success() {
        checks.push(Check::error(
            "connectivity",
            base_url.to_owned(),
            format!("GET {url} が {status} を返しました: {}", body.trim()),
            Some("base_url・API キー・サーバーの起動状態を確認してください".to_owned()),
        ));
        return None;
    }

    match serde_json::from_str::<RemoteModelsResponse>(&body) {
        Ok(parsed) => {
            let ids: HashSet<String> = parsed.data.into_iter().map(|model| model.id).collect();
            checks.push(Check::ok(
                "connectivity",
                base_url.to_owned(),
                format!("接続に成功しました（{}個のモデルを確認）", ids.len()),
            ));
            Some(ids)
        }
        Err(_) => {
            checks.push(Check::warn(
                "connectivity",
                base_url.to_owned(),
                "接続には成功しましたが、応答をモデル一覧として解釈できませんでした",
                None,
            ));
            None
        }
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
/// issue #56's "6. mcp_servers: の各サーバーの起動・初期化" check. Each
/// server is connected to (and its tools listed) one at a time, bounded by
/// `MCP_CHECK_TIMEOUT` so a broken server can't make `doctor` hang; every
/// connection this opens is shut down before returning, whether it
/// succeeded or not.
async fn check_mcp_servers(file_config: &ConfigFile, checks: &mut Vec<Check>) {
    if file_config.mcp_servers.is_empty() {
        return;
    }
    let servers = Arc::new(file_config.mcp_servers.clone());
    let registry = mcp::McpRegistry::new(Arc::clone(&servers));

    let mut names: Vec<&String> = servers.keys().collect();
    names.sort_unstable();
    for name in names {
        let outcome = tokio::time::timeout(
            MCP_CHECK_TIMEOUT,
            registry.tools(std::slice::from_ref(name), None),
        )
        .await;
        match outcome {
            Ok(Ok(tool_set)) => checks.push(Check::ok(
                "mcp",
                name.clone(),
                format!(
                    "起動・初期化に成功しました（{}個のツール）",
                    tool_set.tools.len()
                ),
            )),
            Ok(Err(error)) => checks.push(Check::error(
                "mcp",
                name.clone(),
                format!("{error:#}"),
                Some("command/args/env、または url/headers の設定を確認してください".to_owned()),
            )),
            Err(_) => checks.push(Check::error(
                "mcp",
                name.clone(),
                format!("{}秒でタイムアウトしました", MCP_CHECK_TIMEOUT.as_secs()),
                Some("サーバーが正しく起動・応答するか手動で確認してください".to_owned()),
            )),
        }
    }

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
        Status, check_default_model, check_env_placeholders, check_registry_files,
        resolve_endpoint_uses,
    };
    use crate::config::ConfigFile;

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
}
