use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table, Wrap};

use crate::model::{FinalSummary, RuntimeEvent, UiMode, WorkerResult, WorkerStatus};

#[derive(Debug, Clone)]
struct WorkerView {
    role: String,
    title: String,
    status: WorkerStatus,
    last_event: String,
    worktree_path: String,
}

#[derive(Debug, Clone)]
struct UiState {
    session_id: String,
    task: String,
    phase: String,
    started_at: Instant,
    commander_notes: Vec<String>,
    workers: BTreeMap<String, WorkerView>,
    summary: Option<FinalSummary>,
    graph_summary: Option<String>,
    apply_status: Option<String>,
    verify_status: Option<String>,
}

impl UiState {
    fn new(session_id: &str, task: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            task: task.to_string(),
            phase: "初始化".to_string(),
            started_at: Instant::now(),
            commander_notes: Vec::new(),
            workers: BTreeMap::new(),
            summary: None,
            graph_summary: None,
            apply_status: None,
            verify_status: None,
        }
    }

    fn apply(&mut self, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::PhaseChanged { phase } => {
                self.phase = phase.clone();
            }
            RuntimeEvent::CommanderNote { message } => {
                push_note(&mut self.commander_notes, message)
            }
            RuntimeEvent::GraphReady {
                nodes,
                dependencies,
            } => {
                self.graph_summary = Some(format!("节点 {} / 依赖 {}", nodes, dependencies));
            }
            RuntimeEvent::WorkerDispatched {
                agent_id,
                role,
                title,
                worktree_path,
            } => {
                self.workers.insert(
                    agent_id.clone(),
                    WorkerView {
                        role: role.clone(),
                        title: title.clone(),
                        status: WorkerStatus::Running,
                        last_event: "已下发任务".to_string(),
                        worktree_path: worktree_path.display().to_string(),
                    },
                );
            }
            RuntimeEvent::WorkerUpdate {
                agent_id,
                kind,
                message,
            } => {
                if let Some(worker) = self.workers.get_mut(agent_id) {
                    worker.last_event = format!("{kind}: {}", truncate(message, 72));
                }
            }
            RuntimeEvent::HandoffReady {
                agent_id,
                handoff_path,
            } => {
                if let Some(worker) = self.workers.get_mut(agent_id) {
                    worker.last_event = format!("handoff: {}", handoff_path.display());
                }
            }
            RuntimeEvent::WorkerFinished { result } => {
                upsert_result(&mut self.workers, result);
            }
            RuntimeEvent::ApplyPlanReady { mode, operations } => {
                self.apply_status = Some(format!("计划：{} / {} 个 patch", mode, operations));
            }
            RuntimeEvent::ApplyUpdate { message } => {
                self.apply_status = Some(message.clone());
            }
            RuntimeEvent::VerificationReady {
                stage,
                success,
                message,
            } => {
                self.verify_status = Some(format!(
                    "{} / {} / {}",
                    stage,
                    if *success { "成功" } else { "失败" },
                    truncate(message, 60)
                ));
            }
            RuntimeEvent::SummaryReady { summary } => {
                self.summary = Some(summary.as_ref().clone());
            }
        }
    }
}

pub struct UiController {
    state: UiState,
    backend: UiBackend,
}

enum UiBackend {
    Rich(RichTerminal),
    Plain,
}

struct RichTerminal {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    last_draw: Instant,
}

impl Drop for RichTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

impl UiController {
    pub fn new(session_id: &str, task: &str, ui_mode: UiMode) -> Result<Self> {
        let state = UiState::new(session_id, task);
        let backend = match ui_mode {
            UiMode::Rich if io::stdout().is_terminal() => {
                enable_raw_mode()?;
                execute!(io::stdout(), EnterAlternateScreen)?;
                let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
                UiBackend::Rich(RichTerminal {
                    terminal,
                    last_draw: Instant::now() - Duration::from_secs(1),
                })
            }
            _ => UiBackend::Plain,
        };

        Ok(Self { state, backend })
    }

