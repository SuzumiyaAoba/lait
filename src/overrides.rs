//! The reasoning/sampling/capability "override" types layered by
//! `engine::resolve_request_settings`'s fallback chain (CLI invocation >
//! agent file > workflow node > `lait.config.yml`'s `default:` block), plus
//! `PromptTurn`, the per-call new-turn payload `engine::RequestSettings::
//! complete`/`complete_stream` take.
//!
//! Lives at the crate root rather than under `engine/` so that `cache::key`
//! (which takes a [`SamplingOverrides`] as part of what it hashes into a
//! cache key) does not have to depend on the whole `engine` module. `engine`
//! already depends on `cache` (`RequestSettings::complete_recorded` calls
//! `cache::load`/`cache::key`/`cache::save`), so keeping these types under
//! `engine::` would leave `{engine, cache}` a strongly connected component —
//! a real dependency cycle, unlike the intentional `engine`/`workflow`
//! mutual recursion AGENTS.md documents. `engine` re-exports all four types
//! (`pub(crate) use crate::overrides::{..}`), so every existing
//! `engine::SamplingOverrides`-style call site elsewhere in the crate is
//! unaffected by this split.

use crate::{config, reasoning::ReasoningEffort};
use async_openai::types::chat::ChatCompletionRequestMessage;

/// The reasoning-effort/temperature/top_p/max_tokens knobs a caller (CLI
/// invocation, agent file, or workflow step) may set for a single completion
/// request. Bundled into one struct (rather than four positional parameters)
/// because every layer of `engine::resolve_request_settings`'s fallback chain
/// treats them identically: each field falls back independently to the next
/// layer, unlike e.g. `workflow::RetryDefinition`, which falls back as a
/// whole unit.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SamplingOverrides {
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) max_tokens: Option<u32>,
}

impl SamplingOverrides {
    /// Folds `layers` field by field in priority order (the first layer with
    /// a field set wins, independently per field) — the shared
    /// implementation behind every caller's own precedence chain:
    /// `resolve_chat_settings`'s single `SharedChatArgs` layer,
    /// `resolve_step_settings`'s node > agent file > workflow default, and
    /// `agent_file_settings`'s single frontmatter layer. Does not include the
    /// model-alias/`file_config.default` tail every caller shares —
    /// `engine::resolve_request_settings` adds those two layers after this.
    pub(crate) fn fold(layers: &[Self]) -> Self {
        Self {
            reasoning_effort: layers.iter().find_map(|layer| layer.reasoning_effort),
            temperature: layers.iter().find_map(|layer| layer.temperature),
            top_p: layers.iter().find_map(|layer| layer.top_p),
            max_tokens: layers.iter().find_map(|layer| layer.max_tokens),
        }
    }

    /// `engine::resolve_request_settings`'s tail of the fallback chain
    /// [`fold`] does not cover: `resolved_model`'s own defaults, then
    /// `lait.config.yml`'s `default:` block — each field falling back
    /// independently, same as `fold`.
    ///
    /// [`fold`]: Self::fold
    pub(crate) fn resolve(
        self,
        resolved_model: &config::ResolvedModel,
        default: &config::DefaultSettings,
    ) -> Self {
        Self {
            reasoning_effort: self
                .reasoning_effort
                .or(resolved_model.reasoning_effort)
                .or(default.reasoning_effort),
            temperature: self
                .temperature
                .or(resolved_model.temperature)
                .or(default.temperature),
            top_p: self.top_p.or(resolved_model.top_p).or(default.top_p),
            max_tokens: self
                .max_tokens
                .or(resolved_model.max_tokens)
                .or(default.max_tokens),
        }
    }
}

/// The `mcp`/`max_tool_rounds`/`skills`/`subagents`/`tools` knobs a caller
/// may set for a single completion request, bundled the same way as
/// `SamplingOverrides` and for the same reason (keeps
/// `engine::resolve_request_settings`'s argument count down; each field
/// falls back independently to `file_config.default`, not as a whole unit).
#[derive(Debug, Default, Clone)]
pub(crate) struct CapabilityOverrides {
    pub(crate) mcp: Option<Vec<String>>,
    pub(crate) max_tool_rounds: Option<usize>,
    pub(crate) skills: Option<Vec<String>>,
    pub(crate) subagents: Option<Vec<String>>,
    /// Names of `tools:` entries (see `config::ShellToolDefinition`) made
    /// available as callable shell-command tools during this request's tool
    /// loop. Falls back independently, like `mcp`.
    pub(crate) tools: Option<Vec<String>>,
}

