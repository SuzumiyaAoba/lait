//! Per-node request settings and attachment resolution: [`StepContext`] (the
//! bundle `execute_step_with_retry`/`execute_step` take instead of six
//! separate parameters), [`resolve_step_settings`] (the node > agent file >
//! workflow default precedence chain for a model-calling node), and
//! [`resolve_attachments`] (`files:`/`images:` resolution shared by the
//! `agent`/`prompt` node kinds). Split out of the parent module — these are
//! what a node needs *before* it runs, as opposed to `retry` (the loop that
//! actually runs it) or `nodes` (each node kind's own action).

use std::{borrow::Cow, path::PathBuf};

use anyhow::{Result, anyhow};

use crate::{
    agent::AgentFile,
    attachment,
    config::{self, ConfigFile},
    engine::{
        CapabilityOverrides, EndpointOverrides, RequestSettings, RunContext, SamplingOverrides,
        resolve_request_settings,
    },
    workflow::{self, WorkflowScope},
};

use super::{ExecutionPlacement, StepContextExt};

/// The state a single node execution needs, bundled so
/// `execute_step_with_retry`/`execute_step` take one parameter instead of
/// six. `step_cancel` is the cancellation in effect for this particular
/// attempt — the caller's own cancellation on the first attempt of a node
/// with no `timeout`, or a child token scoped to just that attempt when a
/// `timeout` is set (see `execute_step_with_retry`) — which is why it lives
/// here rather than on `RunContext`: it changes across attempts and nesting
/// depths, unlike everything on `RunContext`, which does not.
#[derive(Clone)]
pub(super) struct StepContext<'a> {
    pub(super) scope: &'a WorkflowScope,
    pub(super) env: &'a RunContext,
    pub(super) placement: ExecutionPlacement,
    pub(super) label: &'a str,
    pub(super) progress_prefix: &'a str,
    pub(super) steps_outputs: &'a workflow::StepOutputs,
    pub(super) step_cancel: Option<tokio_util::sync::CancellationToken>,
}

