//! Top-level workflow orchestration, resume state, and checkpoint publication.

use std::sync::Arc;

use anyhow::{Result, bail};

use crate::{
    async_io, chat, checkpoint,
    cli::RunArgs,
    config::ConfigSource,
    engine::RunContext,
    report, template, trace,
    workflow::{
        self, WorkflowScope,
        exec::{Flow, Frame, Outcome, State, announce_named_file, finish_output, run_steps},
    },
};

/// Runtime progress between top-level steps. Keeping the state together avoids
/// mixing a control step's nested counter with the checkpoint's top-level
/// position.
struct Progress {
    completed_index: usize,
    state: State,
}

/// Immutable metadata shared by every snapshot in a run.
struct CheckpointContext<'a> {
    run_id: &'a str,
    workflow_path: &'a str,
    initial_input: &'a serde_json::Value,
    inputs: &'a serde_json::Map<String, serde_json::Value>,
    labels: &'a [String],
}

impl CheckpointContext<'_> {
    /// Every field here is borrowed (see `checkpoint::CheckpointRef`'s doc
    /// comment) — this write happens after *every* top-level step, and
    /// `progress.state.steps` in particular grows by one entry per completed
    /// step, so building an owned `Checkpoint` first would deep-clone it
    /// again on every single write.
    async fn save(
        &self,
        progress: &Progress,
        status: checkpoint::RunStatus,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        checkpoint::save_cancellable(
            &checkpoint::CheckpointRef {
                run_id: self.run_id,
                workflow_path: self.workflow_path,
                initial_input: self.initial_input,
                inputs: self.inputs,
                top_level_labels: self.labels,
                completed_index: progress.completed_index,
                counter: progress.state.counter,
                current_value: &progress.state.value,
                steps_outputs: &progress.state.steps,
                status,
            },
            cancellation,
        )
        .await
    }
}

/// Owns the deadline task so an early return cannot leave a timer running.
struct RunDeadline(Option<tokio::task::JoinHandle<()>>);

impl RunDeadline {
    fn start(seconds: Option<u64>, cancel: tokio_util::sync::CancellationToken) -> Self {
        Self(seconds.map(|seconds| tokio::spawn(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {},
                () = tokio::time::sleep(std::time::Duration::from_secs(seconds)) => {
                    eprintln!("lait: the workflow's 'timeout' ({seconds}s) was exceeded; cancelling the run");
                    cancel.cancel();
                }
            }
        })))
    }
}

impl Drop for RunDeadline {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

/// Every top-level step's stable label, by position: its `id`, else
/// `step-<position>` (1-based). Deliberately *not* the same value
/// `run_steps`' own progress-counter fallback would produce — that counter
/// only exists once a run is actually executing (it also counts nested
/// steps), whereas this only needs to name each top-level position stably,
/// before anything has run, so `checkpoint::check_resumable` can detect
/// whether the step sequence changed since a checkpoint was written.
fn top_level_step_labels(steps: &[workflow::Step]) -> Vec<String> {
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| step.label_or(index + 1))
        .collect()
}

/// Rejects a `--resume` target that cannot resume against this invocation:
/// checkpointed against a different workflow file, or a run that already
/// completed. A no-op when `resumed` is `None` (a fresh run, not a resume).
fn check_resume_compatible(
    resumed: Option<&checkpoint::Checkpoint>,
    workflow_path: &str,
) -> Result<()> {
    let Some(resumed) = resumed else {
        return Ok(());
    };
    if resumed.workflow_path != workflow_path {
        bail!(
            "run '{}' was checkpointed against workflow '{}', not '{workflow_path}'; pass \
             the same FILE to resume it",
            resumed.run_id,
            resumed.workflow_path,
        );
    }
    if resumed.status == checkpoint::RunStatus::Completed {
        bail!(
            "run '{}' already completed; nothing to resume",
            resumed.run_id
        );
    }
    Ok(())
}

