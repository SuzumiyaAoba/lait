use clap::Parser;

mod app;
mod cli;
mod config;
mod response;
mod schema;

#[tokio::main]
async fn main() {
    if let Err(error) = app::run(cli::Cli::parse()).await {
        eprintln!("lait: {error}");
        std::process::exit(1);
    }
}
