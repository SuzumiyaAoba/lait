//! Interactive tool-call approval (`--approve-tools`): the process-wide
//! serialization gate over stdin, the stderr `y`/`n`/`a` prompt, and the
//! `always`-answer cache. [`ToolDecision`]/[`tool_decision`] are the only
//! items a sibling needs — `engine::tool_loop` calls `tool_decision` for
//! every tool call before dispatching it, and matches on the returned
//! `ToolDecision` to decide whether the call proceeds.

use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::{async_io, config, process};

use super::RunContext;

#[cfg(test)]
use super::AppServices;

/// One tool call's pre-dispatch decision — made for every call in a round
/// before any of them actually run, see `ToolLoop::append_tool_calls`'s own doc
/// comment on why this has to happen sequentially and up front rather than
/// inside the concurrent dispatch below.
pub(super) enum ToolDecision {
    Allow,
    Deny(String),
}

/// Checks `qualified_name` against `env.services.file_config.tool_policy` (see
/// `config::ToolPolicy`) and, when `env.policy.approve_tools` is set and the policy
/// didn't already deny it, interactively confirms the call — `y`/`n`/`a`,
/// via `prompt_tool_approval`. This is the *only* place either gate is
/// enforced; `McpRegistry::call`'s own `allowed_tools` check still applies
/// underneath it for an MCP tool (the two are independent, both must pass).
/// `command_preview` renders the argv a shell tool call would actually exec
/// (see `shell_tool::preview_argv`) for display alongside the model's raw
/// arguments — `None` for an MCP/subagent call, which have no such rendering
/// step. Taken as a closure rather than the rendered `Option<String>` itself
/// so the parse+render only happens on the path that actually reaches
/// `prompt_tool_approval` below — never for a call `tool_policy` denies
/// outright, approval isn't enabled for, or is already in
/// `always_approved_tools`.
pub(super) async fn tool_decision(
    env: &RunContext,
    qualified_name: &str,
    arguments: &str,
    command_preview: impl FnOnce() -> Option<String>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<ToolDecision> {
    if !env.services.file_config.tool_policy.allows(qualified_name) {
        return Ok(ToolDecision::Deny(format!(
            "denied by 'tool_policy' in {}",
            config::CONFIG_FILE_NAME
        )));
    }
    if !env.policy.approve_tools {
        return Ok(ToolDecision::Allow);
    }

    // Tool loops from parallel workflow branches and separate compare jobs
    // share one process stdin. Keep the gate until the blocking reader worker
    // has finished, even when the async owner is cancelled; the lease is
    // moved into that worker by `prompt_tool_approval` below.
    let Some(approval_lease) =
        acquire_approval_slot(env, qualified_name, cancellation.as_ref()).await?
    else {
        return Ok(ToolDecision::Allow);
    };
    let command_preview = command_preview();
    let (answer, approval_lease) = prompt_tool_approval(
        qualified_name,
        arguments,
        command_preview.as_deref(),
        approval_lease,
        cancellation,
    )
    .await?;
    match answer {
        ToolApprovalAnswer::Once => Ok(ToolDecision::Allow),
        ToolApprovalAnswer::Always => {
            env.always_approved_tools
                .lock()
                .expect("always_approved_tools lock poisoned")
                .insert(qualified_name.to_owned());
            // Keep the lease through the cache update so a concurrent caller
            // cannot observe the old cache state and prompt a second time.
            drop(approval_lease);
            Ok(ToolDecision::Allow)
        }
        ToolApprovalAnswer::Deny => {
            drop(approval_lease);
            Ok(ToolDecision::Deny(
                "denied interactively (--approve-tools)".to_owned(),
            ))
        }
    }
}

fn always_approved(env: &RunContext, qualified_name: &str) -> bool {
    env.always_approved_tools
        .lock()
        .expect("always_approved_tools lock poisoned")
        .contains(qualified_name)
}

type ApprovalGateLease = tokio::sync::OwnedMutexGuard<()>;

async fn acquire_approval_gate(
    env: &RunContext,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> Result<ApprovalGateLease> {
    let gate = Arc::clone(&env.approval_gate);
    match cancellation {
        Some(cancellation) => {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    Err(crate::error::Interrupted::cancelled(
                        "tool approval was cancelled",
                    ).into())
                }
                lease = gate.lock_owned() => Ok(lease),
            }
        }
        None => Ok(gate.lock_owned().await),
    }
}

