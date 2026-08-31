//! Cycle/depth-limit checking shared by `workflow:` node nesting
//! (`WorkflowScope::nested`, `lint::lint_sub_workflow`) and subagent nesting
//! (`call_subagent_tool`) — two otherwise-unrelated features that both need
//! to reject a self-referential file before recursing into it. Lives outside
//! `workflow/` (rather than moving in alongside `check_workflow_nesting`'s
//! only workflow-specific caller) precisely because it is not workflow-only.

use std::path::{Path, PathBuf};

/// The maximum `workflow:` nesting depth (a workflow step calling another
/// workflow file, whose own steps may call another, ...), rejected as a
/// runtime error rather than left to overflow the stack or hang.
pub(crate) const MAX_WORKFLOW_DEPTH: usize = 32;

/// Why entering a self-referential file (a `workflow:` node or a subagent)
/// failed `check_nesting_depth` below.
pub(crate) enum NestingDepthError {
    /// The file is already on the call stack.
    Cycle,
    /// Entering it would exceed the caller's `max_depth`.
    TooDeep,
}

/// Whether entering `canonical` from `active` (every file of the same kind
/// currently on the call stack, canonicalized) would create a cycle or
/// exceed `max_depth`. The generic core behind `check_workflow_nesting`
/// (`workflow:` nodes, `max_depth` = `MAX_WORKFLOW_DEPTH`) and
/// `call_subagent_tool` (`subagents:` calls, `max_depth` =
/// `MAX_SUBAGENT_DEPTH`), so both kinds of self-referential file nesting
/// share one cycle/depth-limit check instead of two copies of the same two
/// comparisons.
pub(crate) fn check_nesting_depth(
    active: &[PathBuf],
    canonical: &Path,
    max_depth: usize,
) -> Result<(), NestingDepthError> {
    if active.iter().any(|path| path == canonical) {
        return Err(NestingDepthError::Cycle);
    }
    if active.len() >= max_depth {
        return Err(NestingDepthError::TooDeep);
    }
    Ok(())
}

/// Whether entering `canonical` from `active` (every `workflow:` file
/// currently on the call stack, canonicalized) would create a cycle or
/// exceed `MAX_WORKFLOW_DEPTH`. Shared by `WorkflowScope::nested` (fails the
/// whole run) and `lint::lint_sub_workflow` (reports it as one more issue and
/// keeps linting the rest of the file), so the two can't drift on what counts
/// as too deep or cyclic.
pub(crate) fn check_workflow_nesting(
    active: &[PathBuf],
    canonical: &Path,
) -> Result<(), NestingDepthError> {
    check_nesting_depth(active, canonical, MAX_WORKFLOW_DEPTH)
}
