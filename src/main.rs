use clap::Parser;

mod app;
mod cli;
mod config;
mod jq;
mod llm;
mod response;
mod schema;
mod workflow;

#[tokio::main]
async fn main() {
    if let Err(error) = app::run(cli::Cli::parse()).await {
        eprintln!("lait: {error}");
        std::process::exit(1);
    }
}
