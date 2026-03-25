use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs, Wrap};

use crate::commands::format::{
    approval_status_label, artifact_kind_label, describe_event, role_label, status_label,
};
use crate::harness::{
    ApprovalRecord, ArtifactRecord, HarnessEventRecord, HarnessMessage, TaskNodeKind,
    TaskNodeRecord, TaskNodeStatus,
};

use super::app::TuiApp;
use super::tabs::{DetailTab, FocusMode};

impl TuiApp {
    pub(crate) fn render(&self, frame: &mut ratatui::Frame) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(16),
                Constraint::Length(5),
                Constraint::Length(2),
            ])
            .split(frame.area());

        frame.render_widget(render_header(self), layout[0]);

        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(30),
                Constraint::Length(42),
                Constraint::Min(48),
            ])
            .split(layout[1]);

        frame.render_widget(
            List::new(thread_items(self))
                .block(Block::default().borders(Borders::ALL).title("Threads")),
            body[0],
        );

        let middle = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Length(8),
                Constraint::Min(8),
            ])
            .split(body[1]);
        frame.render_widget(
            Paragraph::new(selected_thread_summary(self))
                .block(Block::default().borders(Borders::ALL).title("Overview"))
                .wrap(Wrap { trim: true }),
            middle[0],
        );
        frame.render_widget(
            List::new(run_items(self)).block(Block::default().borders(Borders::ALL).title("Runs")),
            middle[1],
        );
        frame.render_widget(
            List::new(task_node_items(self))
                .block(Block::default().borders(Borders::ALL).title("Task Nodes")),
            middle[2],
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
        .block(Block::default().borders(Borders::ALL).title("Detail"))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .select(self.detail_tab.index());
        frame.render_widget(tabs, right[0]);

        match self.detail_tab {
            DetailTab::Messages => render_messages_panel(frame, right[1], &self.messages),
            DetailTab::Node => render_node_panel(frame, right[1], selected_node(self)),
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

fn render_header<'a>(app: &'a TuiApp) -> Paragraph<'a> {
    let active_run = app.selected_run_id.as_deref().unwrap_or("-");
    let active_node = app.selected_task_node_id.as_deref().unwrap_or("-");
    Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "Codex Forge",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                app.repo_root.display().to_string(),
                Style::default().fg(Color::Gray),
            ),
        ]),
        Line::from(vec![
            Span::styled("run ", Style::default().fg(Color::Yellow)),
            Span::raw(active_run),
            Span::raw("  "),
            Span::styled("node ", Style::default().fg(Color::Green)),
            Span::raw(active_node),
        ]),
        Line::from(
            "j/k thread | h/l run | J/K node | i 输入 | a/x 审批 | s 恢复 | c 取消 | R 重试",
        ),
    ])
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
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(if selected { "▸ " } else { "  " }, style),
                Span::styled(thread.title.clone(), style),
                Span::raw(format!(
                    "\n  {} msg / {} run / last={}",
                    thread.message_count,
                    thread.run_count,
                    thread.last_run_status.map(status_label).unwrap_or("none")
                )),
            ]))
        })
        .collect()
}

fn run_items(app: &TuiApp) -> Vec<ListItem<'static>> {
    if app.runs.is_empty() {
        return vec![ListItem::new("当前 thread 还没有 run")];
    }

    app.runs
        .iter()
        .map(|run| {
            let selected = app.selected_run_id.as_ref().is_some_and(|id| id == &run.id);
            let style = if selected {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(if selected { "▸ " } else { "  " }, style),
                Span::styled(run.id.clone(), style),
                Span::raw(format!(
                    "\n  {} | turns={} | active={}",
                    status_label(run.status),
                    run.turn_count,
                    run.active_task_node_id.as_deref().unwrap_or("-")
                )),
            ]))
        })
        .collect()
}

fn task_node_items(app: &TuiApp) -> Vec<ListItem<'static>> {
    if app.task_nodes.is_empty() {
        return vec![ListItem::new("当前 run 没有 task nodes")];
    }

    app.task_nodes
        .iter()
        .map(|node| {
            let selected = app
                .selected_task_node_id
                .as_ref()
                .is_some_and(|id| id == &node.id);
            let style = if selected {
                Style::default().fg(node_color(node.status))
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(if selected { "▸ " } else { "  " }, style),
                Span::styled(
                    format!("{} [{}]", node.title, task_status_label(node.status)),
                    style,
                ),
                Span::raw(format!(
                    "\n  {:?} | attempts={}",
                    node.kind, node.attempt_count
                )),
            ]))
        })
        .collect()
}

