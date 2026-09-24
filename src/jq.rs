//! jq expression evaluation for every jq-valued workflow field (`when:`,
//! `jq:`, `output:`, `for_each:`, `while:`/`until:`, `switch` cases, a
//! top-level `output:`) and `assert` conditions. Every evaluation must
//! produce exactly one value ([`eval_one_async`]/[`eval_bool_async`]), and
//! both funnel into `run_filter_with`, which compiles through the
//! process-wide [`FILTER_CACHE`] rather than reparsing `filter_source` and
//! the jq standard-library prelude on every call. Lives at the crate root
//! rather than under `workflow/` because `lint.rs`'s `check_syntax` path
//! validates filter syntax independently of any one workflow step kind.

use std::{
    mem::size_of,
    sync::{Arc, LazyLock, atomic::AtomicBool},
};

use anyhow::{Context, Result, anyhow, bail};
use jaq_core::{
    Compiler, Ctx, Vars, data,
    load::{Arena, File, Loader},
    unwrap_valr,
};
use jaq_json::{Val, read};
use serde::{Deserialize, Serialize};

use crate::{async_io, sync_cache::SyncCache};

mod limits;

use limits::render_value_into;

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

/// A compiled filter plus which of [`GLOBAL_NAMES`] its own source text ever
/// mentions — see [`compiled_filter`]'s doc comment for why a substring
/// search is a sound proxy for "does this filter read the global", and
/// [`run_filter_with`] for how the flags are used.
struct CachedFilter {
    filter: Arc<CompiledFilter>,
    uses_steps: bool,
    uses_inputs: bool,
    uses_loop: bool,
}

/// Filters compiled by [`compiled_filter`], shared across every jq call in
/// the process. Every `run_filter_with`/`check_syntax` invocation parses and
/// compiles the same fixed prelude (`jaq_core`/`jaq_std`/`jaq_json`'s
/// `defs()`, ~200 lines of jq source) plus `filter_source` itself; caching
/// the compiled result means only the first call for a given filter text
/// pays that cost, which matters most for `for_each`/`while`/`until` bodies
/// that re-evaluate the same `when:`/`jq:` filter many times. Deliberately
/// left unbounded: for a single `lait run`/`lait chat` process, a workflow's
/// set of distinct filter strings is fixed at parse time, so this cannot
/// grow without bound the way a per-request cache could. `lait lint <DIR>`
/// recursing over many workflow files is the one case where this grows
/// across a whole directory tree rather than one workflow — still bounded
/// by the number of distinct filter strings on disk, and the process exits
/// once linting finishes, so this is not a genuine leak.
static FILTER_CACHE: LazyLock<SyncCache<CachedFilter>> = LazyLock::new(SyncCache::new);

/// Parses and compiles `filter_source` (defs/funs prelude plus the filter
/// itself), or returns the cached result of an earlier call with the same
/// source text. Callers must call [`validate_filter_source`] first — this
/// function does not re-check the byte/nesting limits, so a cache hit must
/// not be reachable for a filter that failed validation. This is also what
/// lets [`validate_filter_source`] itself short-circuit on a cache hit here
/// instead of re-scanning `filter_source`: a filter only ever reaches
/// [`FILTER_CACHE`] below by first passing that scan, and a failed
/// parse/compile below is never cached, so presence in the cache is sound
/// evidence validation already passed.
///
/// The prelude and `with_global_vars(GLOBAL_NAMES)` are fixed across every
/// caller (`run_filter_with` and `check_syntax` alike), so neither needs to
/// be part of the cache key.
///
/// Also records whether `filter_source` mentions each global at all (see
/// [`CachedFilter`]), computed once here rather than on every
/// [`run_filter_with`] call. jq has no syntax for constructing a variable
/// name dynamically — a global reference is always the literal token
/// `$steps`/`$inputs`/`$loop` somewhere in the source, including inside a
/// string interpolation like `"\($steps.a)"` — so a plain substring search
/// never produces a false negative. A string *literal* that merely contains
/// the text is the only possible false positive, and it only costs the
/// global a construction that then goes unused, never a missing one.
fn compiled_filter(filter_source: &str) -> Result<Arc<CachedFilter>> {
    FILTER_CACHE.get_or_init(filter_source, |filter_source| {
        let program = File {
            code: filter_source,
            path: (),
        };
        let defs = jaq_core::defs()
            .chain(jaq_std::defs())
            .chain(jaq_json::defs());
        // Turbofished: nothing downstream of `compiled_filter` builds a `Ctx`
        // to pin `D` retroactively, since the whole point is to hand back a
        // filter whose `D` is already fixed to `data::JustLut<Val>` (see
        // `CompiledFilter`).
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
            .with_global_vars(GLOBAL_NAMES)
            .compile(modules)
            .map_err(|errors| {
                anyhow!("failed to compile jq filter {filter_source:?}: {errors:?}")
            })?;

        Ok(CachedFilter {
            filter: Arc::new(filter),
            uses_steps: filter_source.contains("$steps"),
            uses_inputs: filter_source.contains("$inputs"),
            uses_loop: filter_source.contains("$loop"),
        })
    })
}

