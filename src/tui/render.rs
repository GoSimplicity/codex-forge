use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs, Wrap};

use crate::commands::format::{
    approval_status_label, artifact_kind_label, describe_event, role_label, status_label,
};
use crate::harness::{
    ApprovalRecord, ArtifactRecord, HarnessEventRecord, HarnessMessage, HarnessRunManifest,
};

use super::app::TuiApp;
use super::tabs::{DetailTab, FocusMode};

impl TuiApp {
    pub(crate) fn render(&self, frame: &mut ratatui::Frame) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(12),
                Constraint::Length(5),
                Constraint::Length(2),
            ])
            .split(frame.area());

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        "Codex Forge",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::raw(self.repo_root.display().to_string()),
                ]),
                Line::from("聊天优先 | i 输入 | a 通过审批 | x 拒绝审批 | Tab 切换面板"),
            ]),
            layout[0],
        );

        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(28),
                Constraint::Min(40),
                Constraint::Length(42),
            ])
            .split(layout[1]);

        frame.render_widget(
            List::new(thread_items(self))
                .block(Block::default().borders(Borders::ALL).title("Threads")),
            body[0],
        );

        let center = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(8)])
            .split(body[1]);

        frame.render_widget(
            Paragraph::new(selected_thread_summary(self))
                .block(Block::default().borders(Borders::ALL).title("概览"))
                .wrap(Wrap { trim: true }),
            center[0],
        );

        let message_items = self
            .messages
            .iter()
            .rev()
            .take(32)
            .rev()
            .map(|message| {
                ListItem::new(format!(
                    "[{}] {}",
                    role_label(message.role),
                    message.content.replace('\n', " ")
                ))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(message_items).block(Block::default().borders(Borders::ALL).title("Chat")),
            center[1],
        );

        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(8)])
            .split(body[2]);
        let tabs = Tabs::new(
            DetailTab::all()
                .iter()
                .map(|tab| Line::from(tab.label()))
                .collect::<Vec<_>>(),
        )
        .block(Block::default().borders(Borders::ALL).title("Panels"))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .select(self.detail_tab.index());
        frame.render_widget(tabs, right[0]);

        match self.detail_tab {
            DetailTab::Messages => render_messages_panel(frame, right[1], &self.messages),
            DetailTab::Runs => render_runs_panel(frame, right[1], &self.runs),
            DetailTab::Approvals => render_approvals_panel(frame, right[1], &self.approvals),
            DetailTab::Artifacts => render_artifacts_panel(frame, right[1], &self.artifacts),
            DetailTab::Events => render_events_panel(frame, right[1], &self.events),
        }

        frame.render_widget(
            Paragraph::new(self.composer.as_str())
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(match self.focus {
                            FocusMode::Browse => "Composer（按 i 输入）",
                            FocusMode::Compose => "Composer（Enter 发送，Esc 返回）",
                        }),
                )
                .wrap(Wrap { trim: false }),
            layout[2],
        );
        frame.render_widget(Paragraph::new(self.status.as_str()), layout[3]);
    }
}

fn thread_items(app: &TuiApp) -> Vec<ListItem<'static>> {
    if app.threads.is_empty() {
        return vec![ListItem::new("还没有 thread，按 n 创建")];
    }

    app.threads
        .iter()
        .map(|thread| {
            let selected = app
                .selected_thread_id
                .as_ref()
                .is_some_and(|id| id == &thread.id);
            let style = if selected {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(if selected { ">" } else { " " }, style),
                Span::raw(" "),
                Span::styled(thread.title.clone(), style),
                Span::raw(format!(
                    "\n  {} msg / {} run",
                    thread.message_count, thread.run_count
                )),
            ]))
        })
        .collect()
}

fn selected_thread_summary(app: &TuiApp) -> String {
    if let Some(thread) = app
        .selected_thread_id
        .as_ref()
        .and_then(|id| app.threads.iter().find(|thread| &thread.id == id))
    {
        format!(
            "thread={} | 消息={} | runs={} | 待审批={}",
            thread.id,
            thread.message_count,
            thread.run_count,
            app.approvals.len()
        )
    } else {
        "未选中 thread".to_string()
    }
}

fn render_messages_panel(frame: &mut ratatui::Frame, area: Rect, messages: &[HarnessMessage]) {
    let items = messages
        .iter()
        .rev()
        .take(20)
        .rev()
        .map(|message| {
            ListItem::new(format!(
                "[{}] {}",
                role_label(message.role),
                message.content.replace('\n', " ")
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title("Messages")),
        area,
    );
}

fn render_runs_panel(frame: &mut ratatui::Frame, area: Rect, runs: &[HarnessRunManifest]) {
    let items = runs
        .iter()
        .map(|run| {
            ListItem::new(format!(
                "{} [{}] {}",
                run.id,
                status_label(run.status),
                run.summary.as_deref().unwrap_or("无摘要")
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title("Runs")),
        area,
    );
}

fn render_approvals_panel(frame: &mut ratatui::Frame, area: Rect, approvals: &[ApprovalRecord]) {
    let items = approvals
        .iter()
        .map(|approval| {
            ListItem::new(format!(
                "{} [{}] {}: {}",
                approval.id,
                approval_status_label(approval.status),
                approval.tool_name,
                approval.reason
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title("Approvals")),
        area,
    );
}

fn render_artifacts_panel(frame: &mut ratatui::Frame, area: Rect, artifacts: &[ArtifactRecord]) {
    let items = artifacts
        .iter()
        .take(20)
        .map(|artifact| {
            ListItem::new(format!(
                "{} [{}] {}",
                artifact.label,
                artifact_kind_label(artifact.kind),
                artifact.path.display()
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title("Artifacts")),
        area,
    );
}

fn render_events_panel(frame: &mut ratatui::Frame, area: Rect, events: &[HarnessEventRecord]) {
    let items = events
        .iter()
        .rev()
        .take(20)
        .rev()
        .map(|event| {
            ListItem::new(format!(
                "{} {}",
                event.at.format("%H:%M:%S"),
                describe_event(&event.payload)
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title("Events")),
        area,
    );
}
