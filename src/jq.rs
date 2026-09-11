//! jq filter evaluation for `when:`/`jq:`/`for_each.items`/`join`/assert
//! conditions across the workflow engine — the sync and cancellable-async
//! entry points (`apply*`/`apply*_cancellable*`) both funnel into
//! `run_filter_with`, which compiles through the process-wide
//! [`FILTER_CACHE`] rather than reparsing `filter_source` and the jq
//! standard-library prelude on every call. Lives at the crate root rather
//! than under `workflow/` because `lint.rs`'s `check_syntax` path validates
//! filter syntax independently of any one workflow node type, and because
//! this module itself depends on `template::parse_input` for normalizing a
//! filter's input value.

use std::{
    collections::HashMap,
    mem::size_of,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, anyhow, bail};
use jaq_core::{
    Compiler, Ctx, Vars, data,
    load::{Arena, File, Loader},
    unwrap_valr,
};
use jaq_json::{Val, read};
use serde::Deserialize;

use crate::{async_io, template};

mod limits;

use limits::{OutputWriter, render_value_into};

/// A compiled jq filter, keyed by its source text in [`FILTER_CACHE`]. The
/// lookup-table representation a filter compiles to (`jaq_core::Filter`'s
/// `Term`s and native-filter function pointers) is fully owned — no
/// `jaq_json::Val` or borrow from the compiling `Arena` survives past
/// `compiled_filter` — so it can be cached across calls and across threads;
/// see the `Send + Sync + 'static` assertion below.
type CompiledFilter = jaq_core::Filter<data::JustLut<Val>>;

const _: fn() = || {
    fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<CompiledFilter>();
};

/// Filters compiled by [`compiled_filter`], shared across every jq call in
/// the process. Every `run_filter_with`/`check_syntax` invocation parses and
/// compiles the same fixed prelude (`jaq_core`/`jaq_std`/`jaq_json`'s
/// `defs()`, ~200 lines of jq source) plus `filter_source` itself; caching
/// the compiled result means only the first call for a given filter text
/// pays that cost, which matters most for `for_each`/`loop` bodies that
/// re-evaluate the same `when:`/`jq:` filter many times. Deliberately left
/// unbounded: for a single `lait run`/`lait chat` process, a workflow's set
/// of distinct filter strings is fixed at parse time, so this cannot grow
/// without bound the way a per-request cache could. `lait lint <DIR>`
/// recursing over many workflow files is the one case where this grows
/// across a whole directory tree rather than one workflow — still bounded
/// by the number of distinct filter strings on disk, and the process exits
/// once linting finishes, so this is not a genuine leak.
static FILTER_CACHE: LazyLock<Mutex<HashMap<String, Arc<CompiledFilter>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Parses and compiles `filter_source` (defs/funs prelude plus the filter
/// itself), or returns the cached result of an earlier call with the same
/// source text. Callers must call [`validate_filter_source`] first — this
/// function does not re-check the byte/nesting limits, so a cache hit must
/// not be reachable for a filter that failed validation.
///
/// The prelude and `with_global_vars(["$steps", "$vars"])` are fixed across
/// every caller (`run_filter_with` and `check_syntax` alike), so neither
/// needs to be part of the cache key.
fn compiled_filter(filter_source: &str) -> Result<Arc<CompiledFilter>> {
    if let Some(filter) = FILTER_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(filter_source)
    {
        return Ok(Arc::clone(filter));
    }

    let program = File {
        code: filter_source,
        path: (),
    };
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    // Turbofished for the same reason `check_syntax` used to spell it out:
    // nothing downstream of `compiled_filter` builds a `Ctx` to pin `D`
    // retroactively, since the whole point is to hand back a filter whose
    // `D` is already fixed to `data::JustLut<Val>` (see `CompiledFilter`).
    let funs = jaq_core::funs::<data::JustLut<Val>>()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());

    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader
        .load(&arena, program)
        .map_err(|errors| anyhow!("failed to parse jq filter {filter_source:?}: {errors:?}"))?;
    let filter: CompiledFilter = Compiler::default()
        .with_funs(funs)
        .with_global_vars(["$steps", "$vars"])
        .compile(modules)
        .map_err(|errors| anyhow!("failed to compile jq filter {filter_source:?}: {errors:?}"))?;

    let filter = Arc::new(filter);
    FILTER_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(filter_source.to_owned(), Arc::clone(&filter));
    Ok(filter)
}

