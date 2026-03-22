mod app;
mod app_shell;
mod apply;
mod cli;
mod codex;
mod commander;
mod config;
mod doctor;
mod memory;
mod model;
mod orchestrator;
mod replay;
mod repo;
mod resources;
mod roles;
mod session;
mod time;
mod ui;
mod verify;
mod workspace;
mod worktree;

#[tokio::main]
async fn main() {
    if let Err(error) = app::run().await {
        eprintln!("❌ {error:#}");
        std::process::exit(1);
    }
}
