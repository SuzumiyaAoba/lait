//! `lait eval`: runs an eval definition YAML (a target workflow or
//! model+prompt template, a list of cases, and `assert:` assertions —
//! optionally `llm_judge`) against a live model connection, reporting a
//! per-case success rate over `--repeat` runs. See docs/usage/ja/eval.md.
//! Unlike `lait test` (`crate::test_run`, replay-only, deterministic control
//! flow), this always calls real models — it measures output *quality*, not
//! control flow.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::Deserialize;

use crate::{
    assert::{self, Assertion, LlmJudgeContext},
    cli::{EvalArgs, EvalFormat},
    config::{self, ConfigFile, ConfigSource, ModelMap},
    engine::{
        AppServices, CapabilityOverrides, PromptTurn, RequestSettings, RunContext,
        SamplingOverrides, resolve_request_settings,
    },
    response, signal, template,
    workflow::{
        self, WorkflowScope,
        exec::{RunStepsFrame, run_steps},
    },
};

/// Upper bound on concurrently in-flight (case, repeat) runs — matches
/// `engine::tool_loop::MAX_CONCURRENT_TOOL_CALLS`'s rationale: independent
/// model round-trips, bounded so a large suite doesn't open unbounded
/// concurrent connections to the endpoint.
const EVAL_CONCURRENCY: usize = 8;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalDefinition {
    target: EvalTarget,
    cases: Vec<EvalCase>,
}

