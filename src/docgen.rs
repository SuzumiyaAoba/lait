//! `lait completions`/`lait man`: shell completion scripts and man pages,
//! both derived from the same `clap::Command` tree `--help` is. Extracted
//! out of `app.rs`'s CLI dispatch — neither function touches any other part
//! of the app.

use std::path::Path;

use anyhow::{Context, Result};

use crate::cli::Cli;

/// Writes the completion script for the requested shell to stdout, derived
/// from the same clap `Command` tree `--help` is.
pub(crate) fn generate_completions(args: crate::cli::CompletionsArgs) {
    let mut command = <Cli as clap::CommandFactory>::command();
    clap_complete::generate(args.shell, &mut command, "lait", &mut std::io::stdout());
}

/// Writes a man page for lait and one per (sub)subcommand into `args.dir`,
/// derived from the same clap `Command` tree `--help` is. Pages are named
/// the conventional way (`lait.1`, `lait-run.1`, `lait-agent-run.1`, ...).
pub(crate) fn generate_man_pages(args: crate::cli::ManArgs) -> Result<()> {
    std::fs::create_dir_all(&args.dir).with_context(|| {
        format!(
            "failed to create man page directory '{}'",
            args.dir.display()
        )
    })?;
    let mut command = <Cli as clap::CommandFactory>::command();
    // Propagates global flags into subcommands so their pages show them.
    command.build();
    let count = render_man_pages(&args.dir, &command, "lait")?;
    eprintln!("generated {count} man page(s) in '{}'", args.dir.display());
    Ok(())
}

/// Renders `command`'s own page as `<name>.1` under `dir`, then recurses
/// into its subcommands as `<name>-<subcommand>.1`, returning how many pages
/// were written. The auto-generated `help` subcommand gets no page.
fn render_man_pages(dir: &Path, command: &clap::Command, name: &str) -> Result<usize> {
    let page = clap_mangen::Man::new(command.clone().name(name.to_owned()));
    let mut buffer = Vec::new();
    page.render(&mut buffer)
        .with_context(|| format!("failed to render the man page for '{name}'"))?;
    let path = dir.join(format!("{name}.1"));
    std::fs::write(&path, buffer)
        .with_context(|| format!("failed to write man page '{}'", path.display()))?;

    let mut count = 1;
    for subcommand in command.get_subcommands() {
        if subcommand.get_name() == "help" {
            continue;
        }
        let full_name = format!("{name}-{}", subcommand.get_name());
        count += render_man_pages(dir, subcommand, &full_name)?;
    }
    Ok(count)
}
