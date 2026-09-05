//! Stateful tool-call protocol shared by streamed and non-streamed requests.
//!
//! A completion round has two independent concerns: how the model response is
//! obtained (an HTTP response or an SSE stream), and how the conversation is
//! advanced after the model asks for tools. Keeping the latter state here
//! prevents the two transport paths from growing subtly different histories,
//! round limits, or dispatch behaviour.

use anyhow::{Result, bail};
use async_openai::types::chat::{ChatCompletionRequestMessage, ChatCompletionTools};
use std::path::PathBuf;

use crate::{llm, mcp, response, shell_tool, subagent};

use super::{AppContext, ToolDecision, call_subagent_tool, tool_decision};

/// The mutable conversation state for one tool-enabled completion.
pub(super) struct ToolLoop {
    messages: Vec<ChatCompletionRequestMessage>,
    mcp_tool_set: mcp::ToolSet,
    subagent_tool_set: subagent::ToolSet,
    shell_tool_set: shell_tool::ToolSet,
    tools: Vec<ChatCompletionTools>,
    round: usize,
}

impl ToolLoop {
    pub(super) fn new(
        messages: Vec<ChatCompletionRequestMessage>,
        mcp_tool_set: mcp::ToolSet,
        subagent_tool_set: subagent::ToolSet,
        shell_tool_set: shell_tool::ToolSet,
        tools: Vec<ChatCompletionTools>,
    ) -> Self {
        Self {
            messages,
            mcp_tool_set,
            subagent_tool_set,
            shell_tool_set,
            tools,
            round: 0,
        }
    }

    /// Starts the next model round and enforces the request's tool-loop cap.
    /// Returning the round number lets callers apply transport-specific
    /// presentation rules (for example, appending a later streamed round to
    /// an output file) without owning a second counter.
    pub(super) fn next_round(&mut self, max_tool_rounds: usize) -> Result<usize> {
        self.round += 1;
        if self.round > max_tool_rounds {
            bail!(
                "tool loop exceeded max_tool_rounds ({max_tool_rounds}) without the model producing a final response"
            );
        }
        Ok(self.round)
    }

    /// Returns a snapshot suitable for an `llm::CompletionRequest`.
    /// Requests own their message vector, while the loop must retain its
    /// history for the following tool round.
    pub(super) fn messages_snapshot(&self) -> Vec<ChatCompletionRequestMessage> {
        self.messages.clone()
    }

    /// Consumes the loop when the final response format re-issue is made.
    /// Ownership makes it impossible for a caller to accidentally continue a
    /// loop after its protocol state has been finalized.
    pub(super) fn into_messages(self) -> Vec<ChatCompletionRequestMessage> {
        self.messages
    }

    pub(super) fn tools(&self) -> &[ChatCompletionTools] {
        &self.tools
    }

    /// Appends the assistant tool-call message and all tool results in the
    /// same deterministic order used by both completion transports.
    pub(super) async fn append_tool_calls(
        &mut self,
        tool_calls: &[response::ToolCall],
        content: Option<&str>,
        env: &AppContext,
        active_agent_paths: &[PathBuf],
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<()> {
        self.messages
            .push(llm::assistant_tool_call_message(tool_calls, content)?);

        // Approval is intentionally decided sequentially, before dispatch:
        // several concurrent prompts would interleave on stdin/stderr and
        // could not be answered reliably.
        let mut decisions = Vec::with_capacity(tool_calls.len());
        for tool_call in tool_calls {
            let command_preview = || {
                self.shell_tool_set
                    .tool_name(&tool_call.function.name)
                    .and_then(|name| env.file_config.tools.get(name))
                    .and_then(|definition| {
                        shell_tool::preview_argv(definition, &tool_call.function.arguments)
                    })
            };
            decisions.push(
                tool_decision(
                    env,
                    &tool_call.function.name,
                    &tool_call.function.arguments,
                    command_preview,
                    cancellation.clone(),
                )
                .await?,
            );
        }

        // A model turn's allowed calls are independent. `try_join_all`
        // preserves the input order so the resulting tool messages are
        // deterministic even when the calls finish in a different order.
        let tool_messages =
            futures_util::future::try_join_all(tool_calls.iter().zip(decisions).map(
                |(tool_call, decision)| async {
                    let name = &tool_call.function.name;
                    if let ToolDecision::Deny(reason) = decision {
                        tracing::debug!(
                            tool = %name,
                            round = self.round,
                            reason = %reason,
                            "tool call denied",
                        );
                        return llm::tool_result_message(&tool_call.id, reason);
                    }
                    tracing::debug!(
                        tool = %name,
                        arguments = %tool_call.function.arguments,
                        round = self.round,
                        "calling tool",
                    );
                    let result = if self.mcp_tool_set.contains(name) {
                        env.registry
                            .call(
                                &self.mcp_tool_set,
                                name,
                                &tool_call.function.arguments,
                                cancellation.clone(),
                            )
                            .await?
                    } else if let Some(subagent_name) = self.subagent_tool_set.subagent_name(name) {
                        call_subagent_tool(
                            subagent_name,
                            &tool_call.function.arguments,
                            env,
                            active_agent_paths,
                            cancellation.clone(),
                        )
                        .await?
                    } else if let Some(tool_name) = self.shell_tool_set.tool_name(name) {
                        let definition = &env.file_config.tools[tool_name];
                        shell_tool::call(
                            definition,
                            &tool_call.function.arguments,
                            cancellation.clone(),
                        )
                        .await?
                    } else {
                        bail!("model called unknown tool '{name}'");
                    };
                    llm::tool_result_message(&tool_call.id, result)
                },
            ))
            .await?;
        self.messages.extend(tool_messages);
        Ok(())
    }
}
