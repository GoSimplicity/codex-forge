use anyhow::Result;
use crossterm::event::KeyCode;

use super::app::TuiApp;
use super::tabs::FocusMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalKeyAction {
    Next,
    Prev,
    Approve,
    Deny,
}

impl TuiApp {
    pub(crate) async fn handle_key(&mut self, code: KeyCode) -> Result<bool> {
        match self.focus {
            FocusMode::Browse => self.handle_browse_key(code).await,
            FocusMode::Compose => self.handle_compose_key(code).await,
        }
    }

    async fn handle_browse_key(&mut self, code: KeyCode) -> Result<bool> {
        if !matches!(
            code,
            KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Enter | KeyCode::Esc
        ) {
            self.pending_delete_thread_id = None;
        }

        if self.pending_delete_thread_id.is_none()
            && let Some(action) = approval_key_action(code, !self.approvals.is_empty())
        {
            match action {
                ApprovalKeyAction::Next => self.select_next_approval(),
                ApprovalKeyAction::Prev => self.select_prev_approval(),
                ApprovalKeyAction::Approve => self.approve_first_pending().await?,
                ApprovalKeyAction::Deny => self.deny_first_pending().await?,
            }
            return Ok(false);
        }

        match code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.focus_next_pane(),
            KeyCode::Left | KeyCode::Char('h') => self.focus_prev_pane(),
            KeyCode::Down | KeyCode::Char('j') => self.select_next_in_focus()?,
            KeyCode::Up | KeyCode::Char('k') => self.select_prev_in_focus()?,
            KeyCode::Char('J') => self.select_next_task_node()?,
            KeyCode::Char('K') => self.select_prev_task_node()?,
            KeyCode::Enter => {
                if self.pending_delete_thread_id.is_some() {
                    self.delete_selected_thread()?;
                } else if matches!(self.browse_pane, super::tabs::BrowsePane::Composer)
                    && !self.composer.trim().is_empty()
                {
                    self.send_message().await?;
                } else if matches!(self.browse_pane, super::tabs::BrowsePane::Composer) {
                    self.focus = FocusMode::Compose;
                    self.status = "开始输入；Enter 保存，Esc 返回".to_string();
                } else if self.can_confirm_selected_plan() {
                    self.confirm_selected_plan_and_resume().await?;
                } else {
                    self.enter_detail();
                }
            }
            KeyCode::Backspace | KeyCode::Delete => {
                if matches!(code, KeyCode::Delete)
                    && self.pending_delete_thread_id.is_none()
                    && self.approvals.is_empty()
                    && matches!(self.browse_pane, super::tabs::BrowsePane::Threads)
                {
                    self.delete_selected_thread()?;
                }
            }
            KeyCode::Esc => {
                if self.pending_delete_thread_id.take().is_some() {
                    self.status = "已取消删除".to_string();
                } else if matches!(self.browse_pane, super::tabs::BrowsePane::Detail) {
                    self.exit_detail();
                }
            }
            KeyCode::Char('i') => {
                self.focus = FocusMode::Compose;
                self.browse_pane = super::tabs::BrowsePane::Composer;
                self.status = "进入草稿编辑；Enter 保存草稿，Esc 返回浏览模式".to_string();
            }
            KeyCode::Char('n') => {
                let thread = self.store.create_thread(None)?;
                self.selected_thread_id = Some(thread.id.clone());
                self.refresh()?;
                self.status = format!("已创建 thread `{}`", thread.id);
            }
            KeyCode::Char('d') | KeyCode::Char('D') => self.delete_selected_thread()?,
            KeyCode::Char('m') => self.cycle_backend_provider(),
            KeyCode::Char('s') => self.resume_selected_run().await?,
            KeyCode::Char('t') => self.retry_selected_task_node().await?,
            KeyCode::Char('r') => {
                self.refresh()?;
                self.status = "已刷新".to_string();
            }
            KeyCode::Char(ch) => {
                if self.pending_delete_thread_id.is_none() && self.approvals.is_empty() {
                    self.browse_pane = super::tabs::BrowsePane::Composer;
                    self.focus = FocusMode::Compose;
                    self.composer.push(ch);
                    self.status = "开始输入；Enter 保存，Esc 返回".to_string();
                }
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_compose_key(&mut self, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Esc => {
                self.focus = FocusMode::Browse;
                self.status = "草稿已保留，返回浏览模式；按 Enter 可运行".to_string();
            }
            KeyCode::Enter => {
                self.focus = FocusMode::Browse;
                self.status = "草稿已保存；按 Enter 可运行".to_string();
            }
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

fn approval_key_action(code: KeyCode, has_approvals: bool) -> Option<ApprovalKeyAction> {
    if !has_approvals {
        return None;
    }
    match code {
        KeyCode::Down | KeyCode::Char('j') => Some(ApprovalKeyAction::Next),
        KeyCode::Up | KeyCode::Char('k') => Some(ApprovalKeyAction::Prev),
        KeyCode::Enter | KeyCode::Char('a') => Some(ApprovalKeyAction::Approve),
        KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('x') => Some(ApprovalKeyAction::Deny),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::{ApprovalKeyAction, approval_key_action};

    #[test]
    fn approval_shortcuts_are_global_when_pending_exists() {
        assert_eq!(
            approval_key_action(KeyCode::Enter, true),
            Some(ApprovalKeyAction::Approve)
        );
        assert_eq!(
            approval_key_action(KeyCode::Char('a'), true),
            Some(ApprovalKeyAction::Approve)
        );
        assert_eq!(
            approval_key_action(KeyCode::Backspace, true),
            Some(ApprovalKeyAction::Deny)
        );
        assert_eq!(
            approval_key_action(KeyCode::Char('x'), true),
            Some(ApprovalKeyAction::Deny)
        );
        assert_eq!(
            approval_key_action(KeyCode::Down, true),
            Some(ApprovalKeyAction::Next)
        );
    }

    #[test]
    fn approval_shortcuts_are_disabled_without_pending_approval() {
        assert_eq!(approval_key_action(KeyCode::Enter, false), None);
        assert_eq!(approval_key_action(KeyCode::Char('a'), false), None);
        assert_eq!(approval_key_action(KeyCode::Backspace, false), None);
    }
}
