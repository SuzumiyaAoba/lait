//! Workflow router execution and branch/item output aggregation.

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, TryStreamExt};

use crate::{engine::value_to_input_text, jq, template, workflow};

use super::{
    ExecutionPlacement, Flow, RouterContext, StepContextExt, StepsOutcome, StepsState,
    record_step_output, run_steps,
};

/// Dispatches a router and publishes its named output when it completes.
/// A switch records its selected branch even when it breaks or stops; a
/// stopped loop/for_each returns the body output before completing aggregation.
pub(super) async fn execute(
    router: workflow::Router<'_>,
    step: &workflow::FlowStep,
    state: StepsState,
    context: &RouterContext<'_>,
    label: &str,
) -> Result<StepsOutcome> {
    let record_on_stop = matches!(&router, workflow::Router::Switch(_));
    let mut outcome = match router {
        workflow::Router::Switch(switch) => execute_switch(switch, state, context, label).await?,
        workflow::Router::Parallel(parallel) => {
            execute_parallel(parallel, state, context, label).await?
        }
        workflow::Router::Loop(loop_def) => execute_loop(loop_def, state, context, label).await?,
        workflow::Router::ForEach(for_each) => {
            execute_for_each(for_each, state, context, label).await?
        }
    };
    if outcome.flow != Flow::Stop || record_on_stop {
        record_step_output(&mut outcome.steps_outputs, step, &outcome.output);
    }
    Ok(outcome)
}

/// Executes a switch router and returns the state produced by its selected
/// branch. Case selection short-circuits at the first truthy condition, then
/// transfers the current state into the selected branch.
async fn execute_switch<'a>(
    switch: &'a workflow::SwitchDefinition,
    state: StepsState,
    context: &RouterContext<'a>,
    label: &str,
) -> Result<StepsOutcome> {
    let StepsState {
        output: current_input,
        counter,
        steps_outputs,
    } = state;
    for (case_index, case) in switch.cases.iter().enumerate() {
        if workflow::eval_when_async(
            &case.when,
            &current_input,
            &steps_outputs,
            &context.env.vars,
            context.cancellation.clone(),
        )
        .await
        .step(label)?
        {
            let case_label = case
                .id
                .clone()
                .unwrap_or_else(|| format!("case-{}", case_index + 1));
            eprintln!(
                "{}    -> case '{case_label}' matched",
                context.progress_prefix
            );
            return run_steps(
                &case.steps,
                current_input,
                steps_outputs,
                context.frame(counter, context.progress_prefix),
            )
            .await;
        }
    }

    let Some(else_steps) = &switch.else_steps else {
        bail!("step '{label}': no case matched and no 'else' branch is defined")
    };
    eprintln!(
        "{}    -> no case matched, running 'else'",
        context.progress_prefix
    );
    run_steps(
        else_steps,
        current_input,
        steps_outputs,
        context.frame(counter, context.progress_prefix),
    )
    .await
}

/// Executes all branches of a parallel router and joins their outputs in
/// declaration order. Each branch gets an isolated copy of named step outputs;
/// only the joined value is returned to the parent state.
async fn execute_parallel<'a>(
    parallel: &'a workflow::ParallelDefinition,
    state: StepsState,
    context: &RouterContext<'a>,
    label: &str,
) -> Result<StepsOutcome> {
    let StepsState {
        output: current_input,
        counter,
        steps_outputs,
    } = state;
    eprintln!(
        "{}    -> running {} branches concurrently",
        context.progress_prefix,
        parallel.branches.len()
    );

    let branch_labels: Vec<String> = parallel
        .branches
        .iter()
        .enumerate()
        .map(|(index, branch)| branch.label(index))
        .collect();
    let branch_prefixes: Vec<String> = branch_labels
        .iter()
        .map(|branch_label| format!("{}[{branch_label}] ", context.progress_prefix))
        .collect();
    let mut branch_futures = Vec::with_capacity(parallel.branches.len());
    for (branch, branch_prefix) in parallel.branches.iter().zip(&branch_prefixes) {
        branch_futures.push(run_steps(
            &branch.steps,
            current_input.clone(),
            steps_outputs.clone(),
            context
                .frame(0, branch_prefix)
                .with_placement(context.placement.parallel()),
        ));
    }
    let branch_results = futures_util::future::try_join_all(branch_futures).await?;

    let mut joined = serde_json::Map::new();
    for (branch_label, branch_result) in branch_labels.into_iter().zip(branch_results) {
        joined.insert(branch_label, template::parse_input(&branch_result.output));
    }
    let joined_json = serde_json::to_string(&serde_json::Value::Object(joined))
        .context("failed to serialize joined 'parallel' branch outputs")?;

    eprintln!("{}    -> branches joined", context.progress_prefix);
    let output = match &parallel.join {
        Some(filter) => jq::apply_cancellable_async(
            filter,
            &joined_json,
            &steps_outputs,
            &context.env.vars,
            context.cancellation.clone(),
        )
        .await
        .step(label)?,
        None => joined_json,
    };

    Ok(StepsState {
        output,
        counter,
        steps_outputs,
    }
    .into_outcome(Flow::Continue))
}