/// jq is intentionally run in a bounded worker rather than on Tokio's
/// executor. These limits keep a filter that emits an unbounded stream from
/// growing the final rendered output without limit. Values are rendered as
/// they are yielded instead of being collected into a `Vec<Val>` first: the
/// latter makes a stream-producing filter an easy memory exhaustion vector.
/// A filter that needs more output should be split into smaller workflow
/// steps. The worker still observes workflow cancellation between yielded
/// values; the outer async wrapper bounds cleanup if jaq is inside one very
/// expensive value-producing operation.
const MAX_FILTER_SOURCE_BYTES: usize = 64 * 1024;
const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_OUTPUT_VALUES: usize = 100_000;
const MAX_RENDERED_BYTES: usize = 16 * 1024 * 1024;
/// Upper bound for the approximate heap occupied by one yielded value. The
/// rendered-size limit alone is insufficient: a value such as
/// `[range(0; 10000000)]` can have a relatively compact representation while
/// keeping millions of `Val`s alive before it is rendered.
const MAX_VALUE_STRUCTURE_BYTES: usize = 64 * 1024 * 1024;
/// Keep the recursive JSON writer away from stack-overflow territory. The
/// structure walk below is iterative, so this check also applies to values
/// produced by jq rather than just values parsed from input.
const MAX_VALUE_DEPTH: usize = 1024;

/// Named step outputs recorded by `id` (see `workflow::StepOutputs`), exposed
/// to jq filters as the `$steps` global variable (e.g. `$steps.extract.city`).
/// The same type also holds `--var KEY=VALUE` overrides exposed as `$vars`
/// (e.g. `$vars.lang`) — both are flat JSON objects keyed by name.
pub(crate) type Steps = serde_json::Map<String, serde_json::Value>;

/// Runs a jq filter while allowing a caller that owns the evaluation worker
/// to request a cooperative stop.  jaq evaluates filters lazily, so checking
/// between yielded values lets large/infinite generators stop promptly after
/// a workflow timeout without leaving a detached thread running to completion.
/// The check is also made around parsing/compilation and while rendering the
/// collected values so cancellation cannot accidentally turn into success.
fn apply_cancellable(
    filter_source: &str,
    input_json: &str,
    steps: &Steps,
    vars: &Steps,
    cancelled: &AtomicBool,
) -> Result<String> {
    check_cancelled(cancelled)?;
    let mut output = OutputWriter::new(Some(cancelled));
    run_filter_with(
        filter_source,
        input_json,
        steps,
        vars,
        Some(cancelled),
        |value| output.render(filter_source, &value),
    )?;
    output.finish(filter_source)
}

/// Shared scaffolding for the three `*_cancellable_async` entry points below:
/// owns the input/steps/vars so the operation can run on a dedicated blocking
/// worker, normalizes the input once inside that worker (so parsing a large
/// plain-text input cannot monopolize a Tokio executor thread before the
/// worker gets a chance to observe cancellation), then delegates to the
/// synchronous, already-cancellable variant.
async fn run_cancellable_async<T, F>(
    filter_source: &str,
    input: &str,
    steps: &Steps,
    vars: &Steps,
    cancellation: Option<tokio_util::sync::CancellationToken>,
    op: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&str, &str, &Steps, &Steps, &AtomicBool) -> Result<T> + Send + 'static,
{
    let filter_source = filter_source.to_owned();
    let input = input.to_owned();
    let steps = steps.clone();
    let vars = vars.clone();
    async_io::run_blocking(
        move |cancelled| {
            let input_json = normalize_input(&input)?;
            op(&filter_source, &input_json, &steps, &vars, cancelled)
        },
        cancellation,
    )
    .await
}

