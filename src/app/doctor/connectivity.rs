//! Checks 4-5 of `lait doctor`: provider connectivity/auth against every
//! configured endpoint, and whether each `models:` alias's `model_id`
//! actually appears on its server. Split out of the parent module because
//! `reqwest` (via `llm::http_client()`) and the `RemoteModel`/
//! `RemoteModelsResponse` response shape are otherwise entirely confined to
//! this pair of checks — no other check in `doctor` talks to a network.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    time::Duration,
};

use anyhow::Result;
use serde::Deserialize;

use crate::{
    config::{self, ApiKeySource, ConfigFile},
    engine::AppServices,
    error::is_interrupted,
    llm,
};

use super::report::Check;

/// How long one `GET {base_url}/models` request is given.
const CONNECTIVITY_TIMEOUT: Duration = Duration::from_secs(10);

/// One `base_url` a real request would actually go to: either the top-level
/// endpoint (used by a raw model id, or an alias with no `provider.base_url`
/// of its own) or a `models:` alias's own endpoint. Built once by
/// `resolve_endpoint_uses` and shared by the connectivity check (4) and the
/// "model id exists on the server" check (5), so both agree on exactly which
/// endpoints a real run would use.
pub(super) struct EndpointUse {
    pub(super) label: String,
    /// `Some(model_id)` when this endpoint came from a `models:` alias —
    /// what check 5 looks for in that endpoint's `/v1/models` response.
    /// `None` for the top-level endpoint, which names no specific model.
    pub(super) model_id: Option<String>,
    pub(super) base_url: String,
    pub(super) api_key: ApiKeySource,
}

/// Resolves every endpoint a real request could hit: the top-level
/// `base_url`/`api_key` (as a raw model id would use it) plus each `models:`
/// alias's own resolved endpoint, using the exact same three-layer
/// resolution (`config::resolve_endpoint`) a real request does. An endpoint
/// that fails to resolve (almost always an unset `${VAR}`, already reported
/// by `check_env_placeholders`) is skipped here rather than reported again.
pub(super) fn resolve_endpoint_uses(file_config: &ConfigFile) -> Vec<EndpointUse> {
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
pub(super) async fn check_connectivity(
    uses: &[EndpointUse],
    services: &AppServices,
    cancellation: tokio_util::sync::CancellationToken,
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
                    &cancellation,
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
    cancellation: &tokio_util::sync::CancellationToken,
    checks: &mut Vec<Check>,
) -> Result<Option<HashSet<String>>> {
    let Some(api_key) = connectivity_step(
        services
            .secret_resolver
            .resolve(api_key_source, cancellation.clone())
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
    cancellation: &tokio_util::sync::CancellationToken,
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

async fn await_with_cancellation<T, E>(
    future: impl Future<Output = std::result::Result<T, E>>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(crate::error::cancelled(
            "doctor connectivity check was cancelled",
        )),
        result = future => result.map_err(anyhow::Error::new),
    }
}

/// Checks 5. every `models:` alias's `model_id` appears in its endpoint's
/// `/v1/models` response — issue #56's "5. 設定済みモデル ID がサーバーに
/// 存在するか" check. Skipped (with a note, not a failure) for an endpoint
/// whose model list couldn't be fetched at all.
pub(super) fn check_models_on_server(
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