/// Executes a loop router, threading the body output and named outputs across
/// iterations until its condition or an explicit `break` succeeds.
async fn execute_loop<'a>(
    loop_def: &'a workflow::LoopDefinition,
    state: StepsState,
    context: &RouterContext<'a>,
    label: &str,
) -> Result<StepsOutcome> {
    let StepsState {
        output: current_input,
        counter,
        mut steps_outputs,
    } = state;
    let max_iterations = loop_def.max_iterations.get();

    let mut iteration_input = current_input;
    let mut loop_counter = counter;
    let mut iterations_run = 0usize;
    let satisfied = loop {
        if let workflow::LoopCondition::While(while_cond) = &loop_def.condition
            && !workflow::eval_when_async(
                while_cond,
                &iteration_input,
                &steps_outputs,
                &context.env.vars,
                context.cancellation.clone(),
            )
            .await
            .step(label)?
        {
            break true;
        }
        if iterations_run >= max_iterations {
            break false;
        }
        iterations_run += 1;
        eprintln!(
            "{}    -> iteration {iterations_run}/{max_iterations}",
            context.progress_prefix
        );
        // `iteration_input`/`steps_outputs` are unconditionally overwritten
        // by `result`/`new_steps_outputs` right below on every path out of
        // this iteration (including the `Flow::Stop` early return, which
        // reads the post-assignment values), so both can be moved out
        // instead of cloned — the same reasoning `execute_sequential_items`
        // already applies to its own `steps_outputs.clone()`. On the `?`
        // failing this iteration, both locals are simply dropped: a `loop`
        // is a router step (see `run_steps`'s `step.router()` branch), so
        // its failure propagates straight past `run_steps`'s `on_error`
        // handling (which only wraps `step.call()`) — nothing upstream
        // reads these post-take values on that path.
        let outcome = run_steps(
            &loop_def.steps,
            std::mem::take(&mut iteration_input),
            std::mem::take(&mut steps_outputs),
            context.frame(loop_counter, context.progress_prefix),
        )
        .await?;
        let StepsOutcome {
            output: result,
            counter: new_counter,
            flow,
            steps_outputs: new_steps_outputs,
        } = outcome;
        iteration_input = result;
        loop_counter = new_counter;
        steps_outputs = new_steps_outputs;
        match flow {
            Flow::Continue => {}
            Flow::Break => break true,
            Flow::Stop => {
                return Ok(StepsState {
                    output: iteration_input,
                    counter: loop_counter,
                    steps_outputs,
                }
                .into_outcome(Flow::Stop));
            }
        }
        if let workflow::LoopCondition::Until(until_cond) = &loop_def.condition
            && workflow::eval_when_async(
                until_cond,
                &iteration_input,
                &steps_outputs,
                &context.env.vars,
                context.cancellation.clone(),
            )
            .await
            .step(label)?
        {
            break true;
        }
    };

    if !satisfied {
        let condition = loop_def.condition.keyword();
        bail!(
            "step '{label}': 'loop' reached max_iterations ({max_iterations}) without satisfying '{condition}'"
        );
    }

    Ok(StepsState {
        output: iteration_input,
        counter: loop_counter,
        steps_outputs,
    }
    .into_outcome(Flow::Continue))
}

