mod app;
mod cli;
mod config;
mod codex;
mod harness;
mod model;
mod tui;
mod workspace;

#[tokio::main]
async fn main() {
    if let Err(error) = app::run().await {
        eprintln!("❌ {error:#}");
        std::process::exit(1);
    }
}
