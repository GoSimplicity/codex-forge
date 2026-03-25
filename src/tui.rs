use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs, Wrap};

use crate::app::{
    approval_status_label, artifact_kind_label, describe_event, role_label, status_label,
};
use crate::cli::TuiArgs;
use crate::config::load_project_config;
use crate::harness::{
    ApprovalStatus, ArtifactRecord, ChatRequest, HarnessEventRecord, HarnessMessage,
    HarnessRunManifest, HarnessStore, HarnessThreadManifest, chat_once,
    resolve_approval_and_resume,
};
use crate::model::ThinkingMode;
use crate::workspace::resolve_target_dir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusMode {
    Browse,
    Compose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailTab {
    Messages,
    Runs,
    Approvals,
    Artifacts,
    Events,
}

impl DetailTab {
    fn all() -> [Self; 5] {
        [
            Self::Messages,
            Self::Runs,
            Self::Approvals,
            Self::Artifacts,
            Self::Events,
        ]
    }

    fn label(self) -> &'static str {
        match self {
            Self::Messages => "消息",
            Self::Runs => "运行",
            Self::Approvals => "审批",
            Self::Artifacts => "产物",
            Self::Events => "事件",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Messages => Self::Runs,
            Self::Runs => Self::Approvals,
            Self::Approvals => Self::Artifacts,
            Self::Artifacts => Self::Events,
            Self::Events => Self::Messages,
        }
    }
}

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

struct TuiApp {
    repo_root: PathBuf,
    store: HarnessStore,
    config: crate::config::ProjectConfig,
    threads: Vec<HarnessThreadManifest>,
    selected_thread_id: Option<String>,
    messages: Vec<HarnessMessage>,
    runs: Vec<HarnessRunManifest>,
    events: Vec<HarnessEventRecord>,
    approvals: Vec<crate::harness::ApprovalRecord>,
    artifacts: Vec<ArtifactRecord>,
    detail_tab: DetailTab,
    focus: FocusMode,
    composer: String,
    status: String,
}

impl TuiApp {
    fn new(repo_root: PathBuf, selected_thread_id: Option<String>) -> Result<Self> {
        let config = load_project_config(&repo_root)?.value;
        Ok(Self {
            store: HarnessStore::new(&repo_root),
            repo_root,
            config,
            threads: Vec::new(),
            selected_thread_id,
            messages: Vec::new(),
            runs: Vec::new(),
            events: Vec::new(),
            approvals: Vec::new(),
            artifacts: Vec::new(),
            detail_tab: DetailTab::Messages,
            focus: FocusMode::Browse,
            composer: String::new(),
            status: "i 输入，Enter 发送，a 通过审批，x 拒绝审批，n 新建 thread，Tab 切换视图，q 退出".to_string(),
        })
    }

    fn refresh(&mut self) -> Result<()> {
        self.threads = self.store.list_threads()?;
        if self.threads.is_empty() {
            self.selected_thread_id = None;
            self.messages.clear();
            self.runs.clear();
            self.events.clear();
            self.approvals.clear();
            self.artifacts.clear();
            return Ok(());
        }

        let selected = self
            .selected_thread_id
            .as_ref()
            .and_then(|id| self.threads.iter().position(|thread| thread.id == *id))
            .unwrap_or(0);
        self.selected_thread_id = Some(self.threads[selected].id.clone());
        self.load_selected_thread()
    }

    fn load_selected_thread(&mut self) -> Result<()> {
        let Some(thread_id) = self.selected_thread_id.clone() else {
            self.messages.clear();
            self.runs.clear();
            self.events.clear();
            self.approvals.clear();
            self.artifacts.clear();
            return Ok(());
        };
        self.messages = self.store.list_messages(&thread_id)?;
        self.runs = self.store.list_runs(&thread_id)?;
        self.approvals = self.store.list_pending_approvals(Some(&thread_id))?;
        self.artifacts = self.store.list_artifacts(Some(&thread_id), None)?;
        self.events = if let Some(run) = self.runs.first() {
            self.store.list_run_events(&thread_id, &run.id)?
        } else {
            Vec::new()
        };
        Ok(())
    }