/// Executes a for_each router. Sequential iteration preserves the parent
/// counter and step namespace; concurrent iteration isolates both per item and
/// joins results in item order.
async fn execute_for_each<'a>(
    for_each: &'a workflow::ForEachDefinition,
    state: StepsState,
    context: &RouterContext<'a>,
    label: &str,
) -> Result<StepsOutcome> {
    let StepsState {
        output: current_input,
        counter,
        mut steps_outputs,
    } = state;
    let items_json = jq::apply_one_cancellable_async(
        &for_each.items,
        &current_input,
        &steps_outputs,
        &context.env.vars,
        context.cancellation.clone(),
    )
    .await
    .step(label)?;
    let items_value: serde_json::Value = serde_json::from_str(&items_json).with_context(|| {
        format!("step '{label}': failed to parse 'for_each.items' output as JSON")
    })?;
    // Moves the array out of `items_value` rather than `.as_array().cloned()`
    // — nothing else needs `items_value` afterward, so cloning every element
    // just to hand back an owned `Vec` would be wasted work.
    let items = match items_value {
        serde_json::Value::Array(items) => items,
        _ => bail!("step '{label}': 'for_each.items' must produce a JSON array"),
    };

    let max_concurrency = for_each.max_concurrency.unwrap_or(1);
    let item_outcome = if max_concurrency <= 1 {
        eprintln!(
            "{}    -> iterating over {} item(s)",
            context.progress_prefix,
            items.len()
        );
        execute_sequential_items(for_each, items, counter, steps_outputs, context).await?
    } else {
        eprintln!(
            "{}    -> iterating over {} item(s), up to {max_concurrency} concurrently",
            context.progress_prefix,
            items.len()
        );
        execute_concurrent_items(
            for_each,
            items,
            counter,
            steps_outputs,
            max_concurrency,
            context,
        )
        .await?
    };

    let ForEachItemsOutcome {
        results,
        counter,
        steps_outputs: updated_steps_outputs,
        stop_output,
    } = item_outcome;
    steps_outputs = updated_steps_outputs;
    if let Some(output) = stop_output {
        return Ok(StepsState {
            output,
            counter,
            steps_outputs,
        }
        .into_outcome(Flow::Stop));
    }

    let results_json = serde_json::to_string(&serde_json::Value::Array(results))
        .context("failed to serialize 'for_each' results")?;
    let output = match &for_each.join {
        Some(filter) => jq::apply_cancellable_async(
            filter,
            &results_json,
            &steps_outputs,
            &context.env.vars,
            context.cancellation.clone(),
        )
        .await
        .step(label)?,
        None => results_json,
    };

    Ok(StepsState {
        output,
        counter,
        steps_outputs,
    }
    .into_outcome(Flow::Continue))
}

struct ForEachItemsOutcome {
    results: Vec<serde_json::Value>,
    counter: usize,
    steps_outputs: workflow::StepOutputs,
    stop_output: Option<String>,
}

async fn execute_sequential_items<'a>(
    for_each: &'a workflow::ForEachDefinition,
    items: Vec<serde_json::Value>,
    counter: usize,
    mut steps_outputs: workflow::StepOutputs,
    context: &RouterContext<'a>,
) -> Result<ForEachItemsOutcome> {
    let mut results = Vec::with_capacity(items.len());
    let mut item_counter = counter;
    for (item_index, item) in items.iter().enumerate() {
        eprintln!(
            "{}    -> item {}/{}",
            context.progress_prefix,
            item_index + 1,
            items.len()
        );
        let item_input = value_to_input_text(item, "failed to serialize a 'for_each' item")?;
        // `steps_outputs` is unconditionally overwritten by
        // `new_steps_outputs` right below, so the value handed to
        // `run_steps` here can be moved out instead of cloned — unlike
        // `execute_concurrent_items`, where every item future needs its own
        // independent copy since they run at once.
        let outcome = run_steps(
            &for_each.steps,
            item_input,
            std::mem::take(&mut steps_outputs),
            context.frame(item_counter, context.progress_prefix),
        )
        .await?;
        let StepsOutcome {
            output,
            counter: new_counter,
            flow,
            steps_outputs: new_steps_outputs,
        } = outcome;
        item_counter = new_counter;
        steps_outputs = new_steps_outputs;
        if flow == Flow::Stop {
            return Ok(ForEachItemsOutcome {
                results,
                counter: item_counter,
                steps_outputs,
                stop_output: Some(output),
            });
        }
        results.push(template::parse_input(&output));
        if flow == Flow::Break {
            break;
        }
    }
    Ok(ForEachItemsOutcome {
        results,
        counter: item_counter,
        steps_outputs,
        stop_output: None,
    })
}