/// jq is intentionally run in a bounded worker rather than on Tokio's
/// executor. These limits keep a filter from materializing an unbounded
/// result: every evaluation must produce exactly one value (see
/// [`eval_one`]/[`eval_bool`]), and that value is rendered into a bounded
/// buffer before jaq is asked for a second one, so a stream-producing filter
/// is rejected after its second value instead of being collected. The worker
/// observes workflow cancellation between yielded values; the outer async
/// wrapper bounds cleanup if jaq is inside one very expensive
/// value-producing operation.
const MAX_FILTER_SOURCE_BYTES: usize = 64 * 1024;
const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
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

/// A flat JSON object keyed by name: recorded step outputs (`$steps`, see
/// `workflow::StepOutputs`) or a workflow's resolved inputs (`$inputs`).
///
/// Copy-on-write over an `Arc`, not a plain `serde_json::Map`: a `for_each`
/// item/`parallel` branch/`while`/`until` iteration each need their own independent
/// view of the accumulated step outputs so far (see `workflow::exec`'s
/// `record_step_output`, this type's one write path), and every jq call
/// (`when:`/`jq:`/`output:`/`for_each:`/every loop condition) previously paid a full deep
/// clone of that accumulated map just to hand a worker thread an owned copy
/// it only ever reads (`run_cancellable_async` below). `Deref` makes reads
/// (`.get`, iteration, `RenderScope::new`'s `&serde_json::Map` parameter via
/// coercion, ...) transparent; `DerefMut` goes through `Arc::make_mut`, so a
/// `.clone()` while the `Arc` is shared (concurrent branches/items) still
/// isolates them exactly as before, but a clone with no other holder (the
/// overwhelmingly common case: sequential steps, `execute_sequential_items`,
/// the post-take rebind in `execute_loop`) is a refcount bump instead of a
/// deep copy. `#[serde(transparent)]` keeps the on-disk `Checkpoint`
/// (de)serialization byte-for-byte identical to the plain-`Map` shape used
/// before this type existed — a checkpoint written by an older build must
/// still resume-load.
///
/// Measured effect (debug build, macOS, `/usr/bin/time -l`; against a mock
/// OpenAI-compatible server, scratch workflows not committed — each mock
/// step returns a ~100KB response so `$steps` accumulates to roughly the
/// size quoted below): a `loop` of 1000 iterations against a ~1MB
/// accumulated `$steps` dropped from ~1.15s/~14.6MB peak to ~0.70s/~10MB
/// peak (with `execute_loop`'s matching `mem::take` fix, not this type
/// alone). A 500-item concurrent `for_each` (`max_concurrency: 4`) with an
/// empty `$steps` — isolating the lazy-future-generation benefit from this
/// type's own effect — dropped peak memory from ~21MB to ~6.7MB. The same
/// `for_each` against a ~1MB `$steps` with a `when:` guard on every item
/// (forcing a jq call, and so the `parse_global_var` re-conversion this
/// type does *not* address — see this module's `Val: !Send` note) improved
/// only ~4%, dominated by that unaddressed cost: this type mostly *defers*
/// a for_each/parallel item's copy to its first write rather than
/// eliminating it (see `record_step_output`'s one write site) — real, but
/// smaller than "no more clones" would suggest.
///
/// P8-4 addressed that remaining cost directly: `compiled_filter`/
/// `CachedFilter` record whether a filter's source text mentions `$steps`/
/// `$vars` at all, and `run_filter_with` skips the `parse_global_var`
/// conversion entirely when it doesn't — the common case for a `when:`
/// guard that only inspects the current item. The ~4% case above (a guard
/// that does read `$steps` on every item) still pays the full conversion,
/// since that cost is inherent to actually using the value, not an
/// artifact of how it was stored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Steps(Arc<serde_json::Map<String, serde_json::Value>>);

