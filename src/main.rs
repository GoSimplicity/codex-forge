mod cli;
mod codex;
mod commands;
mod config;
mod harness;
mod model;
mod tui;
mod workspace;

#[tokio::main]
async fn main() {
    if let Err(error) = commands::run().await {
        eprintln!("❌ {error:#}");
        std::process::exit(1);
    }
}
