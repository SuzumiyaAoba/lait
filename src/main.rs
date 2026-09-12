mod agent;
mod app;
mod assert;
mod async_cache;
mod async_io;
mod attachment;
mod cache;
mod cassette;
mod chat;
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
mod file_lock;
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
mod reasoning;
mod registry;
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
    // `rustls` (pulled in via `async-openai`'s `rustls-no-provider` feature)
    // needs a `CryptoProvider` installed process-wide before any TLS
    // connection is made; without one, the first HTTPS request panics
    // instead of failing gracefully. Installed here, first thing in `main`,
    // rather than lazily on the async/model-request path: today only the
    // async lane ever makes a network request, but a future sync subcommand
    // that does would otherwise panic in production with nothing in CI to
    // catch it, since `cargo check`/`clippy` cannot see a missing runtime
    // installation. `ring` (rather than rustls's default `aws-lc-rs`) is
    // selected via this crate's own `rustls` dependency in `Cargo.toml`,
    // specifically to avoid `aws-lc-sys`'s C/assembly build requirement —
    // see the comment there. The `Err` case only means a provider was
    // already installed (impossible this early, but harmless either way),
    // never that installation is unsupported here.
    let _ = rustls::crypto::ring::default_provider().install_default();

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

    // Computed from `&cli` before `cli.command` is moved into `classify`
    // below — a `&Cli` borrow can't follow a partial move of one of its
    // fields, even though `ConfigSource::from` only ever reads
    // `no_config`/`config`, neither of which `classify` touches.
    let config_source = config::ConfigSource::from(&cli);
    // `cli.command` is moved into `classify` here; every other `Cli` field
    // (`chat`, `cache`, `no_cache`, `approve_tools`) remains available below
    // via the partial move — see `app`'s module doc for why `classify` only
    // ever needs the command itself.
    let dispatch = app::classify(cli.command);
    // The command-specific exit policy: all lint failures are validation
    // errors. Derived from `dispatch` (not re-matched from `cli.command`,
    // which is already moved) so this can never drift from what `classify`
    // itself decided.
    let is_lint = matches!(dispatch, app::Dispatch::Sync(app::SyncCommand::Lint(_)));

    match dispatch {
        // The purely local subcommands (completions/man/init/lint/local
        // models) never await; skip spawning the runtime's worker threads
        // for them — `lait completions` runs from shell startup files,
        // where that cost is felt on every new shell.
        app::Dispatch::Sync(sync_command) => {
            if let Err(error) = app::run_blocking(sync_command, config_source) {
                exit_with_error(error, is_lint);
            }
        }
        app::Dispatch::Async(async_command) => {
            // `--cache`/`--no-cache`/`--approve-tools` are global flags on
            // `Cli` itself, so they're read here rather than re-derived at
            // each async handler's own call site.
            let cache_override = app::cache_override(cli.cache, cli.no_cache);
            let approve_tools = cli.approve_tools;
            // Built once per invocation and passed as the root source to
            // each RunContext. Each async handler arms
            // `signal::spawn_handler` at the point where it starts using
            // the token; the REPL watches that root while reading and
            // running turns so cleanup also covers Ctrl-C there.
            let cancel = tokio_util::sync::CancellationToken::new();

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
            if let Err(error) = runtime.block_on(app::run(
                *async_command,
                cli.chat,
                config_source,
                cache_override,
                approve_tools,
                cancel,
            )) {
                exit_with_error(error, is_lint);
            }
        }
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