impl Steps {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Borrows the underlying flat JSON object as a concrete
    /// `&serde_json::Map`, for call sites (`Val::deserialize` below) whose
    /// generic trait bound needs a concrete `Deserializer` type spelled out
    /// rather than relying on `Deref`-based coercion — coercion only fires
    /// against a caller's already-concrete expected type, not against an
    /// unresolved generic parameter.
    fn as_map(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.0
    }
}

impl std::ops::Deref for Steps {
    type Target = serde_json::Map<String, serde_json::Value>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Steps {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::make_mut(&mut self.0)
    }
}

impl From<serde_json::Map<String, serde_json::Value>> for Steps {
    fn from(map: serde_json::Map<String, serde_json::Value>) -> Self {
        Self(Arc::new(map))
    }
}

/// The global variables every jq expression can reference, in the order
/// they are declared to the compiler (see [`GLOBAL_NAMES`]).
pub(crate) const GLOBAL_NAMES: [&str; 3] = ["$steps", "$inputs", "$loop"];

/// The values bound to [`GLOBAL_NAMES`] for one evaluation: `$steps` (the
/// outputs recorded by `id` so far), `$inputs` (the running workflow file's
/// resolved `inputs:`), and `$loop` (the innermost `for_each`/`while`/
/// `until` iteration's `{index, item}` object, `null` outside any loop).
#[derive(Clone, Debug, Default)]
pub(crate) struct Globals {
    pub(crate) steps: Steps,
    pub(crate) inputs: Steps,
    pub(crate) loop_context: serde_json::Value,
}

/// Evaluates a jq filter that must produce exactly one value, returning it.
/// Zero or several outputs are an error: collect a stream explicitly with
/// `[...]` instead.
#[cfg(test)]
pub(crate) fn eval_one(
    filter_source: &str,
    input: &serde_json::Value,
    globals: &Globals,
) -> Result<serde_json::Value> {
    let input_json = serialize_input(input)?;
    eval_one_inner(
        filter_source,
        &input_json,
        globals,
        &crate::cancellation::NEVER_SET,
    )
}

/// Evaluates a jq filter as a condition: it must produce exactly one value,
/// which is falsy iff it is `false` or `null` (jq's own truthiness rules).
#[cfg(test)]
pub(crate) fn eval_bool(
    filter_source: &str,
    input: &serde_json::Value,
    globals: &Globals,
) -> Result<bool> {
    let input_json = serialize_input(input)?;
    eval_bool_inner(
        filter_source,
        &input_json,
        globals,
        &crate::cancellation::NEVER_SET,
    )
}

