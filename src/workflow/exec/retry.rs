//! A single step's `on_error`/`retry`/`timeout` handling: [`run_on_error_handler`]
//! (runs a failed step's `on_error` block, if any), [`effective_retry`]/
//! [`effective_timeout`] (the node > workflow-default fallback for either
//! setting), and [`execute_step_with_retry`] (the attempt/backoff loop around
//! `execute_step`). Split out of the parent module — this is what happens
//! *around* a node's action when it fails, as opposed to `settings` (what it
//! needs before running) or `nodes` (the action itself).

use std::{ops::ControlFlow, time::Duration};

use anyhow::{Context, Result, bail};

use crate::{template, workflow};

use super::{
    Flow, RouterContext, RunStepsFrame, StepsOutcome, StepsState, WORKFLOW_EXECUTION_CANCELLED,
    WorkflowScope, check_workflow_cancellation, execute_step, run_steps, settings::StepContext,
};

/// Runs `step`'s `on_error` handler after `error` (from the step's own
/// retried attempts), producing either a [`StepsState`] for the outer loop
/// in [`run_steps`] to continue with (`ControlFlow::Continue`, the handler
/// itself completed normally) or a [`StepsOutcome`] for it to return
/// immediately (`ControlFlow::Break`, the handler's own `Break`/`Stop` —
/// recorded under `step`'s output first, the same recording `run_steps`'s
/// own loop tail does for every other path, which this early return skips).
/// Returns `Err(error)` unchanged (not a handler failure — there is none,
/// since it never ran) when `step` has no `on_error` at all, so the original
/// error propagates to `run_steps`'s own caller as-is.
pub(super) async fn run_on_error_handler(
    step: &workflow::FlowStep,
    error: anyhow::Error,
    counter: usize,
    mut state: StepsState,
    router_context: &RouterContext<'_>,
) -> Result<ControlFlow<StepsOutcome, StepsState>> {
    let Some(on_error) = step.on_error() else {
        return Err(error);
    };
    let progress_prefix = router_context.progress_prefix;
    eprintln!("{progress_prefix}    -> step failed, running 'on_error': {error}");
    let error_input = serde_json::json!({
        "error": format!("{error:#}"),
        "input": template::parse_input(&state.output),
    });
    let error_input_json =
        serde_json::to_string(&error_input).context("failed to serialize 'on_error' input")?;
    let outcome = run_steps(
        &on_error.steps,
        error_input_json,
        state.steps_outputs,
        RunStepsFrame {
            scope: router_context.scope,
            env: router_context.env,
            start_counter: counter,
            progress_prefix,
            cancellation: router_context.cancellation.clone(),
            placement: router_context.placement,
        },
    )
    .await?;
    let flow = outcome.flow;
    state = outcome.into_state();
    if flow != Flow::Continue {
        state.record_output(step);
        return Ok(ControlFlow::Break(state.into_outcome(flow)));
    }
    Ok(ControlFlow::Continue(state))
}

/// The upper bound on a single wait between retry attempts (see
/// `execute_step_with_retry`): a `retry` whose `delay_seconds`/`backoff`
/// (validated non-negative and finite by `workflow::validate`, but free to
/// grow exponentially) would wait longer than this waits this long instead —
/// a bounded, predictable worst case rather than an arbitrarily long hang.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(3600);

/// The `retry` actually in effect for `node`: its own `retry` if set, else
/// (only for a node that calls a model — see `NodeDefinition::calls_model`)
/// `scope`'s `defaults.retry`. Shared by `execute_step_with_retry`, which
/// runs under it, and `dryrun::print_plan`, which only displays it (`lait run
/// --dry-run`).
pub(crate) fn effective_retry<'a>(
    node: &'a workflow::NodeDefinition,
    scope: &'a WorkflowScope,
) -> Option<&'a workflow::RetryDefinition> {
    let settings = node.settings();
    settings.retry.or(node
        .calls_model()
        .then_some(scope.defaults.retry.as_ref())
        .flatten())
}

/// The `timeout` actually in effect for `node`, under the same node-first,
/// model-calling-only fallback rule as [`effective_retry`].
pub(crate) fn effective_timeout(
    node: &workflow::NodeDefinition,
    scope: &WorkflowScope,
) -> Option<u64> {
    let settings = node.settings();
    settings.timeout.or(node
        .calls_model()
        .then_some(scope.defaults.timeout)
        .flatten())
}