/// Resolves the initial value, resolved `inputs`, and starting [`Progress`]
/// a run begins from — a `--resume` target restores all three from its
/// checkpoint (unless `--input` overrides its saved inputs), while a fresh
/// run resolves the initial value from `PROMPT`/stdin and starts `Progress`
/// at the beginning.
async fn resolve_run_start(
    run_args: &RunArgs,
    wf: &workflow::WorkflowFile,
    resumed: Option<&checkpoint::Checkpoint>,
    top_level_labels: &[String],
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(
    serde_json::Value,
    serde_json::Map<String, serde_json::Value>,
    Progress,
)> {
    match resumed {
        Some(resumed) => {
            checkpoint::check_resumable(top_level_labels, resumed)?;
            eprintln!(
                "==> resuming run '{}' from step {}/{}",
                resumed.run_id,
                resumed.completed_index + 1,
                top_level_labels.len(),
            );
            let inputs = if run_args.input.is_empty() {
                resumed.inputs.clone()
            } else {
                resolve_cli_inputs(wf, &run_args.input)?
            };
            Ok((
                resumed.initial_input.clone(),
                inputs,
                Progress {
                    completed_index: resumed.completed_index,
                    state: State {
                        value: resumed.current_value.clone(),
                        counter: resumed.counter,
                        steps: resumed.steps_outputs.clone(),
                    },
                },
            ))
        }
        None => {
            let prompt =
                chat::resolve_input_with_stdin_cancellable(run_args.prompt.clone(), cancel.clone())
                    .await?;
            let initial_input = match prompt {
                Some(prompt) => {
                    workflow::inputs::resolve_initial(
                        wf,
                        workflow::inputs::InitialInput::Text(prompt),
                        cancel.clone(),
                    )
                    .await?
                }
                None if wf.declares_inputs() => serde_json::Value::Null,
                None => bail!(
                    "a PROMPT is required; provide one or pipe input via stdin (a workflow that \
                     declares 'inputs:' may be run without one)"
                ),
            };
            let inputs = resolve_cli_inputs(wf, &run_args.input)?;
            Ok((
                initial_input.clone(),
                inputs,
                Progress {
                    completed_index: 0,
                    state: State {
                        value: initial_input,
                        counter: 0,
                        steps: workflow::StepOutputs::new(),
                    },
                },
            ))
        }
    }
}

pub(super) async fn run_workflow(
    run_args: RunArgs,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let file_config = super::load_config(&config_source, &cancel).await?;
    let argument = run_args.file.clone();
    let registry_config = Arc::clone(&file_config);
    let resolved_file = async_io::run_blocking(
        move |_| Ok(workflow::resolve_run_target(&argument, &registry_config)),
        cancel.clone(),
    )
    .await?;
    let workflow_path = resolved_file.display().to_string();

    let resumed = match run_args.resume.as_deref() {
        Some(run_id) => Some(checkpoint::load_cancellable(run_id, cancel.clone()).await?),
        None => None,
    };
    check_resume_compatible(resumed.as_ref(), &workflow_path)?;

    let wf = workflow::load_workflow_cancellable(&resolved_file, cancel.clone()).await?;
    announce_named_file("==>", wf.name.as_deref(), wf.description.as_deref());
    let top_level_labels = top_level_step_labels(&wf.steps);

    let (initial_input, inputs, progress) =
        resolve_run_start(&run_args, &wf, resumed.as_ref(), &top_level_labels, &cancel).await?;
    let run_id = match &resumed {
        Some(resumed) => resumed.run_id.clone(),
        None => checkpoint::generate_run_id(),
    };
    let scope =
        WorkflowScope::top_level(&wf, &resolved_file, inputs.clone(), cancel.clone()).await?;

    if run_args.dry_run {
        return workflow::dryrun::print_plan(&wf, &scope, &file_config, &initial_input);
    }

    // `--resume` implies `--checkpoint`: a run started with `--checkpoint`
    // stays checkpointed across a resume without the flag needing to be
    // repeated.
    let checkpointing = run_args.checkpoint || resumed.is_some();

    let run_cancel = cancel.child_token();
    let deadline = RunDeadline::start(wf.timeout, run_cancel.clone());

    let (services, env) =
        super::build_run_context(&file_config, cache_override, approve_tools, run_cancel);
    let env = env.with_record_replay(run_args.record.clone(), run_args.replay.clone())?;
    let checkpoint = CheckpointContext {
        run_id: &run_id,
        workflow_path: &workflow_path,
        initial_input: &initial_input,
        inputs: &inputs,
        labels: &top_level_labels,
    };
    let (progress, output) = services
        .finish(async {
            let progress = run_top_level(
                &wf.steps,
                progress,
                &scope,
                &env,
                checkpointing.then_some(&checkpoint),
                &run_args.file,
            )
            .await?;
            let output =
                finish_output(&wf, &scope, progress.state.clone(), env.root_token()).await?;
            Ok::<_, anyhow::Error>((progress, output))
        })
        .await?;
    drop(deadline);
    if checkpointing {
        checkpoint
            .save(
                &progress,
                checkpoint::RunStatus::Completed,
                env.root_token(),
            )
            .await?;
    }
    let output_text = template::to_text(&output);
    let prompt_text = template::to_text(&initial_input);

    if let Some(trace_path) = &run_args.trace_file {
        let events = env.trace.events();
        trace::write_jsonl(trace_path, &events)?;
        report::note(format_args!(
            "trace written to '{}' ({} event{})",
            trace_path.display(),
            events.len(),
            if events.len() == 1 { "" } else { "s" },
        ));
    }

    report::emit_run_output(
        &output_text,
        env.usage.total(),
        &run_args.output,
        &file_config,
    )?;
    report::finish_run(
        // A workflow can touch several models across its steps, so no
        // single `model` is recorded here — see `history::HistoryEntry::model`.
        report::RunRecord {
            kind: "workflow",
            model: None,
            prompt: &prompt_text,
            response: &output_text,
        },
        run_args.reporting.no_history,
        &file_config,
        &env.usage,
        run_args.reporting.show_usage,
    )
}

fn resolve_cli_inputs(
    wf: &workflow::WorkflowFile,
    raw: &[String],
) -> Result<serde_json::Map<String, serde_json::Value>> {
    workflow::inputs::resolve(&wf.inputs, workflow::inputs::parse_cli_inputs(raw)?)
}

/// Runs and checkpoints only top-level boundaries; nested control steps stay
/// atomic from the resume protocol's perspective. Failed steps keep their
/// prior state. A `stop` ends the loop early (the run still completes).
async fn run_top_level(
    steps: &[workflow::Step],
    mut progress: Progress,
    scope: &WorkflowScope,
    env: &RunContext,
    checkpoint: Option<&CheckpointContext<'_>>,
    requested_file: &std::path::Path,
) -> Result<Progress> {
    for (index, step) in steps.iter().enumerate().skip(progress.completed_index) {
        let saved_state = checkpoint.map(|_| progress.state.clone());
        let outcome = run_steps(
            std::slice::from_ref(step),
            progress.state,
            Frame::new(scope, env, env.root_token()),
        )
        .await;
        let Outcome { state, flow } = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                if let (Some(checkpoint), Some(saved_state)) = (checkpoint, saved_state) {
                    progress.state = saved_state;
                    // Persistence failure must not replace the execution error,
                    // especially its typed cancellation/API classification.
                    //
                    // Deliberately `cancellation::none()`, not
                    // `env.root_token()`: this branch runs precisely when
                    // the step above failed, which after a SIGINT means the
                    // root token is already cancelled. `async_io::
                    // run_blocking` bails immediately on an already-cancelled
                    // token before the write ever happens, so passing it
                    // here would silently turn every SIGINT into a
                    // checkpoint that was never written. This is the run's
                    // last write on this path — nothing downstream is
                    // waiting on it — so letting it complete uncancelled is
                    // correct.
                    match checkpoint
                        .save(
                            &progress,
                            checkpoint::RunStatus::Failed,
                            crate::cancellation::none(),
                        )
                        .await
                    {
                        Ok(()) => report::note(format_args!(
                            "run checkpointed as '{}'; resume with `lait run {} --resume {}`",
                            checkpoint.run_id,
                            requested_file.display(),
                            checkpoint.run_id,
                        )),
                        Err(save_error) => report::warn(format_args!(
                            "failed to save checkpoint for run '{}': {save_error:#}",
                            checkpoint.run_id,
                        )),
                    }
                }
                return Err(error);
            }
        };
        progress = Progress {
            completed_index: index + 1,
            state,
        };
        if let Some(checkpoint) = checkpoint {
            checkpoint
                .save(&progress, checkpoint::RunStatus::Failed, env.root_token())
                .await?;
        }
        if flow != Flow::Continue {
            break;
        }
    }
    Ok(progress)
}

#[cfg(test)]
mod tests {
    use super::RunDeadline;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn dropping_the_deadline_prevents_late_cancellation() {
        let cancel = CancellationToken::new();
        drop(RunDeadline::start(Some(0), cancel.clone()));
        tokio::task::yield_now().await;
        assert!(!cancel.is_cancelled());
    }

    #[tokio::test]
    async fn the_deadline_cancels_only_the_run_token() {
        let parent = CancellationToken::new();
        let run = parent.child_token();
        let _deadline = RunDeadline::start(Some(0), run.clone());
        tokio::time::timeout(std::time::Duration::from_secs(1), run.cancelled())
            .await
            .unwrap();
        assert!(!parent.is_cancelled());
    }
}