/// `target:` is either a workflow file to run, or an inline model + prompt
/// template (rendered with `{{ input }}`, like `crate::prompt::render_named`)
/// — the same two-shape pattern `schema::JsonSchemaEntry` uses.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
enum EvalTarget {
    /// Path to the target workflow file, relative to this eval definition
    /// file's own directory.
    Workflow { workflow: PathBuf },
    /// A model alias/id and a Handlebars prompt template referencing
    /// `{{ input }}`.
    Prompt { model: String, prompt: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalCase {
    input: String,
    #[serde(default)]
    assert: Vec<Assertion>,
}

/// A loaded, ready-to-run `target:` — a workflow kept alive for the whole
/// eval run (so its `nodes:`/`models:`/`json_schemas:` are only ever resolved
/// once, not per case/repeat), or a resolved model + prompt template.
enum Target {
    Workflow {
        wf: Box<workflow::WorkflowFile>,
        scope: WorkflowScope,
    },
    Prompt {
        settings: RequestSettings,
        template: String,
    },
}

impl Target {
    /// The model an `llm_judge` assertion falls back to when it doesn't set
    /// its own `model:` — the target's own model, when it has a single one.
    fn default_model(&self, file_config: &ConfigFile) -> Option<String> {
        match self {
            Target::Workflow { scope, .. } => scope
                .defaults
                .model
                .clone()
                .or_else(|| file_config.default.model.clone()),
            Target::Prompt { settings, .. } => Some(settings.resolved_model.model_id.clone()),
        }
    }

    async fn run(&self, env: &RunContext, input: &str) -> Result<String> {
        match self {
            Target::Workflow { wf, scope } => {
                let operation = Some(env.operation_token());
                let outcome = run_steps(
                    &wf.steps,
                    input.to_owned(),
                    workflow::StepOutputs::new(),
                    RunStepsFrame {
                        scope,
                        env,
                        start_counter: 0,
                        progress_prefix: "",
                        cancellation: operation.clone(),
                        placement: Default::default(),
                    },
                )
                .await?;
                Ok(outcome.output)
            }
            Target::Prompt { settings, template } => {
                let rendered = template::render(
                    template,
                    &template::parse_input(input),
                    &serde_json::Map::new(),
                    &serde_json::Map::new(),
                )?;
                let response = settings
                    .complete(
                        env,
                        &[],
                        PromptTurn::simple(None, &rendered),
                        None,
                        Some(env.operation_token()),
                    )
                    .await?;
                Ok(response::content_text(&response).to_owned())
            }
        }
    }
}

async fn load_target(
    target: &EvalTarget,
    base_dir: &Path,
    file_config: &ConfigFile,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<Target> {
    match target {
        EvalTarget::Workflow { workflow } => {
            let workflow_path = base_dir.join(workflow);
            let mut wf =
                workflow::load_workflow_cancellable(&workflow_path, cancellation.clone()).await?;
            let scope = WorkflowScope::top_level(&mut wf, &workflow_path, cancellation).await?;
            Ok(Target::Workflow {
                wf: Box::new(wf),
                scope,
            })
        }
        EvalTarget::Prompt { model, prompt } => {
            let settings = resolve_request_settings(
                model.clone(),
                SamplingOverrides::default(),
                None,
                None,
                CapabilityOverrides::default(),
                &ModelMap::default(),
                file_config,
            )?
            .with_usage_label(format!("eval target '{model}'"));
            Ok(Target::Prompt {
                settings,
                template: prompt.clone(),
            })
        }
    }
}

/// One repeat's outcome for one case: each failed assertion's reason (empty
/// when every `assert:` entry passed).
struct RunResult {
    failures: Vec<String>,
}

impl RunResult {
    fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

struct CaseOutcome {
    /// 1-based position in `cases:`, for display.
    index: usize,
    input: String,
    runs: Vec<RunResult>,
}

impl CaseOutcome {
    fn passed_count(&self) -> usize {
        self.runs.iter().filter(|run| run.passed()).count()
    }

    fn success_rate(&self) -> f64 {
        self.passed_count() as f64 / self.runs.len() as f64
    }

    fn fully_passed(&self) -> bool {
        self.passed_count() == self.runs.len()
    }
}

async fn run_case(
    target: &Target,
    env: &RunContext,
    case: &EvalCase,
    default_model: Option<&str>,
    file_config: &ConfigFile,
) -> RunResult {
    match target.run(env, &case.input).await {
        Ok(output) => {
            let judge = LlmJudgeContext {
                env,
                file_config,
                default_model,
                input: Some(case.input.as_str()),
            };
            let failures = assert::evaluate(
                &case.assert,
                Some(&judge),
                &output,
                Some(env.operation_token()),
            )
            .await;
            RunResult {
                failures: failures
                    .into_iter()
                    .map(|failure| format!("assertion {}: {}", failure.position, failure.message))
                    .collect(),
            }
        }
        Err(error) => RunResult {
            failures: vec![format!("{error:#}")],
        },
    }
}

fn print_text_report(cases: &[CaseOutcome]) {
    let mut fully_passed_count = 0usize;
    for case in cases {
        let passed = case.passed_count();
        let total = case.runs.len();
        let rate_pct = case.success_rate() * 100.0;
        if case.fully_passed() {
            fully_passed_count += 1;
            println!(
                "case {}: {passed}/{total} ({rate_pct:.0}%) PASS",
                case.index
            );
        } else {
            println!(
                "case {}: {passed}/{total} ({rate_pct:.0}%) FAIL",
                case.index
            );
            for (run_index, run) in case.runs.iter().enumerate() {
                for failure in &run.failures {
                    println!("  run {}: {failure}", run_index + 1);
                }
            }
        }
    }
    println!(
        "{fully_passed_count} of {} case(s) fully passed",
        cases.len()
    );
}

fn print_json_report(cases: &[CaseOutcome]) -> Result<()> {
    let results: Vec<serde_json::Value> = cases
        .iter()
        .map(|case| {
            serde_json::json!({
                "case": case.index,
                "input": case.input,
                "passed": case.passed_count(),
                "total": case.runs.len(),
                "success_rate": case.success_rate(),
                "runs": case.runs.iter().map(|run| serde_json::json!({
                    "passed": run.passed(),
                    "failures": run.failures,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    println!("{}", serde_json::to_string(&results)?);
    Ok(())
}

pub(crate) async fn run(
    args: EvalArgs,
    config_source: ConfigSource,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    signal::spawn_handler(cancel.clone());
    let file_config =
        Arc::new(config::load_config_cancellable(&config_source, Some(cancel.clone())).await?);

    let contents = crate::async_io::read_to_string_cancellable(
        &args.file,
        Some(cancel.clone()),
        crate::async_io::MAX_READ_BYTES,
    )
    .await
    .with_context(|| format!("failed to read eval definition '{}'", args.file.display()))?;
    let definition: EvalDefinition = serde_yaml::from_str(&contents)
        .with_context(|| format!("failed to parse eval definition '{}'", args.file.display()))?;
    let base_dir = args.file.parent().unwrap_or_else(|| Path::new("."));

    let target = load_target(
        &definition.target,
        base_dir,
        &file_config,
        Some(cancel.clone()),
    )
    .await?;
    let default_model = target.default_model(&file_config);

    let services = Arc::new(AppServices::new(Arc::clone(&file_config)));
    let env = RunContext::new(Arc::clone(&services), cancel);
    let repeat = args.repeat.max(1);

    // Every (case, repeat) run is independent — same target, same
    // read-only env — so they're flattened into one future list and run
    // with a fixed concurrency cap instead of one at a time; a 30-case
    // suite with `--repeat 3` used to be 90 fully serial model round-trips.
    // `buffered` (not `buffer_unordered`) preserves the flattened order, so
    // `runs` below can just `take` each case's slice off the front in turn
    // without needing to track which case a given run belonged to.
    let mut run_futures = Vec::with_capacity(definition.cases.len() * repeat as usize);
    for case in &definition.cases {
        for _ in 0..repeat {
            run_futures.push(run_case(
                &target,
                &env,
                case,
                default_model.as_deref(),
                &file_config,
            ));
        }
    }
    let mut all_runs = services
        .finish(async {
            futures_util::stream::iter(run_futures)
                .buffered(EVAL_CONCURRENCY)
                .collect::<Vec<RunResult>>()
                .await
        })
        .await
        .into_iter();
    let cases: Vec<CaseOutcome> = definition
        .cases
        .iter()
        .enumerate()
        .map(|(index, case)| CaseOutcome {
            index: index + 1,
            input: case.input.clone(),
            runs: (&mut all_runs).take(repeat as usize).collect(),
        })
        .collect();

    match args.format {
        EvalFormat::Text => print_text_report(&cases),
        EvalFormat::Json => print_json_report(&cases)?,
    }

    if !cases.iter().all(CaseOutcome::fully_passed) {
        bail!("one or more eval cases did not fully pass");
    }
    Ok(())
}