/// Runs `execute_step`, applying an effective timeout to each attempt and
/// retrying per an effective `retry` on failure (a timed-out attempt counts
/// as a failure). "Effective" means the node's own `retry`/`timeout` if set,
/// else `scope`'s `defaults.retry`/`defaults.timeout` (see
/// `WorkflowScope::defaults`) — but only for a node that calls a model
/// (`prompt`/`system_prompt`/`agent`, see `NodeDefinition::calls_model`): a
/// `jq`-only or `workflow:` node never falls back to the workflow default
/// (a `workflow:` node's own `retry`/`timeout` are
/// rejected by `validate::validate_node` in favor of the sub-workflow's own
/// steps setting theirs, and applying the *caller's* default on top of that
/// would double up whatever the sub-workflow's own steps already inherit).
/// Returns the last attempt's error once the effective `max_attempts` (or 1,
/// with no effective `retry`) is exhausted; the caller decides whether to run
/// `on_error` or propagate it. `label` is the calling `use:` site's label
/// (not the node's own id), so error messages point at where in the flow the
/// failure happened.
pub(super) async fn execute_step_with_retry(
    node: &workflow::NodeDefinition,
    current_input: &str,
    context: StepContext<'_>,
) -> Result<String> {
    let StepContext {
        scope,
        label,
        progress_prefix,
        step_cancel: workflow_cancel,
        ..
    } = context.clone();

    let effective_retry = effective_retry(node, scope);
    let effective_timeout = effective_timeout(node, scope);

    let max_attempts = effective_retry
        .and_then(|retry| retry.max_attempts)
        .unwrap_or(1);
    let backoff = effective_retry
        .and_then(|retry| retry.backoff)
        .unwrap_or(1.0);
    let mut delay = Duration::from_secs(
        effective_retry
            .and_then(|retry| retry.delay_seconds)
            .unwrap_or(0),
    )
    .min(MAX_RETRY_DELAY);

    let mut attempt = 0usize;
    loop {
        attempt += 1;
        check_workflow_cancellation(&workflow_cancel)?;
        tracing::debug!(step = %label, attempt, max_attempts, "step started");
        let outcome = match effective_timeout {
            // Keep the timeout around the whole node action (including its
            // later jq/write_file work). A cancellation channel is passed to
            // every timed node, not just command nodes: jq and write_file
            // also run outside Tokio and must be told to stop waiting before
            // a retry or an on_error branch starts. The future stays borrowed
            // until cancellation cleanup finishes, avoiding a second attempt
            // racing the child-owning or file-writing future.
            Some(seconds) => {
                // A child token is cancelled both by this node's own timeout
                // (below) and by `workflow_cancel` being cancelled (a
                // `CancellationToken` property, not something forwarded by
                // hand) — `execute_step` only ever needs to watch this one
                // token either way.
                let node_cancel = workflow_cancel.child_token();
                let execution = execute_step(
                    node,
                    current_input,
                    context.with_cancel(node_cancel.clone()),
                );
                tokio::pin!(execution);
                match tokio::time::timeout(Duration::from_secs(seconds), &mut execution).await {
                    Ok(result) => result,
                    Err(_) => {
                        node_cancel.cancel();
                        let _ = execution.await;
                        Err(crate::error::timed_out(format!(
                            "step '{label}' timed out after {seconds}s (attempt {attempt}/{max_attempts})"
                        )))
                    }
                }
            }
            None => {
                execute_step(
                    node,
                    current_input,
                    context.with_cancel(workflow_cancel.clone()),
                )
                .await
            }
        };

        match outcome {
            Ok(output) => {
                tracing::debug!(step = %label, attempt, "step finished");
                return Ok(output);
            }
            Err(error) if attempt < max_attempts => {
                check_workflow_cancellation(&workflow_cancel)?;
                tracing::debug!(
                    step = %label,
                    attempt,
                    max_attempts,
                    error = %error,
                    delay_secs = delay.as_secs_f64(),
                    "step retrying",
                );
                eprintln!(
                    "{progress_prefix}    -> attempt {attempt}/{max_attempts} failed: {error}; retrying in {:.1}s",
                    delay.as_secs_f64()
                );
                wait_retry_delay(delay, &workflow_cancel).await?;
                // `try_from_secs_f64` + the `MAX_RETRY_DELAY` clamp keep an
                // exponentially growing (or pathological) delay from
                // overflowing `Duration` — `Duration::from_secs_f64` would
                // panic there instead of just waiting the capped hour.
                delay = Duration::try_from_secs_f64((delay.as_secs_f64() * backoff).max(0.0))
                    .unwrap_or(MAX_RETRY_DELAY)
                    .min(MAX_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Sleeps between retries while still honoring cancellation inherited from a
/// surrounding workflow. A plain `sleep` would allow a cancelled nested
/// workflow to wait for an arbitrarily large backoff before returning.
async fn wait_retry_delay(
    delay: Duration,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    if delay.is_zero() {
        if cancellation.is_cancelled() {
            bail!(crate::error::cancelled(WORKFLOW_EXECUTION_CANCELLED));
        }
        return Ok(());
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => bail!(crate::error::cancelled(WORKFLOW_EXECUTION_CANCELLED)),
        () = tokio::time::sleep(delay) => Ok(()),
    }
}