/// One `for_each` concurrent item's body: serializes `item` and runs
/// `for_each.steps` against it. Split out of `execute_concurrent_items` as
/// its own `async fn` because building this future inline inside
/// `Iterator::map`'s closure fails to type-check when the closure's argument
/// is a *borrowed* item (`&serde_json::Value` from `items.iter()`) — rustc's
/// closure-return-type inference cannot unify the borrow's lifetime with the
/// one `run_steps` needs ("implementation of `FnOnce` is not general
/// enough"), a known limitation around closures whose return type captures
/// their argument's lifetime. Taking `item`/`item_prefix` by value (the
/// caller uses `items.into_iter()`, not `.iter()`) removes the borrowed
/// lifetime from the closure's argument entirely, which sidesteps it.
async fn run_for_each_item<'a>(
    for_each: &'a workflow::ForEachDefinition,
    item: serde_json::Value,
    item_prefix: String,
    steps_outputs: workflow::StepOutputs,
    context: &RouterContext<'a>,
) -> Result<StepsOutcome> {
    let item_input = value_to_input_text(&item, "failed to serialize a 'for_each' item")?;
    run_steps(
        &for_each.steps,
        item_input,
        steps_outputs,
        context
            .frame(0, &item_prefix)
            .with_placement(ExecutionPlacement::ConcurrentItems),
    )
    .await
}

/// Builds and runs one [`run_for_each_item`] future per item, lazily: the
/// futures aren't collected into a `Vec` up front (that used to mean an
/// item's input serialization, its progress-prefix `String`, *and* a full
/// deep clone of `steps_outputs` all happened for every item before the
/// first one could even start polling — a `for_each` with
/// `max_concurrency: 4` over 500 items paid the clone 500 times, not 4).
/// `items.iter().enumerate().map` stays a plain `Iterator`; `stream::iter` +
/// `buffered(max_concurrency)` pulls from it only as concurrency slots free
/// up, so at most `max_concurrency` items' worth of input/prefix/
/// `steps_outputs` clone are ever live at once — same pattern as
/// `test_run.rs`'s `run_futures` and `engine/tool_loop.rs`'s
/// `execute_bounded`. `steps_outputs` itself is captured by the closure by
/// reference and cloned once per item as that closure runs, so the original
/// binding survives to be returned in `ForEachItemsOutcome` below untouched.
async fn execute_concurrent_items<'a>(
    for_each: &'a workflow::ForEachDefinition,
    items: Vec<serde_json::Value>,
    counter: usize,
    steps_outputs: workflow::StepOutputs,
    max_concurrency: usize,
    context: &RouterContext<'a>,
) -> Result<ForEachItemsOutcome> {
    let item_futures = items.into_iter().enumerate().map(|(index, item)| {
        let item_prefix = format!("{}[item-{}] ", context.progress_prefix, index + 1);
        run_for_each_item(for_each, item, item_prefix, steps_outputs.clone(), context)
    });
    let item_results: Vec<StepsOutcome> = futures_util::stream::iter(item_futures)
        .buffered(max_concurrency)
        .try_collect()
        .await?;
    let results = item_results
        .into_iter()
        .map(|outcome| template::parse_input(&outcome.output))
        .collect();
    Ok(ForEachItemsOutcome {
        results,
        counter,
        steps_outputs,
        stop_output: None,
    })
}