    async fn handle_key(&mut self, code: KeyCode) -> Result<bool> {
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
            KeyCode::Char('a') => {
                self.approve_first_pending().await?;
            }
            KeyCode::Char('x') => {
                self.deny_first_pending().await?;
            }
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
            KeyCode::Enter => {
                self.send_message().await?;
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

    async fn send_message(&mut self) -> Result<()> {
        let message = self.composer.trim().to_string();
        if message.is_empty() {
            self.status = "消息不能为空".to_string();
            return Ok(());
        }
        let thread_id = match self.selected_thread_id.clone() {
            Some(thread_id) => thread_id,
            None => self.store.create_thread(None)?.id,
        };
        self.status = format!("正在向 `{thread_id}` 发送消息...");
        let outcome = chat_once(
            &self.repo_root,
            &self.config,
            ChatRequest {
                thread_id: thread_id.clone(),
                message,
                model: self.config.backend.default_model.clone(),
                thinking_mode: ThinkingMode::Balanced,
            },
        )
        .await?;
        self.selected_thread_id = Some(thread_id.clone());
        self.composer.clear();
        self.focus = FocusMode::Browse;
        self.refresh()?;
        self.status = format!("thread `{thread_id}` 更新完成：{}", outcome.run.id);
        Ok(())
    }

    async fn approve_first_pending(&mut self) -> Result<()> {
        let Some(approval) = self.approvals.first().cloned() else {
            self.status = "当前没有待处理审批".to_string();
            return Ok(());
        };
        let run = resolve_approval_and_resume(
            &self.repo_root,
            &self.config,
            &approval.thread_id,
            &approval.id,
            ApprovalStatus::Approved,
        )
        .await?;
        self.refresh()?;
        self.status = format!("已通过审批 `{}`，run 状态：{}", approval.id, status_label(run.status));
        Ok(())
    }

    async fn deny_first_pending(&mut self) -> Result<()> {
        let Some(approval) = self.approvals.first().cloned() else {
            self.status = "当前没有待处理审批".to_string();
            return Ok(());
        };
        let run = resolve_approval_and_resume(
            &self.repo_root,
            &self.config,
            &approval.thread_id,
            &approval.id,
            ApprovalStatus::Denied,
        )
        .await?;
        self.refresh()?;
        self.status = format!("已拒绝审批 `{}`，run 状态：{}", approval.id, status_label(run.status));
        Ok(())
    }

    fn select_next(&mut self) -> Result<()> {
        if self.threads.is_empty() {
            return Ok(());
        }
        let current = self
            .selected_thread_id
            .as_ref()
            .and_then(|id| self.threads.iter().position(|thread| thread.id == *id))
            .unwrap_or(0);
        let next = (current + 1).min(self.threads.len().saturating_sub(1));
        self.selected_thread_id = Some(self.threads[next].id.clone());
        self.load_selected_thread()
    }

    fn select_prev(&mut self) -> Result<()> {
        if self.threads.is_empty() {
            return Ok(());
        }
        let current = self
            .selected_thread_id
            .as_ref()
            .and_then(|id| self.threads.iter().position(|thread| thread.id == *id))
            .unwrap_or(0);
        let next = current.saturating_sub(1);
        self.selected_thread_id = Some(self.threads[next].id.clone());
        self.load_selected_thread()
    }

    fn render(&self, frame: &mut ratatui::Frame) {
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

        let thread_items = if self.threads.is_empty() {
            vec![ListItem::new("还没有 thread，按 n 创建")]
        } else {
            self.threads
                .iter()
                .map(|thread| {
                    let selected = self
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
                        Span::raw(format!("\n  {} msg / {} run", thread.message_count, thread.run_count)),
                    ]))
                })
                .collect()
        };
        frame.render_widget(
            List::new(thread_items).block(Block::default().borders(Borders::ALL).title("Threads")),
            body[0],
        );

        let center = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(8)])
            .split(body[1]);

        let summary = if let Some(thread) = self
            .selected_thread_id
            .as_ref()
            .and_then(|id| self.threads.iter().find(|thread| &thread.id == id))
        {
            format!(
                "thread={} | 消息={} | runs={} | 待审批={}",
                thread.id,
                thread.message_count,
                thread.run_count,
                self.approvals.len()
            )
        } else {
            "未选中 thread".to_string()
        };
        frame.render_widget(
            Paragraph::new(summary)
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
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .select(match self.detail_tab {
            DetailTab::Messages => 0,
            DetailTab::Runs => 1,
            DetailTab::Approvals => 2,
            DetailTab::Artifacts => 3,
            DetailTab::Events => 4,
        });
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
                .block(Block::default().borders(Borders::ALL).title(match self.focus {
                    FocusMode::Browse => "Composer（按 i 输入）",
                    FocusMode::Compose => "Composer（Enter 发送，Esc 返回）",
                }))
                .wrap(Wrap { trim: false }),
            layout[2],
        );
        frame.render_widget(Paragraph::new(self.status.as_str()), layout[3]);
    }
}

fn render_messages_panel(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, messages: &[HarnessMessage]) {
    let items = messages
        .iter()
        .rev()
        .take(20)
        .rev()
        .map(|message| ListItem::new(format!("[{}] {}", role_label(message.role), message.content.replace('\n', " "))))
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items).block(Block::default().borders(Borders::ALL).title("Messages")), area);
}

fn render_runs_panel(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, runs: &[HarnessRunManifest]) {
    let items = runs
        .iter()
        .map(|run| ListItem::new(format!("{} [{}] {}", run.id, status_label(run.status), run.summary.as_deref().unwrap_or("无摘要"))))
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items).block(Block::default().borders(Borders::ALL).title("Runs")), area);
}

fn render_approvals_panel(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, approvals: &[crate::harness::ApprovalRecord]) {
    let items = approvals
        .iter()
        .map(|approval| ListItem::new(format!("{} [{}] {}: {}", approval.id, approval_status_label(approval.status), approval.tool_name, approval.reason)))
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items).block(Block::default().borders(Borders::ALL).title("Approvals")), area);
}

fn render_artifacts_panel(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, artifacts: &[ArtifactRecord]) {
    let items = artifacts
        .iter()
        .take(20)
        .map(|artifact| ListItem::new(format!("{} [{}] {}", artifact.label, artifact_kind_label(artifact.kind), artifact.path.display())))
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items).block(Block::default().borders(Borders::ALL).title("Artifacts")), area);
}

fn render_events_panel(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, events: &[HarnessEventRecord]) {
    let items = events
        .iter()
        .rev()
        .take(20)
        .rev()
        .map(|event| ListItem::new(format!("{} {}", event.at.format("%H:%M:%S"), describe_event(&event.payload))))
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items).block(Block::default().borders(Borders::ALL).title("Events")), area);
}