impl CapabilityOverrides {
    /// Folds `layers` field by field in priority order — see
    /// `SamplingOverrides::fold`, which this mirrors, except `layers` is
    /// taken by value: unlike `SamplingOverrides`' `Copy` fields, each field
    /// here is a `Vec<String>` the caller already owns (built fresh per
    /// call, one per layer). A borrowed `&[Self]` would need its own
    /// `.clone()` to move a winning field out of a `&Self` — on top of the
    /// clone the caller already pays constructing its owned `Self` layers —
    /// so every field would be cloned twice. Taking ownership here instead
    /// lets each field move out of whichever layer supplies it, exactly
    /// once, in one pass over `layers`.
    pub(crate) fn fold<const N: usize>(layers: [Self; N]) -> Self {
        let mut folded = Self::default();
        for layer in layers {
            folded.mcp = folded.mcp.or(layer.mcp);
            folded.max_tool_rounds = folded.max_tool_rounds.or(layer.max_tool_rounds);
            folded.skills = folded.skills.or(layer.skills);
            folded.subagents = folded.subagents.or(layer.subagents);
            folded.tools = folded.tools.or(layer.tools);
        }
        folded
    }

    /// `engine::resolve_request_settings`'s tail of the fallback chain
    /// [`fold`] does not cover: `lait.config.yml`'s `default:` block, with
    /// every list-valued field defaulted to empty rather than left `None` —
    /// unlike `max_tool_rounds`, which [`ResolvedCapabilities`] keeps as a
    /// raw `Option` since its caller still has to validate it before
    /// choosing `crate::engine::DEFAULT_MAX_TOOL_ROUNDS`.
    ///
    /// [`fold`]: Self::fold
    pub(crate) fn resolve(self, default: &config::DefaultSettings) -> ResolvedCapabilities {
        ResolvedCapabilities {
            mcp: self.mcp.or_else(|| default.mcp.clone()).unwrap_or_default(),
            max_tool_rounds: self.max_tool_rounds.or(default.max_tool_rounds),
            skills: self
                .skills
                .or_else(|| default.skills.clone())
                .unwrap_or_default(),
            subagents: self
                .subagents
                .or_else(|| default.subagents.clone())
                .unwrap_or_default(),
            tools: self
                .tools
                .or_else(|| default.tools.clone())
                .unwrap_or_default(),
        }
    }
}

/// [`CapabilityOverrides::resolve`]'s result — every field already merged
/// with `lait.config.yml`'s `default:` block, except `max_tool_rounds`,
/// which stays a raw `Option<usize>` because its caller must validate it
/// (`llm::validate_max_tool_rounds`) before substituting
/// `crate::engine::DEFAULT_MAX_TOOL_ROUNDS`.
pub(crate) struct ResolvedCapabilities {
    pub(crate) mcp: Vec<String>,
    pub(crate) max_tool_rounds: Option<usize>,
    pub(crate) skills: Vec<String>,
    pub(crate) subagents: Vec<String>,
    pub(crate) tools: Vec<String>,
}

/// The new-turn inputs shared by `engine::RequestSettings::complete`/
/// `complete_stream`: the system prompt, any prior turns from a resumed
/// `--session` (empty for every caller but chat), the new user-role prompt
/// text, and any `--image` attachments for it (empty for every caller but
/// chat). Bundled into one struct, like `SamplingOverrides`/
/// `CapabilityOverrides` above, to keep `complete`'s argument count under
/// clippy's `too_many_arguments` threshold.
pub(crate) struct PromptTurn<'a> {
    pub(crate) system_prompt: Option<&'a str>,
    pub(crate) history: &'a [ChatCompletionRequestMessage],
    pub(crate) prompt: &'a str,
    pub(crate) image_urls: &'a [String],
}

impl<'a> PromptTurn<'a> {
    /// A turn with no prior history and no image attachments — every caller
    /// but chat's own (`run_chat`/`repl::run_turn`, which have a real
    /// `--session`/`--image` history to carry).
    pub(crate) fn simple(system_prompt: Option<&'a str>, prompt: &'a str) -> Self {
        Self {
            system_prompt,
            history: &[],
            prompt,
            image_urls: &[],
        }
    }
}