/// [`eval_one`] on a bounded blocking worker. The input is serialized inside
/// the worker, so a very large value cannot block a Tokio executor thread
/// before cancellation gets a chance to win.
pub(crate) async fn eval_one_async(
    filter_source: &str,
    input: &serde_json::Value,
    globals: &Globals,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value> {
    run_cancellable_async(filter_source, input, globals, cancellation, eval_one_inner).await
}

/// [`eval_bool`] on a bounded blocking worker.
pub(crate) async fn eval_bool_async(
    filter_source: &str,
    input: &serde_json::Value,
    globals: &Globals,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<bool> {
    run_cancellable_async(filter_source, input, globals, cancellation, eval_bool_inner).await
}

/// Owns the filter/input/globals so the operation can run on a dedicated
/// blocking worker, serializes the input there, then delegates to the
/// synchronous, cancellable evaluation.
async fn run_cancellable_async<T, F>(
    filter_source: &str,
    input: &serde_json::Value,
    globals: &Globals,
    cancellation: tokio_util::sync::CancellationToken,
    op: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&str, &str, &Globals, &AtomicBool) -> Result<T> + Send + 'static,
{
    let filter_source = filter_source.to_owned();
    let input = input.clone();
    let globals = globals.clone();
    async_io::run_blocking(
        move |cancelled| {
            check_cancelled(cancelled)?;
            let input_json = serialize_input(&input)?;
            op(&filter_source, &input_json, &globals, cancelled)
        },
        cancellation,
    )
    .await
}

fn serialize_input(input: &serde_json::Value) -> Result<String> {
    serde_json::to_string(input).context("failed to serialize jq input")
}

/// Parses and compiles `filter_source` without running it against any input,
/// to check its syntax statically (used by the workflow/agent linter, which
/// has no input value at hand yet). Goes through the same
/// [`compiled_filter`] cache `run_filter_with` uses, so a filter the linter
/// already checked doesn't pay the parse/compile cost twice. Every name in
/// [`GLOBAL_NAMES`] is declared, the same way `run_filter_with` declares
/// them, so a filter that references `$steps`/`$inputs`/`$loop` still
/// compiles here.
pub(crate) fn check_syntax(filter_source: &str) -> Result<()> {
    validate_filter_source(filter_source)?;
    compiled_filter(filter_source)?;
    Ok(())
}

fn eval_bool_inner(
    filter_source: &str,
    input_json: &str,
    globals: &Globals,
    cancelled: &AtomicBool,
) -> Result<bool> {
    // Conditions do not return their value to the caller, but they still
    // must not be able to materialize an arbitrarily large result, so the
    // shared helper below still renders into a bounded scratch buffer before
    // this closure derives the boolean.
    run_single_value(
        filter_source,
        input_json,
        globals,
        cancelled,
        "condition",
        |value, _| Ok(!matches!(value, Val::Null | Val::Bool(false))),
    )
}

fn eval_one_inner(
    filter_source: &str,
    input_json: &str,
    globals: &Globals,
    cancelled: &AtomicBool,
) -> Result<serde_json::Value> {
    run_single_value(
        filter_source,
        input_json,
        globals,
        cancelled,
        "filter",
        |_, rendered| {
            serde_json::from_slice(&rendered).context("jq rendered output was not valid JSON")
        },
    )
}

/// Shared scaffolding for a jq evaluation that must produce exactly one
/// value: tracks the output count, rejects a second value, and renders the
/// (sole) value into a bounded scratch buffer before handing it to `extract`
/// — importantly, that render happens before the filter is asked for its
/// next value, so a second, oversized value cannot slip through uncounted.
/// `label` distinguishes conditions from value-producing filters in error
/// text.
fn run_single_value<T>(
    filter_source: &str,
    input_json: &str,
    globals: &Globals,
    cancelled: &AtomicBool,
    label: &str,
    mut extract: impl FnMut(Val, Vec<u8>) -> Result<T>,
) -> Result<T> {
    let mut result = None;
    let mut count = 0usize;
    run_filter_with(filter_source, input_json, globals, cancelled, |value| {
        count += 1;
        if count > 1 {
            bail!(
                "jq {label} {filter_source:?} produced {count} outputs; expected exactly one value \
                 (wrap a stream in '[...]' to collect it into an array)"
            );
        }
        let mut rendered = Vec::new();
        render_value_into(&value, &mut rendered, cancelled)
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
    globals: &Globals,
    cancelled: &AtomicBool,
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
    check_cancelled(cancelled)?;
    let input = read::parse_single(input_json.as_bytes())
        .map_err(|error| anyhow!("failed to parse jq input as JSON: {error}"))?;
    validate_value_structure(&input)
        .context("jq input structure exceeds the configured memory limit")?;
    check_cancelled(cancelled)?;

    let cached = compiled_filter(filter_source)?;
    check_cancelled(cancelled)?;

    // Skip converting a global into a jaq `Val` tree (`parse_global_var`
    // walks the whole thing) when `filter_source` never references it —
    // the common case for a `when:` guard that only inspects the current
    // value, where `$steps` may have accumulated every named step's output
    // so far. `Vars::new` is positional, not name-keyed, so an unused slot
    // is simply never read; substituting `Val::Null` for it is safe
    // regardless of which position it occupies.
    let steps_val = if cached.uses_steps {
        parse_global_var(globals.steps.as_map(), "$steps")?
    } else {
        Val::Null
    };
    check_cancelled(cancelled)?;
    let inputs_val = if cached.uses_inputs {
        parse_global_var(globals.inputs.as_map(), "$inputs")?
    } else {
        Val::Null
    };
    check_cancelled(cancelled)?;
    let loop_val = if cached.uses_loop {
        parse_global_var(&globals.loop_context, "$loop")?
    } else {
        Val::Null
    };
    check_cancelled(cancelled)?;

    let ctx = Ctx::<data::JustLut<Val>>::new(
        &cached.filter.lut,
        Vars::new([steps_val, inputs_val, loop_val]),
    );
    for result in cached.filter.id.run((ctx, input)).map(unwrap_valr) {
        check_cancelled(cancelled)?;
        let value =
            result.map_err(|error| anyhow!("jq filter {filter_source:?} failed: {error}"))?;
        // `on_value` is invoked before jaq is asked to produce its next value,
        // so `run_single_value` rejects the second value here instead of
        // collecting the rest of an otherwise unbounded stream.
        on_value(value)?;
    }
    check_cancelled(cancelled)?;
    Ok(())
}

/// Converts a global (`$steps`/`$inputs`, or `$loop`) directly into a jaq
/// `Val`, bounding its resulting structure the same way the jq input itself
/// is bounded. `label` names the global in the error text. `Val` implements
/// `serde::Deserialize` (the `jaq-json` "serde" feature) and
/// `serde_json`'s borrowed `Map`/`Value` implement `serde::Deserializer`
/// directly over their own trees, so this walks `value` once, in memory,
/// with no intermediate JSON text — `$steps` can accumulate KB-to-MB of
/// model output over a run, and a serialize-then-reparse round trip on
/// every jq call would pay for that twice.
fn parse_global_var<'a, D>(value: D, label: &str) -> Result<Val>
where
    D: serde::Deserializer<'a>,
    D::Error: std::fmt::Display,
{
    let parsed = Val::deserialize(value)
        .map_err(|error| anyhow!("failed to convert {label} data to a jq value: {error}"))?;
    validate_value_structure(&parsed)
        .with_context(|| format!("jq '{label}' structure exceeds the configured memory limit"))?;
    Ok(parsed)
}

/// The byte-length and nesting-depth checks below are a full char-by-char
/// scan of `filter_source`. Both callers (`check_syntax`, `run_filter_with`)
/// look this same `filter_source` up in [`FILTER_CACHE`] shortly afterwards
/// via [`compiled_filter`], and a filter only ever enters that cache *after*
/// this exact scan has already passed (`compiled_filter` never caches a
/// filter that failed to parse/compile, and this function is always called
/// before it). So a cache hit here is sound evidence the scan already ran
/// successfully for this exact source text; short-circuit on it instead of
/// repeating the scan for every `for_each` item or loop iteration.
fn validate_filter_source(filter_source: &str) -> Result<()> {
    if FILTER_CACHE.contains(filter_source) {
        return Ok(());
    }

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

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    crate::cancellation::check_flag(cancelled, "jq evaluation was cancelled")
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
mod tests;