    pub fn apply(&mut self, event: &RuntimeEvent) -> Result<()> {
        self.state.apply(event);
        match &mut self.backend {
            UiBackend::Rich(rich) => {
                if rich.last_draw.elapsed() >= Duration::from_millis(90)
                    || matches!(
                        event,
                        RuntimeEvent::WorkerFinished { .. }
                            | RuntimeEvent::SummaryReady { .. }
                            | RuntimeEvent::VerificationReady { .. }
                    )
                {
                    render_rich(rich, &self.state)?;
                }
            }
            UiBackend::Plain => render_plain(event),
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        if let UiBackend::Rich(rich) = &mut self.backend {
            render_rich(rich, &self.state)?;
        }
        Ok(())
    }
}

fn render_rich(rich: &mut RichTerminal, state: &UiState) -> Result<()> {
    rich.terminal.draw(|frame| {
        let area = frame.area();
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5),
                Constraint::Min(10),
                Constraint::Length(8),
                Constraint::Length(6),
            ])
            .split(area);

        let success = state
            .workers
            .values()
            .filter(|worker| worker.status == WorkerStatus::Succeeded)
            .count();
        let failed = state
            .workers
            .values()
            .filter(|worker| matches!(worker.status, WorkerStatus::Failed | WorkerStatus::Skipped))
            .count();
        let running = state
            .workers
            .values()
            .filter(|worker| worker.status == WorkerStatus::Running)
            .count();

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "◢ CODEX-FORGE V2 ◣",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("Session {}", state.session_id),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::from(format!("任务：{}", truncate(&state.task, 100))),
            Line::from(format!(
                "阶段：{}   运行中：{}   成功：{}   失败/跳过：{}   已耗时：{}s",
                state.phase,
                running,
                success,
                failed,
                state.started_at.elapsed().as_secs()
            )),
            Line::from(format!(
                "执行图：{}   应用：{}   验证：{}",
                state
                    .graph_summary
                    .clone()
                    .unwrap_or_else(|| "等待".to_string()),
                state
                    .apply_status
                    .clone()
                    .unwrap_or_else(|| "等待".to_string()),
                state
                    .verify_status
                    .clone()
                    .unwrap_or_else(|| "等待".to_string())
            )),
        ])
        .block(Block::default().title("总览").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
        frame.render_widget(header, sections[0]);

        let worker_rows = state
            .workers
            .iter()
            .map(|(agent_id, worker)| {
                Row::new(vec![
                    Cell::from(agent_id.clone()),
                    Cell::from(worker.role.clone()),
                    Cell::from(worker.status.label()),
                    Cell::from(truncate(&worker.title, 24)),
                    Cell::from(truncate(&worker.last_event, 44)),
                ])
                .style(style_for_status(worker.status))
            })
            .collect::<Vec<_>>();

        let workers_table = Table::new(
            worker_rows,
            [
                Constraint::Length(14),
                Constraint::Length(12),
                Constraint::Length(10),
                Constraint::Length(24),
                Constraint::Min(20),
            ],
        )
        .header(
            Row::new(vec!["Agent", "角色", "状态", "任务", "最新事件"]).style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(Block::default().title("Worker 矩阵").borders(Borders::ALL))
        .column_spacing(1);
        frame.render_widget(workers_table, sections[1]);

        let note_items = state
            .commander_notes
            .iter()
            .rev()
            .map(|note| ListItem::new(Line::from(Span::raw(note.clone()))))
            .collect::<Vec<_>>();
        let notes = List::new(note_items).block(
            Block::default()
                .title("Commander 决策流")
                .borders(Borders::ALL),
        );
        frame.render_widget(notes, sections[2]);

        let summary_text = if let Some(summary) = &state.summary {
            vec![
                Line::from(Span::styled(
                    "最终收敛摘要",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(summary.overview.clone()),
                Line::from(format!(
                    "接收文件：{}",
                    if summary.accepted_files.is_empty() {
                        "无".to_string()
                    } else {
                        truncate(&summary.accepted_files.join("；"), 120)
                    }
                )),
                Line::from(format!(
                    "人工复核：{}",
                    if summary.manual_review_files.is_empty() {
                        "无".to_string()
                    } else {
                        truncate(&summary.manual_review_files.join("；"), 120)
                    }
                )),
                Line::from(format!(
                    "风险：{}",
                    if summary.open_risks.is_empty() {
                        "无".to_string()
                    } else {
                        truncate(&summary.open_risks.join("；"), 120)
                    }
                )),
            ]
        } else if let Some(worker) = state.workers.values().last() {
            vec![
                Line::from("等待最终收敛摘要…"),
                Line::from(format!(
                    "最近 worktree：{}",
                    truncate(&worker.worktree_path, 100)
                )),
            ]
        } else {
            vec![Line::from("等待 worker 启动…")]
        };
        let footer = Paragraph::new(summary_text)
            .block(Block::default().title("状态面板").borders(Borders::ALL))
            .wrap(Wrap { trim: true });
        frame.render_widget(footer, sections[3]);
    })?;
    rich.last_draw = Instant::now();
    Ok(())
}

fn render_plain(event: &RuntimeEvent) {
    match event {
        RuntimeEvent::PhaseChanged { phase } => {
            println!("== 阶段切换：{phase}");
        }
        RuntimeEvent::CommanderNote { message } => {
            println!("🧠 {message}");
        }
        RuntimeEvent::GraphReady {
            nodes,
            dependencies,
        } => {
            println!("🕸️ 执行图就绪：节点 {nodes} / 依赖 {dependencies}");
        }
        RuntimeEvent::WorkerDispatched {
            agent_id,
            role,
            title,
            worktree_path,
        } => {
            println!(
                "🚀 {agent_id} / {role} 已启动：{title} ({})",
                worktree_path.display()
            );
        }
        RuntimeEvent::WorkerUpdate {
            agent_id,
            kind,
            message,
        } => {
            if kind.contains("error") || kind.contains("stderr") || kind == "retry" {
                println!("⚠️  {agent_id} [{kind}] {}", truncate(message, 120));
            }
        }
        RuntimeEvent::HandoffReady {
            agent_id,
            handoff_path,
        } => {
            println!("📦 {agent_id} handoff 就绪：{}", handoff_path.display());
        }
        RuntimeEvent::WorkerFinished { result } => {
            println!(
                "✅ {} 完成：{}（状态：{}）",
                result.agent_id,
                result.task_title,
                result.status.label()
            );
        }
        RuntimeEvent::ApplyPlanReady { mode, operations } => {
            println!("🧩 apply 计划：模式={}，候选 patch={operations}", mode);
        }
        RuntimeEvent::ApplyUpdate { message } => {
            println!("🪄 {message}");
        }
        RuntimeEvent::VerificationReady {
            stage,
            success,
            message,
        } => {
            println!(
                "🧪 {}：{} - {}",
                stage,
                if *success { "成功" } else { "失败" },
                message
            );
        }
        RuntimeEvent::SummaryReady { summary } => {
            println!("📦 最终摘要：{}", summary.overview);
        }
    }
}

fn upsert_result(workers: &mut BTreeMap<String, WorkerView>, result: &WorkerResult) {
    workers.insert(
        result.agent_id.clone(),
        WorkerView {
            role: result.role.clone(),
            title: result.task_title.clone(),
            status: result.status,
            last_event: result
                .summary
                .clone()
                .or_else(|| result.diagnostic_summary.clone())
                .unwrap_or_else(|| truncate(&result.final_message, 72)),
            worktree_path: result.worktree_path.display().to_string(),
        },
    );
}

fn style_for_status(status: WorkerStatus) -> Style {
    match status {
        WorkerStatus::Pending => Style::default().fg(Color::Gray),
        WorkerStatus::Running => Style::default().fg(Color::Cyan),
        WorkerStatus::Succeeded => Style::default().fg(Color::Green),
        WorkerStatus::Failed => Style::default().fg(Color::Red),
        WorkerStatus::Skipped => Style::default().fg(Color::Yellow),
    }
}

fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.replace('\n', " ");
    if trimmed.chars().count() <= max {
        trimmed
    } else {
        format!("{}…", trimmed.chars().take(max).collect::<String>())
    }
}

fn push_note(notes: &mut Vec<String>, note: &str) {
    notes.push(note.to_string());
    if notes.len() > 8 {
        let overflow = notes.len() - 8;
        notes.drain(0..overflow);
    }
}