fn selected_thread_summary(app: &TuiApp) -> String {
    let thread_line = app
        .selected_thread_id
        .as_deref()
        .map(|thread_id| format!("thread={thread_id}"))
        .unwrap_or_else(|| "thread=-".to_string());
    let run_line = app
        .runs
        .iter()
        .find(|run| app.selected_run_id.as_deref() == Some(run.id.as_str()))
        .map(|run| {
            format!(
                "run={} | {} | blocked={}",
                run.id,
                status_label(run.status),
                run.blocked_reason.as_deref().unwrap_or("-")
            )
        })
        .unwrap_or_else(|| "run=-".to_string());
    format!(
        "{}\n{}\nmessages={} | approvals={} | artifacts={}\nworking={} | project={}",
        thread_line,
        run_line,
        app.messages.len(),
        app.approvals.len(),
        app.artifacts.len(),
        app.working_memory.len(),
        app.project_memory.len()
    )
}

fn render_messages_panel(frame: &mut ratatui::Frame, area: Rect, messages: &[HarnessMessage]) {
    let items = messages
        .iter()
        .rev()
        .take(24)
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

fn render_node_panel(frame: &mut ratatui::Frame, area: Rect, node: Option<&TaskNodeRecord>) {
    let text = if let Some(node) = node {
        format!(
            "id={}\nkind={}\nstatus={}\nattempts={}\ndepends_on={}\n\ninstructions:\n{}\n\nsummary:\n{}\n\nerror:\n{}",
            node.id,
            task_kind_label(node.kind),
            task_status_label(node.status),
            node.attempt_count,
            if node.depends_on.is_empty() {
                "-".to_string()
            } else {
                node.depends_on.join(", ")
            },
            node.instructions,
            node.output_summary.as_deref().unwrap_or("-"),
            node.error.as_deref().unwrap_or("-"),
        )
    } else {
        "当前没有选中节点".to_string()
    };
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title("Node Detail"))
            .wrap(Wrap { trim: true }),
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
        .take(24)
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
        .take(24)
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

fn selected_node(app: &TuiApp) -> Option<&TaskNodeRecord> {
    app.selected_task_node_id
        .as_ref()
        .and_then(|id| app.task_nodes.iter().find(|node| &node.id == id))
}

fn task_status_label(status: TaskNodeStatus) -> &'static str {
    match status {
        TaskNodeStatus::Pending => "pending",
        TaskNodeStatus::Ready => "ready",
        TaskNodeStatus::Running => "running",
        TaskNodeStatus::WaitingForInput => "waiting",
        TaskNodeStatus::Completed => "completed",
        TaskNodeStatus::Failed => "failed",
        TaskNodeStatus::Skipped => "skipped",
    }
}

fn task_kind_label(kind: TaskNodeKind) -> &'static str {
    match kind {
        TaskNodeKind::Plan => "plan",
        TaskNodeKind::Initialize => "initialize",
        TaskNodeKind::BuildExecutionContract => "build_execution_contract",
        TaskNodeKind::SelectNextFeature => "select_next_feature",
        TaskNodeKind::ExecuteFeature => "execute_feature",
        TaskNodeKind::EvaluateFeature => "evaluate_feature",
        TaskNodeKind::CheckpointProgress => "checkpoint_progress",
        TaskNodeKind::FinalizeDelivery => "finalize_delivery",
        TaskNodeKind::Explore => "explore",
        TaskNodeKind::Implement => "implement",
        TaskNodeKind::Review => "review",
        TaskNodeKind::Test => "test",
        TaskNodeKind::Summarize => "summarize",
        TaskNodeKind::ApprovalGate => "approval_gate",
    }
}

fn node_color(status: TaskNodeStatus) -> Color {
    match status {
        TaskNodeStatus::Pending => Color::DarkGray,
        TaskNodeStatus::Ready => Color::Cyan,
        TaskNodeStatus::Running => Color::Yellow,
        TaskNodeStatus::WaitingForInput => Color::Magenta,
        TaskNodeStatus::Completed => Color::Green,
        TaskNodeStatus::Failed => Color::Red,
        TaskNodeStatus::Skipped => Color::Gray,
    }
}