/// Acquires the process-wide approval gate and rechecks the `always` cache
/// while holding it. The second check closes the race where another tool loop
/// approved this name while this caller was waiting for stdin ownership.
async fn acquire_approval_slot(
    env: &RunContext,
    qualified_name: &str,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> Result<Option<ApprovalGateLease>> {
    let approval_lease = acquire_approval_gate(env, cancellation).await?;
    if always_approved(env, qualified_name) {
        drop(approval_lease);
        return Ok(None);
    }
    Ok(Some(approval_lease))
}

#[derive(Debug)]
enum ToolApprovalAnswer {
    Once,
    Always,
    Deny,
}

/// Prompts on stderr and reads one `y`/`n`/`a` answer from stdin for
/// `--approve-tools` — see `workflow::ask::run_ask`, whose TTY-detection and
/// non-interactive-is-an-error reasoning this mirrors exactly (a
/// non-interactive stdin has no one to answer and no way to tell a closed
/// pipe from a slow human, so this fails fast rather than hanging or
/// silently denying). Unlike `run_ask`, there is no `default:` to fall back
/// to here — `--approve-tools` without a terminal is simply a
/// misconfiguration to report, not a case with a sensible default answer.
async fn prompt_tool_approval(
    name: &str,
    arguments: &str,
    command_preview: Option<&str>,
    approval_lease: ApprovalGateLease,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<(ToolApprovalAnswer, ApprovalGateLease)> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        bail!(
            "'--approve-tools' requires an interactive stdin to confirm calling '{name}', but \
             stdin is not a terminal"
        );
    }
    eprintln!("tool call: {name}");
    eprintln!("arguments: {arguments}");
    // A shell tool's `command:` template can transform the arguments above
    // into something quite different from what they look like on their own
    // (e.g. splicing a path into a larger shell one-liner) — show the
    // actual argv about to run so approval is informed by what will really
    // execute, not just the model's raw JSON.
    if let Some(command_preview) = command_preview {
        eprintln!("command: {command_preview}");
    }
    eprint!("allow this call? [y(es)/n(o)/a(lways for this tool)] ");
    let name = name.to_owned();
    read_tool_approval_with_lease(approval_lease, cancellation, move || {
        read_tool_approval_answer(&name)
    })
    .await
}

/// Runs one approval reader while transferring the gate lease into the
/// worker. `run_blocking` may return a cancellation error before that worker
/// exits; in that case the worker still owns the lease and remains the sole
/// reader of stdin until its blocking read completes.
async fn read_tool_approval_with_lease<R>(
    approval_lease: ApprovalGateLease,
    cancellation: Option<tokio_util::sync::CancellationToken>,
    reader: R,
) -> Result<(ToolApprovalAnswer, ApprovalGateLease)>
where
    R: FnOnce() -> Result<ToolApprovalAnswer> + Send + 'static,
{
    async_io::run_blocking(
        move |_cancelled| Ok((reader()?, approval_lease)),
        cancellation,
    )
    .await
}