impl<'a> StepContext<'a> {
    /// Returns a copy of this context with `step_cancel` swapped for
    /// `step_cancel` — used by `execute_step_with_retry` to hand
    /// `execute_step` an attempt-scoped child token (when the node has an
    /// effective `timeout`) or the unmodified workflow token (when it
    /// doesn't), without repeating every other field at each call site.
    pub(super) fn with_cancel(
        &self,
        step_cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Self {
        Self {
            step_cancel,
            ..self.clone()
        }
    }
}

/// Resolves the model/reasoning-effort settings for a node's model call,
/// applying the node > agent file (when this node has one) > workflow
/// default precedence chain shared by `execute_step`'s `agent` and `prompt`
/// branches. `agent_file` is `Some` only for an `agent` node; besides adding
/// its fallback layer, its presence also selects which hint text a
/// missing-model error uses. Also called (read-only, no network) by
/// `dryrun::print_plan` to display the resolved model/base_url for `lait run
/// --dry-run`.
pub(crate) fn resolve_step_settings(
    node: &workflow::NodeDefinition,
    scope: &WorkflowScope,
    file_config: &ConfigFile,
    agent_file: Option<&AgentFile>,
    label: &str,
) -> Result<RequestSettings> {
    let node_settings = node.settings();
    let model_name = node_settings
        .model
        .map(str::to_owned)
        .or_else(|| agent_file.and_then(|agent_file| agent_file.model.clone()))
        .or_else(|| scope.defaults.model.clone())
        .or_else(|| file_config.default.model.clone())
        .ok_or_else(|| {
            anyhow!(
                "model is required for step '{label}'; set it on the node,{} the workflow's default.model, or in {}",
                if agent_file.is_some() { " its agent file," } else { "" },
                config::CONFIG_FILE_NAME
            )
        })?;
    // Each layer mirrors one of the node > agent file > workflow default
    // precedence chain's own sources; `agent_file`'s is absent (`Default`,
    // all `None`/empty) when this node has none. `SamplingOverrides::fold`/
    // `CapabilityOverrides::fold` then pick the first layer with each field
    // set, independently per field.
    let node_sampling = SamplingOverrides {
        reasoning_effort: node_settings.reasoning_effort,
        temperature: node_settings.temperature,
        top_p: node_settings.top_p,
        max_tokens: node_settings.max_tokens,
    };
    let agent_sampling = agent_file
        .map(|agent_file| SamplingOverrides {
            reasoning_effort: agent_file.reasoning_effort,
            temperature: agent_file.temperature,
            top_p: agent_file.top_p,
            max_tokens: agent_file.max_tokens,
        })
        .unwrap_or_default();
    let workflow_sampling = SamplingOverrides {
        reasoning_effort: scope.defaults.reasoning_effort,
        temperature: scope.defaults.temperature,
        top_p: scope.defaults.top_p,
        max_tokens: scope.defaults.max_tokens,
    };
    let overrides = SamplingOverrides::fold(&[node_sampling, agent_sampling, workflow_sampling]);

    let node_capability = CapabilityOverrides {
        mcp: node_settings.mcp.map(<[String]>::to_vec),
        max_tool_rounds: node_settings.max_tool_rounds,
        skills: node_settings.skills.map(<[String]>::to_vec),
        subagents: node_settings.subagents.map(<[String]>::to_vec),
        tools: node_settings.tools.map(<[String]>::to_vec),
    };
    let agent_capability = agent_file
        .map(|agent_file| CapabilityOverrides {
            mcp: agent_file.mcp.clone(),
            max_tool_rounds: agent_file.max_tool_rounds,
            skills: agent_file.skills.clone(),
            subagents: agent_file.subagents.clone(),
            tools: agent_file.tools.clone(),
        })
        .unwrap_or_default();
    let workflow_capability = CapabilityOverrides {
        mcp: scope.defaults.mcp.clone(),
        max_tool_rounds: scope.defaults.max_tool_rounds,
        skills: scope.defaults.skills.clone(),
        subagents: scope.defaults.subagents.clone(),
        tools: scope.defaults.tools.clone(),
    };
    let capability_overrides =
        CapabilityOverrides::fold([node_capability, agent_capability, workflow_capability]);

    resolve_request_settings(
        model_name,
        overrides,
        EndpointOverrides::default(),
        capability_overrides,
        &scope.models,
        file_config,
    )
    .step(label)
}

/// Resolves a node's `files:`/`images:` attachments against `base_prompt`:
/// file contents become a named fenced code block appended after it
/// (`base_prompt` unchanged when `files` is unset), and image paths/URLs
/// resolve into `image_url` content parts for the caller's eventual
/// `AgentTurn`/`PromptTurn`. The two kinds are read/resolved concurrently
/// since they're otherwise-independent I/O. Shared by `execute_step`'s
/// `Agent` and `Prompt` arms, which each attach to a different "base" user
/// message (the current input passed through unchanged, vs. the rendered
/// `prompt` template) — takes `files`/`images` directly (rather than a
/// `&workflow::NodeDefinition`) since only those two variants have either
/// field.
pub(super) async fn resolve_attachments<'a>(
    files: Option<&[PathBuf]>,
    images: Option<&[String]>,
    base_prompt: &'a str,
    label: &str,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<(Cow<'a, str>, Vec<String>)> {
    let (file_context, image_urls) = tokio::try_join!(
        attachment::read_file_attachments_cancellable(files.unwrap_or(&[]), cancellation.clone(),),
        attachment::resolve_image_urls_cancellable(images.unwrap_or(&[]), cancellation),
    )
    .step(label)?;
    let prompt = match file_context {
        Some(context) => Cow::Owned(format!("{base_prompt}\n\n{context}")),
        None => Cow::Borrowed(base_prompt),
    };
    Ok((prompt, image_urls))
}
