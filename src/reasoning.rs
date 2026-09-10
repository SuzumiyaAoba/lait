//! [`ReasoningEffort`]: the `reasoning_effort:` value shared by the CLI
//! (`--reasoning-effort`), `lait.config.yml` (`default.reasoning_effort`,
//! a model definition's own `reasoning_effort:`), workflow/agent files, and
//! the response cache key. Lives in its own module rather than `cli.rs`
//! (where it originated) because `config.rs`, `workflow/model.rs`, and
//! `agent.rs` all need it purely as a domain value with no CLI concept
//! attached — putting it in `cli.rs` made those into `config -> cli`
//! dependencies for a type that has nothing to do with argument parsing.
//! `clap::ValueEnum` still derives here (so `--reasoning-effort` keeps
//! working directly off this type), but the module's own role is "the
//! domain type CLI happens to borrow", not "a CLI type other layers must
//! reach into" — see the design plan's B4.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, ValueEnum)]
pub(crate) enum ReasoningEffort {
    #[value(name = "none")]
    #[serde(rename = "none")]
    None,
    #[value(name = "minimal")]
    #[serde(rename = "minimal")]
    Minimal,
    #[value(name = "low")]
    #[serde(rename = "low")]
    Low,
    #[value(name = "medium")]
    #[serde(rename = "medium")]
    Medium,
    #[value(name = "high")]
    #[serde(rename = "high")]
    High,
    #[value(name = "xhigh")]
    #[serde(rename = "xhigh")]
    Xhigh,
}

impl ReasoningEffort {
    /// The lowercase name used on the CLI and in YAML, for display (e.g.
    /// `lait models`' DEFAULTS column). Must match the `#[value(name)]`
    /// attributes above — pinned by `as_str_matches_the_clap_value_names`
    /// (`&'static str` is why this can't just call `to_possible_value`).
    /// Now that this type derives `Serialize` (see this module's doc
    /// comment), also matches `#[serde(rename)]` above — pinned indirectly
    /// by `cache::key_output_is_pinned_against_a_fixed_encoding`, which
    /// would fail if a `Serialize` encoding ever disagreed with this.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ReasoningEffort;

    #[test]
    fn as_str_matches_the_clap_value_names() {
        use clap::ValueEnum;

        for variant in ReasoningEffort::value_variants() {
            assert_eq!(
                variant.as_str(),
                variant
                    .to_possible_value()
                    .expect("no reasoning effort variant is skipped")
                    .get_name(),
                "ReasoningEffort::as_str drifted from the #[value(name)] attribute"
            );
        }
    }

    /// The other half of the CLI/`serde` naming guarantee `as_str`'s doc
    /// comment claims: `#[serde(rename)]` must agree with `as_str()` too, not
    /// just `#[value(name)]`. `cache::key_output_is_pinned_against_a_fixed_encoding`
    /// depends on this staying true (see B4's design-plan note on why the
    /// cache key doesn't change when `Serialize` was added).
    #[test]
    fn serialized_name_matches_as_str() {
        for variant in [
            ReasoningEffort::None,
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Xhigh,
        ] {
            let serialized =
                serde_json::to_value(variant).expect("ReasoningEffort should serialize");
            assert_eq!(
                serialized.as_str(),
                Some(variant.as_str()),
                "serde rename drifted from ReasoningEffort::as_str for {variant:?}"
            );
        }
    }
}
