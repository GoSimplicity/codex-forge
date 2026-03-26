mod app;
mod data;
mod input;
mod render;
mod tabs;

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::cli::TuiArgs;
use crate::workspace::resolve_target_dir;

use self::app::TuiApp;

pub async fn run_tui(args: TuiArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let mut app = TuiApp::new(repo_root, args.thread)?;
    app.refresh()?;

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = async {
        loop {
            app.poll_background_tasks().await?;
            app.maybe_auto_refresh()?;
            terminal.draw(|frame| app.render(frame))?;
            if event::poll(Duration::from_millis(150))? {
                let Event::Key(key) = event::read()? else {
                    continue;
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if app.handle_key(key.code).await? {
                    break;
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}