/// Runs a cancellable jq transform through the same bounded blocking-worker
/// pool as filesystem operations. Keeping worker admission here means every
/// node jq call shares one global thread limit instead of creating an
/// unbounded detached OS thread per timeout.
pub(crate) async fn apply_cancellable_async(
    filter_source: &str,
    input: &str,
    steps: &Steps,
    vars: &Steps,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<String> {
    run_cancellable_async(
        filter_source,
        input,
        steps,
        vars,
        cancellation,
        apply_cancellable,
    )
    .await
}

/// Runs a jq filter as a boolean condition on a bounded blocking worker.
pub(crate) async fn apply_bool_cancellable_async(
    filter_source: &str,
    input: &str,
    steps: &Steps,
    vars: &Steps,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<bool> {
    run_cancellable_async(
        filter_source,
        input,
        steps,
        vars,
        cancellation,
        apply_bool_cancellable,
    )
    .await
}

/// Runs a jq filter that must produce exactly one value on a bounded blocking
/// worker. This is the execution path used by `for_each.items`; it retains
/// JSON quoting for string results, unlike `apply`'s jq-style raw rendering.
pub(crate) async fn apply_one_cancellable_async(
    filter_source: &str,
    input: &str,
    steps: &Steps,
    vars: &Steps,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<String> {
    run_cancellable_async(
        filter_source,
        input,
        steps,
        vars,
        cancellation,
        apply_one_cancellable,
    )
    .await
}

/// Runs a jq filter as a boolean condition (used by workflow `when:` guards).
/// The filter must produce exactly one output value; that value is falsy iff
/// it is JSON `false` or `null` (jq's own truthiness rules), truthy otherwise.
/// Kept `pub(crate)` (not just test-only, despite the `#[cfg(test)]` at its
/// only call site) because `workflow::eval_when` — itself `#[cfg(test)]`,
/// retained for the pure workflow unit tests — calls this directly rather
/// than `apply_bool_cancellable_async`; removing it would also break
/// `src/workflow/tests.rs`'s dependency on that synchronous chain.
#[cfg(test)]
pub(crate) fn apply_bool(
    filter_source: &str,
    input_json: &str,
    steps: &Steps,
    vars: &Steps,
) -> Result<bool> {
    apply_bool_inner(filter_source, input_json, steps, vars, None)
}

/// Parses and compiles `filter_source` without running it against any input,
/// to check its syntax statically (used by the workflow/agent linter, which
/// has no `$steps`/input value at hand yet). Goes through the same
/// [`compiled_filter`] cache `run_filter_with` uses, so a filter the linter
/// already checked (or that a prior workflow run already compiled) doesn't
/// pay the parse/compile cost twice. A filter that only references `$steps`
/// still compiles here, since `with_global_vars` is declared the same way
/// `run_filter_with` does.
pub(crate) fn check_syntax(filter_source: &str) -> Result<()> {
    validate_filter_source(filter_source)?;
    compiled_filter(filter_source)?;
    Ok(())
}

fn apply_bool_cancellable(
    filter_source: &str,
    input_json: &str,
    steps: &Steps,
    vars: &Steps,
    cancelled: &AtomicBool,
) -> Result<bool> {
    apply_bool_inner(filter_source, input_json, steps, vars, Some(cancelled))
}

fn apply_one_cancellable(
    filter_source: &str,
    input_json: &str,
    steps: &Steps,
    vars: &Steps,
    cancelled: &AtomicBool,
) -> Result<String> {
    apply_one_inner(filter_source, input_json, steps, vars, Some(cancelled))
}

fn apply_bool_inner(
    filter_source: &str,
    input_json: &str,
    steps: &Steps,
    vars: &Steps,
    cancelled: Option<&AtomicBool>,
) -> Result<bool> {
    // Conditions do not return their value to the caller, but they still
    // must not be able to materialize an arbitrarily large result, so the
    // shared helper below still renders into a bounded scratch buffer before
    // this closure derives the boolean.
    run_single_value(
        filter_source,
        input_json,
        steps,
        vars,
        cancelled,
        "condition",
        |value, _| Ok(!matches!(value, Val::Null | Val::Bool(false))),
    )
}

fn apply_one_inner(
    filter_source: &str,
    input_json: &str,
    steps: &Steps,
    vars: &Steps,
    cancelled: Option<&AtomicBool>,
) -> Result<String> {
    run_single_value(
        filter_source,
        input_json,
        steps,
        vars,
        cancelled,
        "filter",
        |_, rendered| String::from_utf8(rendered).context("jq rendered output was not valid UTF-8"),
    )
}

/// Shared scaffolding for a jq evaluation that must produce exactly one
/// value: tracks the output count, rejects a second value, and renders the
/// (sole) value into a bounded scratch buffer before handing it to `extract`
/// — importantly, that render happens before the filter is asked for its
/// next value, so a second, oversized value cannot slip through uncounted.
/// `label` distinguishes the two callers' error text ("condition" for
/// `when:` guards, "filter" for `for_each.items`).
fn run_single_value<T>(
    filter_source: &str,
    input_json: &str,
    steps: &Steps,
    vars: &Steps,
    cancelled: Option<&AtomicBool>,
    label: &str,
    mut extract: impl FnMut(Val, Vec<u8>) -> Result<T>,
) -> Result<T> {
    let mut result = None;
    let mut count = 0usize;
    run_filter_with(filter_source, input_json, steps, vars, cancelled, |value| {
        count += 1;
        if count > 1 {
            bail!(
                "jq {label} {filter_source:?} produced {count} outputs; expected exactly one value"
            );
        }
        let mut rendered = Vec::new();
        render_value_into(
            &value,
            false,
            &mut rendered,
            0,
            MAX_RENDERED_BYTES,
            MAX_RENDERED_BYTES,
            cancelled,
        )
        .with_context(|| format!("jq {label} {filter_source:?}"))?;
        result = Some(extract(value, rendered)?);
        Ok(())
    })?;
    result.ok_or_else(|| {
        anyhow!("jq {label} {filter_source:?} produced no output; expected exactly one value")
    })
}

fn run_filter_with<F>(
    filter_source: &str,
    input_json: &str,
    steps: &Steps,
    vars: &Steps,
    cancelled: Option<&AtomicBool>,
    mut on_value: F,
) -> Result<()>
where
    F: FnMut(Val) -> Result<()>,
{
    validate_filter_source(filter_source)?;
    if input_json.len() > MAX_INPUT_BYTES {
        bail!(
            "jq input exceeds the configured limit of {} bytes",
            MAX_INPUT_BYTES
        );
    }
    check_cancelled_opt(cancelled)?;
    let input = read::parse_single(input_json.as_bytes())
        .map_err(|error| anyhow!("failed to parse jq input as JSON: {error}"))?;
    validate_value_structure(&input)
        .context("jq input structure exceeds the configured memory limit")?;
    check_cancelled_opt(cancelled)?;
    let steps_val = parse_global_var(steps, "$steps")?;
    check_cancelled_opt(cancelled)?;
    let vars_val = parse_global_var(vars, "$vars")?;
    check_cancelled_opt(cancelled)?;

    let filter = compiled_filter(filter_source)?;
    check_cancelled_opt(cancelled)?;

    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([steps_val, vars_val]));
    for (output_count, result) in filter.id.run((ctx, input)).map(unwrap_valr).enumerate() {
        check_cancelled_opt(cancelled)?;
        if output_count >= MAX_OUTPUT_VALUES {
            bail!(
                "jq filter {filter_source:?} produced more than the configured limit of {} outputs",
                MAX_OUTPUT_VALUES
            );
        }
        let value =
            result.map_err(|error| anyhow!("jq filter {filter_source:?} failed: {error}"))?;
        // `on_value` is invoked before jaq is asked to produce its next value.
        // In particular, apply_bool/apply_one reject the second value here,
        // instead of collecting the rest of an otherwise unbounded stream.
        on_value(value)?;
    }
    check_cancelled_opt(cancelled)?;
    Ok(())
}

/// Converts a `Steps`-shaped global (`$steps` or `$vars`) directly into a jaq
/// `Val`, bounding its resulting structure the same way the jq input itself
/// is bounded. `label` (`"$steps"`/`"$vars"`) names the global in the error
/// text.
///
/// This used to go through `serde_json::to_string` followed by
/// `read::parse_single` — a full JSON-text serialize-then-reparse round trip
/// paid on every single jq call (every `when:` guard, every `jq:` node,
/// every `for_each` item), where `value` is `steps_outputs`: the accumulated
/// output of every named step so far, which for LLM workflows can be
/// KB-to-MB sized and only grows as a run progresses. `Val` implements
/// `serde::Deserialize` (the `jaq-json` "serde" feature enables this), and
/// `serde_json::Map<String, Value>` implements `serde::Deserializer`
/// directly over its own borrowed tree — so this walks `value` once, in
/// memory, with no intermediate JSON text at all. The old `json.len() >
/// MAX_INPUT_BYTES` pre-check existed to avoid parsing an oversized text
/// blob; with no text produced there is nothing to pre-check, so
/// `validate_value_structure` below — which bounds the resulting `Val`
/// tree's depth/heap estimate directly — is now this function's only limit,
/// same as it already is for the jq input value in `run_filter_with`.
fn parse_global_var(value: &Steps, label: &str) -> Result<Val> {
    let parsed = Val::deserialize(value)
        .map_err(|error| anyhow!("failed to convert {label} data to a jq value: {error}"))?;
    validate_value_structure(&parsed)
        .with_context(|| format!("jq '{label}' structure exceeds the configured memory limit"))?;
    Ok(parsed)
}

fn validate_filter_source(filter_source: &str) -> Result<()> {
    if filter_source.len() > MAX_FILTER_SOURCE_BYTES {
        bail!(
            "jq filter exceeds the configured limit of {} bytes",
            MAX_FILTER_SOURCE_BYTES
        );
    }

    // jaq's parser/compiler recursively walks nested array/object/grouping
    // expressions.  A syntactically valid filter with a few thousand nested
    // delimiters can therefore overflow the worker thread's stack before the
    // resulting `Val` reaches `validate_value_structure`.  Bound the source
    // nesting first so malformed or adversarial filters fail as a normal
    // validation error instead of aborting the whole process.  Delimiters in
    // strings and comments are data, not expression nesting.
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut in_comment = false;
    for character in filter_source.chars() {
        if in_comment {
            if character == '\n' {
                in_comment = false;
            }
            continue;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '#' => in_comment = true,
            '[' | '{' | '(' => {
                depth = depth.checked_add(1).ok_or_else(structure_limit_error)?;
                if depth > MAX_VALUE_DEPTH {
                    bail!(
                        "jq filter exceeds the configured nesting limit of {}",
                        MAX_VALUE_DEPTH
                    );
                }
            }
            ']' | '}' | ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

/// Converts workflow input to the JSON representation jq consumes. Valid JSON
/// is preserved verbatim; plain text becomes a JSON string. This is kept in
/// the blocking worker's closure so even the serialization of a very large
/// plain-text input cannot block a Tokio executor thread.
///
/// The validity probe below only needs a yes/no answer, but deserializing
/// into `serde_json::Value` builds a complete owned tree just to throw it
/// away — for a large `input` (the previous step's output, potentially a
/// sizeable model response), that is a full parse and allocation wasted on
/// every call whose input happens to be valid JSON. `serde::de::IgnoredAny`
/// still walks the whole input (so a truncated/invalid tail is still
/// rejected exactly as before) but discards each value as it goes instead
/// of materializing it, so this is `run_filter_with`'s own `read::
/// parse_single` call doing the one real parse instead of two.
fn normalize_input(input: &str) -> Result<String> {
    if serde_json::from_str::<serde::de::IgnoredAny>(input).is_ok() {
        return Ok(input.to_owned());
    }
    serde_json::to_string(&template::parse_input(input))
        .context("failed to serialize plain-text jq input")
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    check_cancelled_opt(Some(cancelled))
}

fn check_cancelled_opt(cancelled: Option<&AtomicBool>) -> Result<()> {
    if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Acquire)) {
        bail!(crate::error::Interrupted::cancelled(
            "jq evaluation was cancelled"
        ));
    }
    Ok(())
}

/// Checks an already-materialized jaq value before handing it to the JSON
/// writer. The walk is iterative and keeps only one frame per nesting level,
/// so a very wide array cannot make the guard itself allocate a second list of
/// all children. The estimate intentionally over-counts shared `Rc` storage;
/// rejecting a value that is near the limit is preferable to allowing a
/// process-wide memory spike.
fn validate_value_structure(value: &Val) -> Result<()> {
    enum Frame<'a> {
        Value(&'a Val, usize),
        Array(&'a [Val], usize, usize),
        Object(&'a jaq_json::Map<Val, Val>, usize, usize),
    }

    let mut frames = vec![Frame::Value(value, 1)];
    let mut estimated = 0usize;
    while let Some(frame) = frames.pop() {
        match frame {
            Frame::Value(value, depth) => {
                if depth > MAX_VALUE_DEPTH {
                    bail!(
                        "jq output structure exceeds the configured nesting limit of {}",
                        MAX_VALUE_DEPTH
                    );
                }
                charge_structure(&mut estimated, size_of::<Val>())?;
                match value {
                    Val::TStr(bytes) | Val::BStr(bytes) => {
                        charge_structure(&mut estimated, bytes.len())?;
                    }
                    Val::Arr(values) => {
                        let values = values.as_ref();
                        charge_structure(&mut estimated, size_of::<Vec<Val>>())?;
                        charge_structure(
                            &mut estimated,
                            values
                                .len()
                                .checked_mul(size_of::<Val>())
                                .ok_or_else(structure_limit_error)?,
                        )?;
                        frames.push(Frame::Array(values.as_slice(), 0, depth));
                    }
                    Val::Obj(map) => {
                        let map = map.as_ref();
                        charge_structure(&mut estimated, size_of::<jaq_json::Map<Val, Val>>())?;
                        let entry_size = size_of::<Val>()
                            .checked_mul(2)
                            .and_then(|size| size.checked_add(size_of::<usize>() * 2))
                            .ok_or_else(structure_limit_error)?;
                        charge_structure(
                            &mut estimated,
                            map.len()
                                .checked_mul(entry_size)
                                .ok_or_else(structure_limit_error)?,
                        )?;
                        frames.push(Frame::Object(map, 0, depth));
                    }
                    Val::Num(number) => match number {
                        jaq_json::Num::BigInt(number) => {
                            let bytes = (number.bits() as usize)
                                .checked_add(7)
                                .and_then(|bits| bits.checked_div(8))
                                .ok_or_else(structure_limit_error)?;
                            charge_structure(&mut estimated, bytes)?;
                        }
                        jaq_json::Num::Dec(number) => {
                            charge_structure(&mut estimated, number.len())?;
                        }
                        jaq_json::Num::Int(_) | jaq_json::Num::Float(_) => {}
                    },
                    Val::Null | Val::Bool(_) => {}
                }
            }
            Frame::Array(values, index, depth) if index < values.len() => {
                frames.push(Frame::Array(values, index + 1, depth));
                frames.push(Frame::Value(&values[index], depth + 1));
            }
            Frame::Array(_, _, _) => {}
            Frame::Object(map, index, depth) => {
                if let Some((key, value)) = map.get_index(index) {
                    frames.push(Frame::Object(map, index + 1, depth));
                    frames.push(Frame::Value(value, depth + 1));
                    frames.push(Frame::Value(key, depth + 1));
                }
            }
        }
    }
    Ok(())
}

fn structure_limit_error() -> anyhow::Error {
    anyhow!("jq output structure exceeds the configured memory limit")
}

fn charge_structure(estimated: &mut usize, bytes: usize) -> Result<()> {
    let next = estimated
        .checked_add(bytes)
        .ok_or_else(structure_limit_error)?;
    if next > MAX_VALUE_STRUCTURE_BYTES {
        return Err(structure_limit_error());
    }
    *estimated = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::{Steps, apply_bool, apply_cancellable, apply_one_cancellable};

    fn no_steps() -> Steps {
        Steps::new()
    }

    fn no_vars() -> Steps {
        Steps::new()
    }

    fn steps_with(id: &str, value: serde_json::Value) -> Steps {
        let mut steps = Steps::new();
        steps.insert(id.to_owned(), value);
        steps
    }

    fn vars_with(key: &str, value: serde_json::Value) -> Steps {
        let mut vars = Steps::new();
        vars.insert(key.to_owned(), value);
        vars
    }

    #[test]
    fn extracts_a_string_field_raw() {
        assert_eq!(
            apply_cancellable(
                ".name",
                r#"{"name":"Alice"}"#,
                &no_steps(),
                &no_vars(),
                &AtomicBool::new(false)
            )
            .unwrap(),
            "Alice"
        );
    }

    #[test]
    fn extracts_a_number_field_as_json() {
        assert_eq!(
            apply_cancellable(
                ".age",
                r#"{"age":30}"#,
                &no_steps(),
                &no_vars(),
                &AtomicBool::new(false)
            )
            .unwrap(),
            "30"
        );
    }

    #[test]
    fn joins_multiple_outputs_with_newlines() {
        assert_eq!(
            apply_cancellable(
                ".[]",
                r#"["a","b","c"]"#,
                &no_steps(),
                &no_vars(),
                &AtomicBool::new(false)
            )
            .unwrap(),
            "a\nb\nc"
        );
    }

    #[test]
    fn renders_objects_and_arrays_as_compact_json() {
        assert_eq!(
            apply_cancellable(
                "{n: .name}",
                r#"{"name":"Alice"}"#,
                &no_steps(),
                &no_vars(),
                &AtomicBool::new(false)
            )
            .unwrap(),
            r#"{"n":"Alice"}"#
        );
    }

    #[test]
    fn rejects_invalid_json_input() {
        assert!(
            apply_cancellable(
                ".",
                "not json",
                &no_steps(),
                &no_vars(),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_invalid_filter_syntax() {
        assert!(
            apply_cancellable(".[", "{}", &no_steps(), &no_vars(), &AtomicBool::new(false))
                .is_err()
        );
    }

    #[test]
    fn reports_a_runtime_error_from_the_filter() {
        assert!(
            apply_cancellable(
                ".foo.bar",
                "1",
                &no_steps(),
                &no_vars(),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }

    #[test]
    fn apply_bool_treats_false_and_null_as_falsy() {
        assert!(!apply_bool(".flag", r#"{"flag":false}"#, &no_steps(), &no_vars()).unwrap());
        assert!(!apply_bool(".missing", "{}", &no_steps(), &no_vars()).unwrap());
    }

    #[test]
    fn apply_bool_treats_everything_else_as_truthy() {
        assert!(apply_bool(".flag", r#"{"flag":true}"#, &no_steps(), &no_vars()).unwrap());
        assert!(apply_bool(".n", r#"{"n":0}"#, &no_steps(), &no_vars()).unwrap());
        assert!(apply_bool(".s", r#"{"s":""}"#, &no_steps(), &no_vars()).unwrap());
    }

    #[test]
    fn apply_bool_rejects_zero_outputs() {
        assert!(apply_bool(".[]", "[]", &no_steps(), &no_vars()).is_err());
    }

    #[test]
    fn apply_bool_rejects_multiple_outputs() {
        assert!(apply_bool(".[]", "[true, false]", &no_steps(), &no_vars()).is_err());
    }

    #[test]
    fn apply_one_renders_a_string_output_as_quoted_json() {
        assert_eq!(
            apply_one_cancellable(
                ".name",
                r#"{"name":"Alice"}"#,
                &no_steps(),
                &no_vars(),
                &AtomicBool::new(false)
            )
            .unwrap(),
            r#""Alice""#
        );
    }

    #[test]
    fn apply_one_renders_an_array_output_as_compact_json() {
        assert_eq!(
            apply_one_cancellable(
                ".items",
                r#"{"items":[1,2,3]}"#,
                &no_steps(),
                &no_vars(),
                &AtomicBool::new(false)
            )
            .unwrap(),
            "[1,2,3]"
        );
    }

    #[test]
    fn apply_one_rejects_zero_outputs() {
        assert!(
            apply_one_cancellable(
                ".[]",
                "[]",
                &no_steps(),
                &no_vars(),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }

    #[test]
    fn apply_one_rejects_multiple_outputs() {
        assert!(
            apply_one_cancellable(
                ".[]",
                "[1, 2]",
                &no_steps(),
                &no_vars(),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_an_unbounded_number_of_outputs() {
        let error = apply_cancellable(
            "range(0; 100001)",
            "null",
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("configured limit"));
    }

    #[test]
    fn apply_bool_rejects_a_stream_after_the_second_value() {
        let error =
            apply_bool("range(0; 1000000000)", "null", &no_steps(), &no_vars()).unwrap_err();
        assert!(
            error.to_string().contains("produced 2 outputs"),
            "unexpected jq error: {error:#}"
        );
    }

    #[test]
    fn apply_one_rejects_a_stream_after_the_second_value() {
        let error = apply_one_cancellable(
            "range(0; 1000000000)",
            "null",
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("produced 2 outputs"),
            "unexpected jq error: {error:#}"
        );
    }

    #[test]
    fn rejects_rendered_output_larger_than_the_evaluation_limit() {
        let filter = format!("\"x\" * {}", super::MAX_RENDERED_BYTES + 1);
        let error = apply_cancellable(
            &filter,
            "null",
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("rendered output exceeds"),
            "unexpected jq error: {error:#}"
        );
    }

    #[test]
    fn apply_one_rejects_rendered_output_larger_than_the_evaluation_limit() {
        let filter = format!("\"x\" * {}", super::MAX_RENDERED_BYTES + 1);
        let error = apply_one_cancellable(
            &filter,
            "null",
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("rendered output exceeds"),
            "unexpected jq error: {error:#}"
        );
    }

    #[test]
    fn rejects_an_output_with_excessive_nesting() {
        let filter =
            (0..=super::MAX_VALUE_DEPTH).fold("null".to_owned(), |value, _| format!("[{value}]"));
        let error = apply_cancellable(
            &filter,
            "null",
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("nesting limit"),
            "unexpected jq error: {error:#}"
        );
    }

    #[test]
    fn rejects_input_larger_than_the_evaluation_limit() {
        let input = format!("\"{}\"", "x".repeat(super::MAX_INPUT_BYTES));
        let error = apply_cancellable(
            ".",
            &input,
            &no_steps(),
            &no_vars(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("input exceeds"));
    }

    #[test]
    fn apply_can_reference_a_named_step_output_via_dollar_steps() {
        let steps = steps_with("extract", serde_json::json!({"city": "Tokyo"}));
        assert_eq!(
            apply_cancellable(
                "$steps.extract.city",
                "null",
                &steps,
                &no_vars(),
                &AtomicBool::new(false)
            )
            .unwrap(),
            "Tokyo"
        );
    }

    #[test]
    fn apply_bool_can_reference_a_named_step_output_via_dollar_steps() {
        let steps = steps_with("check", serde_json::json!({"ok": true}));
        assert!(apply_bool("$steps.check.ok", "null", &steps, &no_vars()).unwrap());
    }

    #[test]
    fn dollar_steps_is_an_empty_object_when_no_step_output_is_recorded() {
        assert_eq!(
            apply_cancellable(
                "$steps",
                "null",
                &no_steps(),
                &no_vars(),
                &AtomicBool::new(false)
            )
            .unwrap(),
            "{}"
        );
    }

    #[test]
    fn apply_can_reference_a_var_via_dollar_vars() {
        let vars = vars_with("lang", serde_json::json!("英語"));
        assert_eq!(
            apply_cancellable(
                "$vars.lang",
                "null",
                &no_steps(),
                &vars,
                &AtomicBool::new(false)
            )
            .unwrap(),
            "英語"
        );
    }

    #[test]
    fn dollar_vars_is_an_empty_object_when_no_var_is_set() {
        assert_eq!(
            apply_cancellable(
                "$vars",
                "null",
                &no_steps(),
                &no_vars(),
                &AtomicBool::new(false)
            )
            .unwrap(),
            "{}"
        );
    }

    #[test]
    fn check_syntax_accepts_a_valid_filter() {
        assert!(super::check_syntax(".foo.bar").is_ok());
    }

    #[test]
    fn check_syntax_accepts_a_filter_that_references_dollar_steps() {
        assert!(super::check_syntax("$steps.extract.city").is_ok());
    }

    #[test]
    fn check_syntax_accepts_a_filter_that_references_dollar_vars() {
        assert!(super::check_syntax("$vars.lang").is_ok());
    }

    #[test]
    fn check_syntax_rejects_malformed_syntax() {
        assert!(super::check_syntax(".[").is_err());
    }

    #[test]
    fn check_syntax_does_not_require_a_value_to_run_against() {
        // Unlike `apply`/`apply_bool`, `check_syntax` never parses/evaluates
        // input, so a filter guaranteed to fail at runtime (dividing by a
        // field that isn't a number) still passes a syntax-only check.
        assert!(super::check_syntax(".foo / 0").is_ok());
    }
}
