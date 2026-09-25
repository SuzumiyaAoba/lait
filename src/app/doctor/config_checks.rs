//! Checks 2-3 of `lait doctor`: `${VAR}` environment placeholders and
//! `default.model` resolution — both purely local, config-file-only checks
//! that never touch the network or spawn a process (unlike connectivity,
//! MCP servers, or registry file existence), so they're grouped together
//! separately from those.

use crate::config::{self, ConfigFile};

use super::report::Check;

/// Checks 1. every `${VAR}` placeholder in the top-level/model-alias
/// `base_url`/`api_key` fields, and 2. every `mcp_servers:` entry's
/// placeholders (via `McpServerConfig::resolve_transport`, which resolves
/// every field that can carry one) — issue #56's "2. `${VAR}` 参照している
/// 環境変数の存在" check. A field with no `${...}` is skipped entirely rather
/// than reported as trivially OK.
pub(super) fn check_env_placeholders(file_config: &ConfigFile, checks: &mut Vec<Check>) {
    let mut fields: Vec<(String, String)> = Vec::new();
    if let Some(value) = &file_config.base_url {
        fields.push(("top-level base_url".to_owned(), value.clone()));
    }
    if let Some(value) = &file_config.api_key {
        fields.push(("top-level api_key".to_owned(), value.clone()));
    }
    if let Some(value) = &file_config.jev.base_url {
        fields.push(("jev.base_url".to_owned(), value.clone()));
    }
    if let Some(value) = &file_config.jev.api_key {
        fields.push(("jev.api_key".to_owned(), value.clone()));
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
pub(super) fn check_default_model(file_config: &ConfigFile, checks: &mut Vec<Check>) {
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
