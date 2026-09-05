//! Top-level workflow orchestration, resume state, and checkpoint publication.

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};

use crate::{
    checkpoint,
    cli::RunArgs,
    config::{self, ConfigSource},
    engine::AppContext,
    report,
    workflow::{
        self, WorkflowScope,
        exec::{Flow, RunStepsFrame, StepsOutcome, announce_named_file, run_steps},
    },
};

use super::{resolve_cache_settings, resolve_input_with_stdin};

/// Runtime progress between top-level steps. Keeping the state together avoids
/// mixing a router's nested counter with the checkpoint's top-level position.
struct Progress {
    completed_index: usize,
    counter: usize,
    input: String,
    outputs: workflow::StepOutputs,
}

/// Immutable metadata shared by every snapshot in a run.
struct CheckpointContext<'a> {
    run_id: &'a str,
    workflow_path: &'a str,
    initial_prompt: &'a str,
    vars: &'a serde_json::Map<String, serde_json::Value>,
    labels: &'a [String],
}

impl CheckpointContext<'_> {
    fn save(&self, progress: &Progress, status: checkpoint::RunStatus) -> Result<()> {
        checkpoint::save(&checkpoint::Checkpoint {
            run_id: self.run_id.to_owned(),
            workflow_path: self.workflow_path.to_owned(),
            initial_prompt: self.initial_prompt.to_owned(),
            vars: self.vars.clone(),
            top_level_labels: self.labels.to_vec(),
            completed_index: progress.completed_index,
            counter: progress.counter,
            current_input: progress.input.clone(),
            steps_outputs: progress.outputs.clone(),
            status,
        })
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
                    eprintln!("lait: 'default.workflow_timeout' ({seconds}s) exceeded; cancelling the run");
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

/// Every top-level step's label, by position: this site's own label (see
/// `FlowStep::label`) if set, else `step-<position>` (1-based). Deliberately
/// *not* the same value `run_steps`' own progress-counter fallback would
/// produce for an unlabeled router site — that counter only exists once a
/// run is actually executing (it also counts nested steps), whereas this
/// only needs to name each top-level position stably, before anything has
/// run, so `checkpoint::check_resumable` can detect whether the step
/// sequence changed since a checkpoint was written.
fn top_level_step_labels(steps: &[workflow::FlowStep]) -> Vec<String> {
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| step.label_or(index + 1))
        .collect()
}

