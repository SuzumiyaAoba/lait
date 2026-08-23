use clap::Parser;

/// Lightweight AI Tool command-line interface.
#[derive(Debug, Parser)]
#[command(name = "lait", version, about = "Lightweight AI Tool")]
struct Cli;

fn main() {
    let _ = Cli::parse();
    println!("Hello, World!");
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn parses_without_arguments() {
        let cli = Cli::try_parse_from(["lait"]);

        assert!(cli.is_ok());
    }
}
