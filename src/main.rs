mod app;
mod apply;
mod cli;
mod codex;
mod commander;
mod config;
mod doctor;
mod model;
mod orchestrator;
mod replay;
mod repo;
mod roles;
mod session;
mod ui;
mod verify;
mod worktree;

#[tokio::main]
async fn main() {
    if let Err(error) = app::run().await {
        eprintln!("❌ {error:#}");
        std::process::exit(1);
    }
}