/// The blocking half of `prompt_tool_approval`, run on a dedicated thread via
/// `async_io::run_blocking` the same way `workflow::ask::run_ask`'s own
/// blocking stdin read is. No re-prompt loop on a bad answer, for the same
/// reason `ask.rs`'s `validate_choice` doesn't retry: stdin may not be
/// interactive in every sense, and looping risks hanging rather than ever
/// finishing.
fn read_tool_approval_answer(name: &str) -> Result<ToolApprovalAnswer> {
    use std::io::{BufRead, Write};
    std::io::stderr().flush().ok();
    let mut buffer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut buffer)
        .context("failed to read from stdin")?;
    let answer = process::strip_one_trailing_line_ending(buffer);
    match answer.as_str() {
        "y" | "Y" => Ok(ToolApprovalAnswer::Once),
        "a" | "A" => Ok(ToolApprovalAnswer::Always),
        "n" | "N" => Ok(ToolApprovalAnswer::Deny),
        other => bail!(
            "unrecognized answer {other:?} to 'allow this call?' for tool '{name}'; expected \
             'y', 'n', or 'a'"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AppServices, RunContext, ToolApprovalAnswer, acquire_approval_gate, acquire_approval_slot,
        read_tool_approval_with_lease,
    };
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn approval_gate_serializes_injected_readers() {
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let first_started = Arc::new(AtomicBool::new(false));
        let first_answer = {
            let (sender, receiver) = std::sync::mpsc::channel();
            let lease = gate.clone().lock_owned().await;
            let started = Arc::clone(&first_started);
            let first = tokio::spawn(read_tool_approval_with_lease(lease, None, move || {
                started.store(true, Ordering::Release);
                receiver
                    .recv()
                    .map_err(|_| anyhow::anyhow!("first approval reader was closed"))?;
                Ok(ToolApprovalAnswer::Once)
            }));
            (first, sender)
        };
        for _ in 0..100 {
            if first_started.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(first_started.load(Ordering::Acquire));

        let second_started = Arc::new(AtomicBool::new(false));
        let second = tokio::spawn({
            let gate = Arc::clone(&gate);
            let started = Arc::clone(&second_started);
            async move {
                let lease = gate.lock_owned().await;
                read_tool_approval_with_lease(lease, None, move || {
                    started.store(true, Ordering::Release);
                    Ok(ToolApprovalAnswer::Once)
                })
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !second_started.load(Ordering::Acquire),
            "a second approval reader must wait for the first reader"
        );

        first_answer.1.send(()).unwrap();
        first_answer.0.await.unwrap().unwrap();
        for _ in 0..100 {
            if second_started.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(second_started.load(Ordering::Acquire));
        assert!(matches!(
            second.await.unwrap().unwrap().0,
            ToolApprovalAnswer::Once
        ));
    }

    #[tokio::test]
    async fn cancelling_an_approval_reader_keeps_the_gate_until_the_worker_exits() {
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let lease = gate.clone().lock_owned().await;
        let started = Arc::new(AtomicBool::new(false));
        let (answer_sender, answer_receiver) = std::sync::mpsc::channel();
        let cancellation = CancellationToken::new();
        let reader = tokio::spawn({
            let started = Arc::clone(&started);
            read_tool_approval_with_lease(lease, Some(cancellation.clone()), move || {
                started.store(true, Ordering::Release);
                answer_receiver
                    .recv()
                    .map_err(|_| anyhow::anyhow!("approval reader was closed"))?;
                Ok(ToolApprovalAnswer::Once)
            })
        });
        for _ in 0..100 {
            if started.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(started.load(Ordering::Acquire));

        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(1), reader)
            .await
            .expect("a cancelled approval owner must return promptly")
            .unwrap()
            .unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<crate::error::Interrupted>())
        );
        assert!(
            gate.clone().try_lock_owned().is_err(),
            "the cancelled worker must retain the approval gate while blocked"
        );

        answer_sender.send(()).unwrap();
        let released = tokio::time::timeout(Duration::from_secs(1), gate.lock_owned())
            .await
            .expect("the worker must eventually release the gate");
        drop(released);
    }

    #[tokio::test]
    async fn always_cache_is_rechecked_after_waiting_for_the_approval_gate() {
        let services = Arc::new(AppServices::new(Arc::new(
            crate::config::ConfigFile::default(),
        )));
        let env = Arc::new(RunContext::new(services, CancellationToken::new()));
        let held = acquire_approval_gate(&env, None).await.unwrap();
        let waiter = tokio::spawn({
            let env = Arc::clone(&env);
            async move { acquire_approval_slot(&env, "tool__echo", None).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        env.always_approved_tools
            .lock()
            .unwrap()
            .insert("tool__echo".to_owned());
        drop(held);

        let slot = waiter.await.unwrap().unwrap();
        assert!(
            slot.is_none(),
            "always-approved calls must not prompt again"
        );
    }
}
