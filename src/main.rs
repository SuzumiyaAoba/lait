use clap::Parser;

mod agent;
mod app;
mod assert;
mod async_cache;
mod async_io;
mod attachment;
mod cache;
mod cassette;
mod checkpoint;
mod cli;
mod compare;
mod config;
mod docgen;
mod doctor;
mod dotenv;
mod engine;
mod error;
mod eval;
mod frontmatter;
mod history;
mod init;
mod jq;
mod jsonl;
mod lint;
mod llm;
mod logging;
mod mcp;
mod models;
mod nesting;
mod process;
mod prompt;
mod registry;
mod render;
mod repl;
mod report;
mod response;
mod schema;
mod secret;
mod session;
mod shell_tool;
mod signal;
mod skill;
mod storage;
mod subagent;
mod template;
mod test_run;
#[cfg(test)]
mod test_support;
mod usage;
mod workflow;
mod xdg;

fn main() {
    // `.env` must be loaded before `Cli::parse()` runs (clap's `env = ...`
    // fallbacks read the process environment at parse time), so `--no-env`
    // is detected from the raw command line here; the `Cli` flag of the
    // same name only exists for `--help` and validation. Everything up to a
    // literal `--` is scanned, matching where clap itself would accept the
    // flag.
    let no_env = std::env::args()
        .skip(1)
        .take_while(|argument| argument != "--")
        .any(|argument| argument == "--no-env");
    if !no_env {
        // SAFETY: called before the tokio runtime below spawns its worker
        // threads, so no other thread can be reading the environment yet —
        // see `load_from_current_dir`'s safety contract.
        if let Err(error) = unsafe { dotenv::load_from_current_dir() } {
            exit_with_error(error, false);
        }
    }

    let cli = cli::Cli::parse();
    logging::init(cli.verbose);
    // Captured before `cli` is moved into `run_blocking`/`run` below — the
    // command-specific exit policy: all lint failures are validation errors.
    let is_lint = matches!(cli.command, Some(cli::Command::Lint(_)));

    // The purely local subcommands (completions/man/init/lint/local models)
    // never await; skip spawning the runtime's worker threads for them —
    // `lait completions` runs from shell startup files, where that cost is
    // felt on every new shell.
    if !app::needs_async_runtime(&cli) {
        if let Err(error) = app::run_blocking(cli) {
            exit_with_error(error, is_lint);
        }
        return;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => exit_with_error(
            anyhow::Error::new(error).context("failed to start the async runtime"),
            is_lint,
        ),
    };
    if let Err(error) = runtime.block_on(app::run(cli)) {
        exit_with_error(error, is_lint);
    }
}

fn exit_with_error(error: anyhow::Error, is_lint: bool) -> ! {
    eprintln!("lait: {error:#}");
    // An explicit SIGINT keeps the conventional shell exit code; execution
    // deadlines and programmatic cancellation use the typed error policy.
    let code = if signal::received() {
        signal::SIGINT_EXIT_CODE
    } else {
        error::classify(&error, is_lint) as i32
    };
    std::process::exit(code);
}
