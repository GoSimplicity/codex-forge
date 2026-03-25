use anyhow::Result;
use crossterm::event::KeyCode;

use super::app::TuiApp;
use super::tabs::FocusMode;

impl TuiApp {
    pub(crate) async fn handle_key(&mut self, code: KeyCode) -> Result<bool> {
        match self.focus {
            FocusMode::Browse => self.handle_browse_key(code).await,
            FocusMode::Compose => self.handle_compose_key(code).await,
        }
    }

    async fn handle_browse_key(&mut self, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('j') | KeyCode::Down => self.select_next()?,
            KeyCode::Char('k') | KeyCode::Up => self.select_prev()?,
            KeyCode::Char('h') => self.select_prev_run()?,
            KeyCode::Char('l') => self.select_next_run()?,
            KeyCode::Char('J') => self.select_next_task_node()?,
            KeyCode::Char('K') => self.select_prev_task_node()?,
            KeyCode::Tab => self.detail_tab = self.detail_tab.next(),
            KeyCode::Char('i') => {
                self.focus = FocusMode::Compose;
                self.status = "输入消息后按 Enter 发送，Esc 返回浏览模式".to_string();
            }
            KeyCode::Char('n') => {
                let title = if self.composer.trim().is_empty() {
                    None
                } else {
                    Some(self.composer.trim())
                };
                let thread = self.store.create_thread(title)?;
                self.selected_thread_id = Some(thread.id.clone());
                self.composer.clear();
                self.refresh()?;
                self.status = format!("已创建 thread `{}`", thread.id);
            }
            KeyCode::Char('a') => self.approve_first_pending().await?,
            KeyCode::Char('x') => self.deny_first_pending().await?,
            KeyCode::Char('s') => self.resume_selected_run().await?,
            KeyCode::Char('c') => self.cancel_selected_run()?,
            KeyCode::Char('R') => self.retry_selected_task_node().await?,
            KeyCode::Char('r') => {
                self.refresh()?;
                self.status = "已刷新".to_string();
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_compose_key(&mut self, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Esc => {
                self.focus = FocusMode::Browse;
                self.status = "返回浏览模式".to_string();
            }
            KeyCode::Enter => self.send_message().await?,
            KeyCode::Backspace => {
                self.composer.pop();
            }
            KeyCode::Char(ch) => {
                self.composer.push(ch);
            }
            _ => {}
        }
        Ok(false)
    }
}
