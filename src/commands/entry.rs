use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Commands, TuiArgs};
use crate::tui::run_tui;
use crate::{commands, commands::approval, commands::artifact, commands::chat, commands::config};

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => {
            run_tui(TuiArgs {
                target_dir: None,
                thread: None,
            })
            .await?
        }
        Some(Commands::Tui(args)) => run_tui(args).await?,
        Some(Commands::Thread(args)) => commands::thread::run(args)?,
        Some(Commands::Chat(args)) => chat::run(args).await?,
        Some(Commands::Run(args)) => commands::runs::run(args)?,
        Some(Commands::Replay(args)) => commands::replay::run(args)?,
        Some(Commands::Approval(args)) => approval::run(args).await?,
        Some(Commands::Artifact(args)) => artifact::run(args)?,
        Some(Commands::Config(args)) => config::run(args)?,
    }
    Ok(())
}
