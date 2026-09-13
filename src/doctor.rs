//! `lait doctor`: a one-shot diagnosis of environment/configuration/
//! connectivity issues. Checks run in the order documented in
//! `docs/usage/ja/troubleshooting.md`: config parsing, `${VAR}` environment
//! variables, `default.model` resolution, provider connectivity/auth,
//! whether configured model ids exist on the server, `mcp_servers:` startup,
//! and `agents:`/`skills:` file references. Every check that can be run
//! still runs even after an earlier one fails (mirroring `lint::run`), so one
//! invocation reports everything wrong at once instead of stopping at the
//! first problem.

use std::{sync::Arc, time::Duration};

use anyhow::{Result, bail};

use crate::{
    cli::DoctorArgs,
    config::{self, ConfigFile, ConfigSource},
    engine::AppServices,
    error::is_interrupted,
    mcp,
};

mod config_checks;
mod connectivity;
mod report;

use config_checks::{check_default_model, check_env_placeholders};
use connectivity::{check_connectivity, check_models_on_server, resolve_endpoint_uses};
use report::{Check, Status, emit};

/// How long one `mcp_servers:` entry is given to start and initialize before
/// being reported as failed. Much shorter than `mcp`'s own internal
/// (5-minute) initialization timeout, which assumes a real run is willing to
/// wait for a slow-starting server — `doctor` is a quick health check and
/// should not hang on a broken one.
const MCP_CHECK_TIMEOUT: Duration = Duration::from_secs(15);

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
///
/// Connectivity (an HTTP round-trip per configured endpoint, up to
/// `CONNECTIVITY_TIMEOUT` each) and MCP servers (a child-process/HTTP
/// handshake per server, up to `MCP_CHECK_TIMEOUT` each) are otherwise
/// independent checks that both used to run one after the other — with a
/// broken endpoint and a broken MCP server configured together, that paid
/// both timeouts back to back. `try_join!` (not `join!`) runs them
/// concurrently while still preserving the original short-circuit-on-error
/// behavior: a genuine interruption from either side drops the other future
/// immediately rather than waiting out its own timeout first.
async fn run_all_checks(
    file_config: &Arc<ConfigFile>,
    cancellation: &tokio_util::sync::CancellationToken,
    checks: &mut Vec<Check>,
) -> Result<()> {
    check_env_placeholders(file_config, checks);
    check_default_model(file_config, checks);
    let uses = resolve_endpoint_uses(file_config);
    let services = Arc::new(AppServices::new(Arc::clone(file_config)));
    let (server_models, mcp_checks) = tokio::try_join!(
        services.clone().finish(check_connectivity(
            &uses,
            &services,
            Some(cancellation.clone()),
            checks,
        )),
        async { Ok::<_, anyhow::Error>(check_mcp_servers(file_config).await) },
    )?;
    checks.extend(mcp_checks);
    check_models_on_server(&uses, &server_models, checks);
    check_registry_files(file_config, checks);
    Ok(())
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
/// is shut down before returning, whether it succeeded or not. Returns its
/// own `Vec<Check>` (rather than appending to a shared one, as it used to)
/// so [`run_all_checks`] can run it concurrently with [`check_connectivity`]
/// — both need `&mut Vec<Check>` otherwise, which can't be held by two
/// futures polled at once — mirroring the local-`Vec`-then-`extend` pattern
/// [`check_connectivity`] already uses internally for the same reason.
async fn check_mcp_servers(file_config: &ConfigFile) -> Vec<Check> {
    if file_config.mcp_servers.is_empty() {
        return Vec::new();
    }
    let servers = file_config.mcp_servers.clone();
    let registry = mcp::McpRegistry::new(servers.clone());

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

    registry.shutdown().await;
    server_checks
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
    use super::config_checks::{check_default_model, check_env_placeholders};
    use super::connectivity::{EndpointUse, check_connectivity, resolve_endpoint_uses};
    use super::{Status, check_registry_files};
    use crate::config::{ApiKeySource, CommandSpec, ConfigFile};
    use crate::engine::AppServices;
    use crate::error::is_interrupted;
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

        assert!(is_interrupted(&error));
        assert!(
            checks.is_empty(),
            "cancellation must not be absorbed as a check"
        );
    }
}
