use clap::Parser;

mod agent;
mod app;
mod attachment;
mod cli;
mod config;
mod dotenv;
mod frontmatter;
mod init;
mod jq;
mod lint;
mod llm;
mod mcp;
mod models;
mod prompt;
mod repl;
mod response;
mod schema;
mod session;
mod skill;
mod subagent;
mod template;
mod workflow;

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
            exit_with_error(error);
        }
    }

    let cli = cli::Cli::parse();

    // The purely local subcommands (completions/man/init/lint/local models)
    // never await; skip spawning the runtime's worker threads for them —
    // `lait completions` runs from shell startup files, where that cost is
    // felt on every new shell.
    if !app::needs_async_runtime(&cli) {
        if let Err(error) = app::run_blocking(cli) {
            exit_with_error(error);
        }
        return;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            exit_with_error(anyhow::Error::new(error).context("failed to start the async runtime"))
        }
    };
    if let Err(error) = runtime.block_on(app::run(cli)) {
        exit_with_error(error);
    }
}

fn exit_with_error(error: anyhow::Error) -> ! {
    eprintln!("lait: {error:#}");
    std::process::exit(1);
}