pub(super) async fn run_workflow(
    run_args: RunArgs,
    config_source: ConfigSource,
    cache_override: Option<bool>,
    approve_tools: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    crate::signal::spawn_handler(cancel.clone());
    let file_config = Arc::new(config::load_config(&config_source)?);
    let resolved_file = workflow::resolve_run_target(&run_args.file, &file_config);
    let workflow_path = resolved_file.display().to_string();

    let resumed = run_args
        .resume
        .as_deref()
        .map(checkpoint::load)
        .transpose()?;
    if let Some(resumed) = &resumed {
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
    }

    let mut wf = workflow::load_workflow(&resolved_file)?;
    announce_named_file("==>", wf.name.as_deref(), wf.description.as_deref());
    let scope = WorkflowScope::top_level(&mut wf, &resolved_file)?;
    let top_level_labels = top_level_step_labels(&wf.steps);

    let (initial_prompt, vars, progress) = match &resumed {
        Some(resumed) => {
            checkpoint::check_resumable(&top_level_labels, resumed)?;
            eprintln!(
                "==> resuming run '{}' from step {}/{}",
                resumed.run_id,
                resumed.completed_index + 1,
                top_level_labels.len(),
            );
            let vars = if run_args.var.var.is_empty() {
                resumed.vars.clone()
            } else {
                workflow::build_vars(&run_args.var.var)?
            };
            (
                resumed.initial_prompt.clone(),
                vars,
                Progress {
                    completed_index: resumed.completed_index,
                    counter: resumed.counter,
                    input: resumed.current_input.clone(),
                    outputs: resumed.steps_outputs.clone(),
                },
            )
        }
        None => {
            let prompt = resolve_input_with_stdin(run_args.prompt.clone())?.ok_or_else(|| {
                anyhow!("a PROMPT is required; provide one or pipe input via stdin")
            })?;
            let vars = workflow::build_vars(&run_args.var.var)?;
            (
                prompt.clone(),
                vars,
                Progress {
                    completed_index: 0,
                    counter: 0,
                    input: prompt,
                    outputs: workflow::StepOutputs::new(),
                },
            )
        }
    };
    let run_id = match &resumed {
        Some(resumed) => resumed.run_id.clone(),
        None => checkpoint::generate_run_id(),
    };

    if run_args.dry_run {
        return workflow::dryrun::print_plan(&wf, &scope, &file_config, &initial_prompt, &vars);
    }

    // `--resume` implies `--checkpoint`: a run started with `--checkpoint`
    // stays checkpointed across a resume without the flag needing to be
    // repeated.
    let checkpointing = run_args.checkpoint || resumed.is_some();

    let run_cancel = cancel.child_token();
    let deadline = RunDeadline::start(scope.defaults.workflow_timeout, run_cancel.clone());

    let (cache_enabled, cache_ttl) = resolve_cache_settings(cache_override, &file_config);
    let env = AppContext::new(Arc::clone(&file_config))
        .with_vars(vars.clone())
        .with_cancel(run_cancel)
        .with_cache(cache_enabled, cache_ttl)
        .with_approve_tools(approve_tools)
        .with_record_replay(run_args.record.clone(), run_args.replay.clone());
    let checkpoint = CheckpointContext {
        run_id: &run_id,
        workflow_path: &workflow_path,
        initial_prompt: &initial_prompt,
        vars: &vars,
        labels: &top_level_labels,
    };
    let progress = env
        .finish(run_top_level(
            &wf.steps,
            progress,
            &scope,
            &env,
            checkpointing.then_some(&checkpoint),
            &run_args.file,
        ))
        .await?;
    drop(deadline);
    if checkpointing {
        checkpoint.save(&progress, checkpoint::RunStatus::Completed)?;
    }
    let current_input = progress.input;

    report::emit_run_output(
        &current_input,
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
            prompt: &initial_prompt,
            response: &current_input,
        },
        run_args.reporting.no_history,
        &file_config,
        &env.usage,
        run_args.reporting.show_usage,
    )
}

/// Runs and checkpoints only top-level boundaries; nested routers stay atomic
/// from the resume protocol's perspective. Failed steps keep their prior state.
async fn run_top_level(
    steps: &[workflow::FlowStep],
    mut progress: Progress,
    scope: &WorkflowScope,
    env: &AppContext,
    checkpoint: Option<&CheckpointContext<'_>>,
    requested_file: &std::path::Path,
) -> Result<Progress> {
    for (index, step) in steps.iter().enumerate().skip(progress.completed_index) {
        let saved_state = checkpoint.map(|_| (progress.input.clone(), progress.outputs.clone()));
        let outcome = run_steps(
            std::slice::from_ref(step),
            progress.input,
            progress.outputs,
            RunStepsFrame {
                scope,
                env,
                start_counter: progress.counter,
                progress_prefix: "",
                cancellation: env.cancel.clone(),
            },
        )
        .await;
        let StepsOutcome {
            output,
            counter,
            flow,
            steps_outputs,
        } = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                if let (Some(checkpoint), Some((input, outputs))) = (checkpoint, saved_state) {
                    progress.input = input;
                    progress.outputs = outputs;
                    // Persistence failure must not replace the execution error,
                    // especially its typed cancellation/API classification.
                    match checkpoint.save(&progress, checkpoint::RunStatus::Failed) {
                        Ok(()) => eprintln!(
                            "note: run checkpointed as '{}'; resume with `lait run {} --resume {}`",
                            checkpoint.run_id,
                            requested_file.display(),
                            checkpoint.run_id,
                        ),
                        Err(save_error) => eprintln!(
                            "warning: failed to save checkpoint for run '{}': {save_error:#}",
                            checkpoint.run_id,
                        ),
                    }
                }
                return Err(error);
            }
        };
        progress = Progress {
            completed_index: index + 1,
            counter,
            input: output,
            outputs: steps_outputs,
        };
        if let Some(checkpoint) = checkpoint {
            checkpoint.save(&progress, checkpoint::RunStatus::Failed)?;
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
