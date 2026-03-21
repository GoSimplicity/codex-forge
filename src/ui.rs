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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table, Wrap};

use crate::model::{
    FinalSummary, ReviewGateReport, RuntimeEvent, TodoStatus, UiMode, WorkerResult, WorkerStatus,
};

/// Dashboard 里展示的单个 worker 视图状态。
#[derive(Debug, Clone)]
pub struct WorkerView {
    pub role: String,
    pub title: String,
    pub status: WorkerStatus,
    pub last_event: String,
    pub worktree_path: String,
}

/// 运行态共享视图模型。
/// 旧 CLI Rich UI 和 v6 AppShell 都复用这份状态，避免出现两套运行态解释逻辑。
#[derive(Debug, Clone)]
pub struct RuntimeViewState {
    pub session_id: String,
    pub task: String,
    pub phase: String,
    pub started_at: Instant,
    pub commander_notes: Vec<String>,
    pub workers: BTreeMap<String, WorkerView>,
    pub todos: BTreeMap<String, (String, TodoStatus, String)>,
    pub review_report: Option<ReviewGateReport>,
    pub summary: Option<FinalSummary>,
    pub graph_summary: Option<String>,
    pub apply_status: Option<String>,
    pub verify_status: Option<String>,
}

impl RuntimeViewState {
    pub fn new(session_id: &str, task: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            task: task.to_string(),
            phase: "初始化".to_string(),
            started_at: Instant::now(),
            commander_notes: Vec::new(),
            workers: BTreeMap::new(),
            todos: BTreeMap::new(),
            review_report: None,
            summary: None,
            graph_summary: None,
            apply_status: None,
            verify_status: None,
        }
    }

    pub fn set_identity(&mut self, session_id: impl Into<String>, task: impl Into<String>) {
        self.session_id = session_id.into();
        self.task = task.into();
    }

    pub fn apply(&mut self, event: &RuntimeEvent) {
        // 所有 RuntimeEvent 都在这里被折叠成“终端可渲染的稳定状态”。
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
            RuntimeEvent::TodoStateChanged {
                todo_id,
                title,
                status,
                message,
                commit_hash,
            } => {
                self.todos
                    .insert(todo_id.clone(), (title.clone(), *status, message.clone()));
                let suffix = commit_hash
                    .as_ref()
                    .map(|hash| format!(" / commit {}", truncate(hash, 12)))
                    .unwrap_or_default();
                push_note(
                    &mut self.commander_notes,
                    &format!(
                        "{} {} -> {} / {}{}",
                        todo_id,
                        title,
                        status.label(),
                        truncate(message, 48),
                        suffix
                    ),
                );
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
                if let Some(note) = summarize_worker_update(kind, message) {
                    push_note(&mut self.commander_notes, &format!("{agent_id} / {note}"));
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
            RuntimeEvent::ReviewGateReady { report } => {
                self.review_report = Some(report.as_ref().clone());
                self.apply_status = Some(format!("review gate：{}", report.decision.label()));
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
    state: RuntimeViewState,
    backend: UiBackend,
}

enum UiBackend {
    Rich(RichTerminal),
    Plain,
    Silent,
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
        let state = RuntimeViewState::new(session_id, task);
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

    pub fn silent(session_id: &str, task: &str) -> Self {
        Self {
            state: RuntimeViewState::new(session_id, task),
            backend: UiBackend::Silent,
        }
    }

    pub fn apply(&mut self, event: &RuntimeEvent) -> Result<()> {
        self.state.apply(event);
        match &mut self.backend {
            UiBackend::Rich(rich) => {
                // Rich 模式做一点节流，避免高频 worker 事件把终端刷爆；
                // 但关键节点完成、验证完成、summary 完成时仍然立即刷新。
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
            UiBackend::Silent => {}
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

pub fn render_runtime_dashboard(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &RuntimeViewState,
    title: &str,
) {
    if area.width < 100 || area.height < 24 {
        render_runtime_dashboard_compact(frame, area, state, title);
        return;
    }

    // 这个布局是 v6 执行页主视图，也是旧 Rich UI 的基础信息源：
    // 顶部总览 / 中间 worker+todo / 下方 commander notes / 底部状态面板。
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
                "◢ CODEX-FORGE V6 ◣",
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
    .block(Block::default().title(title).borders(Borders::ALL))
    .wrap(Wrap { trim: true });
    frame.render_widget(header, sections[0]);

    let middle_sections = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(sections[1]);

    if state.workers.is_empty() {
        frame.render_widget(
            Paragraph::new(planning_activity_lines(state, false))
                .block(Block::default().title("规划动态").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            middle_sections[0],
        );
    } else {
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
        frame.render_widget(workers_table, middle_sections[0]);
    }

    let todo_items = state
        .todos
        .iter()
        .map(|(todo_id, (title, status, message))| {
            ListItem::new(Line::from(format!(
                "{} {} / {} / {}",
                todo_id,
                truncate(title, 18),
                status.label(),
                truncate(message, 22)
            )))
        })
        .collect::<Vec<_>>();
    let review_lines = if let Some(report) = &state.review_report {
        vec![
            Line::from(format!("结论：{}", report.decision.label())),
            Line::from(format!(
                "说明：{}",
                truncate(
                    report
                        .confidence_reasoning
                        .as_deref()
                        .unwrap_or("无补充说明"),
                    44
                )
            )),
            Line::from(format!(
                "阻断项：{}",
                truncate(&report.blocking_findings.join("；"), 44)
            )),
        ]
    } else {
        vec![
            Line::from("结论：等待 reviewer"),
            Line::from("说明：尚未形成 gate"),
            Line::from("阻断项：无"),
        ]
    };
    let right_sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(middle_sections[1]);
    frame.render_widget(
        List::new(todo_items).block(Block::default().title("Todo 态势").borders(Borders::ALL)),
        right_sections[0],
    );
    frame.render_widget(
        Paragraph::new(review_lines)
            .block(Block::default().title("Review Gate").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        right_sections[1],
    );

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
}

fn render_runtime_dashboard_compact(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &RuntimeViewState,
    title: &str,
) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints(compact_dashboard_constraints(area))
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

    let header_lines = if area.width < 72 {
        vec![
            Line::from(vec![
                Span::styled(
                    "◢ CF V6 ◣",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    truncate(&state.session_id, 16),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::from(format!(
                "{} | 运行{} 成功{} 失败{}",
                truncate(&state.phase, 10),
                running,
                success,
                failed
            )),
            Line::from(format!("任务：{}", truncate(&state.task, 36))),
        ]
    } else {
        vec![
            Line::from(vec![
                Span::styled(
                    "◢ CODEX-FORGE V6 ◣",
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
            Line::from(format!("任务：{}", truncate(&state.task, 72))),
            Line::from(format!(
                "阶段：{}   运行中：{}   成功：{}   失败/跳过：{}   {}s",
                state.phase,
                running,
                success,
                failed,
                state.started_at.elapsed().as_secs()
            )),
        ]
    };
    frame.render_widget(
        Paragraph::new(header_lines)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        sections[0],
    );

    if state.workers.is_empty() {
        frame.render_widget(
            Paragraph::new(planning_activity_lines(state, true))
                .block(Block::default().title("规划动态").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            sections[1],
        );
    } else {
        let worker_items = state
            .workers
            .iter()
            .take(if area.height < 20 { 4 } else { 6 })
            .map(|(agent_id, worker)| {
                ListItem::new(Line::from(format!(
                    "{} {} / {} / {}",
                    truncate(agent_id, 10),
                    truncate(&worker.role, 8),
                    worker.status.label(),
                    truncate(&worker.last_event, if area.width < 72 { 20 } else { 42 })
                )))
                .style(style_for_status(worker.status))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(worker_items).block(Block::default().title("Workers").borders(Borders::ALL)),
            sections[1],
        );
    }

    let status_lines = compact_status_lines(state, area.width);
    frame.render_widget(
        Paragraph::new(status_lines)
            .block(
                Block::default()
                    .title("Todo / Gate / 状态")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true }),
        sections[2],
    );

    if sections.len() > 3 {
        let summary_lines = compact_summary_lines(state, area.width);
        frame.render_widget(
            Paragraph::new(summary_lines)
                .block(Block::default().title("摘要").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            sections[3],
        );
    }
}

fn compact_dashboard_constraints(area: Rect) -> Vec<Constraint> {
    if area.height < 18 {
        vec![
            Constraint::Length(5),
            Constraint::Min(4),
            Constraint::Min(4),
        ]
    } else {
        vec![
            Constraint::Length(5),
            Constraint::Min(5),
            Constraint::Length(6),
            Constraint::Min(4),
        ]
    }
}

fn compact_status_lines(state: &RuntimeViewState, width: u16) -> Vec<Line<'static>> {
    let todo_limit = if width < 72 { 2 } else { 3 };
    let mut lines = vec![
        Line::from(format!(
            "执行图：{}",
            state
                .graph_summary
                .clone()
                .unwrap_or_else(|| "等待".to_string())
        )),
        Line::from(format!(
            "应用：{}",
            truncate(
                &state
                    .apply_status
                    .clone()
                    .unwrap_or_else(|| "等待".to_string()),
                if width < 72 { 24 } else { 48 }
            )
        )),
        Line::from(format!(
            "验证：{}",
            truncate(
                &state
                    .verify_status
                    .clone()
                    .unwrap_or_else(|| "等待".to_string()),
                if width < 72 { 24 } else { 48 }
            )
        )),
    ];
    for (todo_id, (title, status, message)) in state.todos.iter().take(todo_limit) {
        lines.push(Line::from(format!(
            "{} {} / {}",
            todo_id,
            truncate(title, if width < 72 { 10 } else { 18 }),
            truncate(
                &format!("{} / {}", status.label(), message),
                if width < 72 { 20 } else { 42 }
            )
        )));
    }
    if let Some(report) = &state.review_report {
        lines.push(Line::from(format!(
            "Gate：{} / {}",
            report.decision.label(),
            truncate(
                report
                    .confidence_reasoning
                    .as_deref()
                    .unwrap_or("无补充说明"),
                if width < 72 { 18 } else { 36 }
            )
        )));
    }
    lines
}

fn planning_activity_lines(state: &RuntimeViewState, compact: bool) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(format!("当前阶段：{}", state.phase)),
        Line::from(format!(
            "执行图：{}",
            state
                .graph_summary
                .clone()
                .unwrap_or_else(|| "尚未生成".to_string())
        )),
    ];

    if state.todos.is_empty() {
        lines.push(Line::from("Todo：等待规划结果…"));
    } else {
        let todo_limit = if compact { 3 } else { 6 };
        for (todo_id, (title, status, message)) in state.todos.iter().take(todo_limit) {
            lines.push(Line::from(format!(
                "{} {} / {} / {}",
                todo_id,
                truncate(title, if compact { 10 } else { 20 }),
                status.label(),
                truncate(message, if compact { 18 } else { 42 })
            )));
        }
    }

    if state.commander_notes.is_empty() {
        lines.push(Line::from("指挥动态：等待更多规划事件…"));
    } else {
        let note_limit = if compact { 2 } else { 4 };
        for note in state.commander_notes.iter().rev().take(note_limit).rev() {
            lines.push(Line::from(format!(
                "指挥：{}",
                truncate(note, if compact { 28 } else { 72 })
            )));
        }
    }

    lines
}

fn compact_summary_lines(state: &RuntimeViewState, width: u16) -> Vec<Line<'static>> {
    if let Some(summary) = &state.summary {
        vec![
            Line::from(summary.overview.clone()),
            Line::from(format!(
                "结果：{} / Apply：{}",
                summary.result_status.label(),
                summary.apply_status.label()
            )),
            Line::from(format!(
                "风险：{}",
                truncate(
                    &summary.open_risks.join("；"),
                    if width < 72 { 26 } else { 56 }
                )
            )),
        ]
    } else if let Some(note) = state.commander_notes.last() {
        vec![
            Line::from("等待最终摘要…"),
            Line::from(format!(
                "最近决策：{}",
                truncate(note, if width < 72 { 26 } else { 56 })
            )),
        ]
    } else {
        vec![Line::from("等待更多运行事件…")]
    }
}

pub fn describe_runtime_event(event: &RuntimeEvent) -> String {
    match event {
        RuntimeEvent::PhaseChanged { phase } => format!("阶段切换 -> {phase}"),
        RuntimeEvent::CommanderNote { message } => format!("指挥备注：{message}"),
        RuntimeEvent::GraphReady {
            nodes,
            dependencies,
        } => format!("执行图就绪：节点 {nodes} / 依赖 {dependencies}"),
        RuntimeEvent::TodoStateChanged {
            todo_id,
            title,
            status,
            message,
            ..
        } => format!("{todo_id} {title} -> {} / {message}", status.label()),
        RuntimeEvent::WorkerDispatched {
            agent_id,
            role,
            title,
            ..
        } => format!("启动 {agent_id} / {role} / {title}"),
        RuntimeEvent::WorkerUpdate {
            agent_id,
            kind,
            message,
        } => format!("{agent_id} [{kind}] {message}"),
        RuntimeEvent::HandoffReady {
            agent_id,
            handoff_path,
        } => format!("交接就绪 {agent_id} -> {}", handoff_path.display()),
        RuntimeEvent::WorkerFinished { result } => {
            format!("{} 完成：{}", result.agent_id, result.status.label())
        }
        RuntimeEvent::ApplyPlanReady { mode, operations } => {
            format!("apply 计划：{} / {} 个 patch", mode, operations)
        }
        RuntimeEvent::ReviewGateReady { report } => format!(
            "review gate：{} / {}",
            report.decision.label(),
            report
                .confidence_reasoning
                .clone()
                .unwrap_or_else(|| "无补充说明".to_string())
        ),
        RuntimeEvent::ApplyUpdate { message } => format!("应用更新：{message}"),
        RuntimeEvent::VerificationReady {
            stage,
            success,
            message,
        } => format!(
            "验证 {stage}：{} / {message}",
            if *success { "成功" } else { "失败" }
        ),
        RuntimeEvent::SummaryReady { summary } => format!("总结：{}", summary.overview),
    }
}

fn render_rich(rich: &mut RichTerminal, state: &RuntimeViewState) -> Result<()> {
    rich.terminal.draw(|frame| {
        render_runtime_dashboard(frame, frame.area(), state, "总览");
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
        RuntimeEvent::TodoStateChanged {
            todo_id,
            title,
            status,
            message,
            commit_hash,
        } => {
            if let Some(hash) = commit_hash {
                println!(
                    "📝 {todo_id} {title}：{} - {} ({})",
                    status.label(),
                    message,
                    truncate(hash, 12)
                );
            } else {
                println!("📝 {todo_id} {title}：{} - {}", status.label(), message);
            }
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
        RuntimeEvent::ReviewGateReady { report } => {
            println!(
                "🛡️ review gate：{} - {}",
                report.decision.label(),
                report
                    .confidence_reasoning
                    .clone()
                    .unwrap_or_else(|| "无补充说明".to_string())
            );
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

fn summarize_worker_update(kind: &str, message: &str) -> Option<String> {
    if message.trim().is_empty() {
        return None;
    }
    if matches!(kind, "raw" | "stderr:raw" | "stdout:raw" | "empty") {
        return None;
    }
    if kind.starts_with("stderr:") {
        return Some(format!("stderr：{}", truncate(message, 56)));
    }
    if matches!(
        kind,
        "item.started"
            | "item.completed"
            | "item.updated"
            | "turn.started"
            | "turn.completed"
            | "thread.started"
    ) {
        return Some(truncate(message, 56));
    }
    if kind.contains("error") || kind == "retry" {
        return Some(format!("{kind} / {}", truncate(message, 56)));
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    use super::{
        RuntimeViewState, compact_status_lines, compact_summary_lines, describe_runtime_event,
        planning_activity_lines, render_runtime_dashboard,
    };
    use crate::model::{
        ApplyDecision, ApplyStatus, FinalSummary, ResultStatus, ReviewGateReport, RuntimeEvent,
        TodoStatus, TrustLevel, WorkerResult, WorkerStatus,
    };

    fn sample_review_report(decision: ApplyDecision) -> ReviewGateReport {
        ReviewGateReport {
            decision,
            blocking_findings: vec!["存在阻断项".to_string()],
            accepted_scopes: vec!["src/ui.rs".to_string()],
            rejected_scopes: vec!["README.md".to_string()],
            confidence_reasoning: Some("证据充分".to_string()),
        }
    }

    fn sample_summary(overview: &str) -> FinalSummary {
        FinalSummary {
            overview: overview.to_string(),
            result_status: ResultStatus::Completed,
            review_gate: Some(ApplyDecision::AllowFull),
            apply_status: ApplyStatus::Applied,
            trust_level: TrustLevel::High,
            accepted_files: vec!["src/ui.rs".to_string()],
            manual_review_files: vec!["tests/ui.rs".to_string()],
            rejected_files: Vec::new(),
            verified_capabilities: vec!["cargo test".to_string()],
            blocked_verifications: Vec::new(),
            open_risks: vec!["需要补一条真实终端回归".to_string()],
            recommended_next_action: vec!["补充冒烟".to_string()],
            todo_states: Vec::new(),
            used_fallback: false,
            review_report: Some(sample_review_report(ApplyDecision::AllowFull)),
            evidence_summary: vec!["单测通过".to_string()],
            iteration_index: 1,
            based_on_session_id: None,
            feedback_summary: Vec::new(),
            delta_summary: Vec::new(),
            completed_this_iteration: vec!["补测完成".to_string()],
            unaccepted_feedback: Vec::new(),
        }
    }

    fn sample_worker_result(status: WorkerStatus) -> WorkerResult {
        WorkerResult {
            agent_id: "implementer-1".to_string(),
            role: "implementer".to_string(),
            task_title: "补齐 TUI".to_string(),
            status,
            exit_code: Some(0),
            attempts: 1,
            diagnostic_summary: Some("diagnostic".to_string()),
            final_message: "final message".to_string(),
            summary: Some("实现完成".to_string()),
            changed_files: vec!["src/ui.rs".to_string()],
            worktree_path: PathBuf::from("/tmp/demo-worktree"),
            prompt_path: PathBuf::from("/tmp/prompt.md"),
            stdout_path: PathBuf::from("/tmp/stdout.log"),
            stderr_path: PathBuf::from("/tmp/stderr.log"),
            events_path: PathBuf::from("/tmp/events.jsonl"),
            final_output_path: PathBuf::from("/tmp/final.md"),
            diff_path: None,
            git_status_path: None,
            handoff_path: None,
            handoff: None,
            error: None,
        }
    }

    fn render_to_text(state: &RuntimeViewState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                render_runtime_dashboard(frame, frame.area(), state, "测试态势");
            })
            .expect("draw dashboard");
        buffer_to_text(terminal.backend().buffer())
    }

    fn buffer_to_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn normalize_rendered_text(text: &str) -> String {
        text.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    #[test]
    fn planning_activity_lines_show_todos_and_notes_without_workers() {
        let mut state = RuntimeViewState::new("session-1", "规划一个任务");
        state.apply(&RuntimeEvent::PhaseChanged {
            phase: "规划清单已生成".to_string(),
        });
        state.apply(&RuntimeEvent::CommanderNote {
            message: "计划清单已生成，共 2 项 todo。".to_string(),
        });
        state.apply(&RuntimeEvent::TodoStateChanged {
            todo_id: "todo-1".to_string(),
            title: "修复 TUI".to_string(),
            status: TodoStatus::Pending,
            message: "规划完成，等待调度".to_string(),
            commit_hash: None,
        });

        let lines = planning_activity_lines(&state, true);
        let rendered = lines
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("规划清单已生成"));
        assert!(rendered.contains("todo-1"));
        assert!(rendered.contains("计划清单已生成，共 2 项 todo。"));
    }

    #[test]
    fn worker_updates_surface_into_planning_notes() {
        let mut state = RuntimeViewState::new("session-2", "继续优化");
        state.apply(&RuntimeEvent::WorkerDispatched {
            agent_id: "implementer-1".to_string(),
            role: "implementer".to_string(),
            title: "实现交互".to_string(),
            worktree_path: "/tmp/demo".into(),
        });
        state.apply(&RuntimeEvent::WorkerUpdate {
            agent_id: "implementer-1".to_string(),
            kind: "item.completed".to_string(),
            message: "命令完成：cargo test -q / 退出码 0 / ok".to_string(),
        });

        let rendered = planning_activity_lines(&state, false)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("implementer-1"));
        assert!(rendered.contains("命令完成"));
    }

    #[test]
    fn runtime_view_state_tracks_full_event_flow_and_limits_notes() {
        let mut state = RuntimeViewState::new("session-3", "覆盖全部事件");

        for index in 0..10 {
            state.apply(&RuntimeEvent::CommanderNote {
                message: format!("note-{index}"),
            });
        }
        assert_eq!(state.commander_notes.len(), 8);
        assert_eq!(
            state.commander_notes.first().map(String::as_str),
            Some("note-2")
        );

        state.apply(&RuntimeEvent::GraphReady {
            nodes: 3,
            dependencies: 2,
        });
        state.apply(&RuntimeEvent::TodoStateChanged {
            todo_id: "todo-1".to_string(),
            title: "补齐紧凑布局".to_string(),
            status: TodoStatus::Running,
            message: "开始处理".to_string(),
            commit_hash: Some("1234567890abcdef".to_string()),
        });
        state.apply(&RuntimeEvent::WorkerDispatched {
            agent_id: "implementer-1".to_string(),
            role: "implementer".to_string(),
            title: "补齐 TUI".to_string(),
            worktree_path: "/tmp/demo".into(),
        });
        state.apply(&RuntimeEvent::WorkerUpdate {
            agent_id: "implementer-1".to_string(),
            kind: "stderr:error".to_string(),
            message: "发现一个渲染边界".to_string(),
        });
        state.apply(&RuntimeEvent::HandoffReady {
            agent_id: "implementer-1".to_string(),
            handoff_path: "/tmp/demo/handoff.md".into(),
        });
        state.apply(&RuntimeEvent::WorkerFinished {
            result: Box::new(sample_worker_result(WorkerStatus::Succeeded)),
        });
        state.apply(&RuntimeEvent::ApplyPlanReady {
            mode: crate::model::ApplyMode::AutoSafe,
            operations: 2,
        });
        state.apply(&RuntimeEvent::ReviewGateReady {
            report: Box::new(sample_review_report(ApplyDecision::AllowPartial)),
        });
        state.apply(&RuntimeEvent::ApplyUpdate {
            message: "集成完成".to_string(),
        });
        state.apply(&RuntimeEvent::VerificationReady {
            stage: "integration".to_string(),
            success: false,
            message: "存在一条失败验证".to_string(),
        });
        state.apply(&RuntimeEvent::SummaryReady {
            summary: Box::new(sample_summary("TUI 已稳定")),
        });

        assert_eq!(state.graph_summary.as_deref(), Some("节点 3 / 依赖 2"));
        assert_eq!(
            state.todos.get("todo-1").map(|item| item.1),
            Some(TodoStatus::Running)
        );
        assert_eq!(
            state
                .workers
                .get("implementer-1")
                .map(|worker| worker.status),
            Some(WorkerStatus::Succeeded)
        );
        assert_eq!(
            state
                .workers
                .get("implementer-1")
                .map(|worker| worker.last_event.as_str()),
            Some("实现完成")
        );
        assert_eq!(state.apply_status.as_deref(), Some("集成完成"));
        assert_eq!(
            state.verify_status.as_deref(),
            Some("integration / 失败 / 存在一条失败验证")
        );
        assert_eq!(
            state
                .summary
                .as_ref()
                .map(|summary| summary.overview.as_str()),
            Some("TUI 已稳定")
        );
        assert!(
            state
                .commander_notes
                .iter()
                .any(|note| note.contains("stderr：发现一个渲染边界"))
        );
    }

    #[test]
    fn compact_helpers_and_event_descriptions_cover_review_and_summary_fallbacks() {
        let mut state = RuntimeViewState::new("session-4", "紧凑模式");
        state.graph_summary = Some("节点 2 / 依赖 1".to_string());
        state.apply_status = Some("等待 apply".to_string());
        state.verify_status = Some("等待 verify".to_string());
        state.commander_notes.push("最近一次决策".to_string());
        state.todos.insert(
            "todo-1".to_string(),
            (
                "补齐摘要".to_string(),
                TodoStatus::NeedsManualFollowup,
                "需要人工复核".to_string(),
            ),
        );
        state.review_report = Some(sample_review_report(ApplyDecision::Block));

        let compact = compact_status_lines(&state, 60)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(compact.contains("todo-1"));
        assert!(compact.contains("Gate：阻止应用 / 证据充分"));

        let fallback_summary = compact_summary_lines(&state, 60)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(fallback_summary.contains("等待最终摘要"));
        assert!(fallback_summary.contains("最近一次决策"));

        state.summary = Some(sample_summary("最终摘要可见"));
        let ready_summary = compact_summary_lines(&state, 80)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(ready_summary.contains("最终摘要可见"));
        assert!(ready_summary.contains("结果：完成 / Apply：已应用"));

        let event = RuntimeEvent::ReviewGateReady {
            report: Box::new(sample_review_report(ApplyDecision::AllowFull)),
        };
        let rendered = describe_runtime_event(&event);
        assert!(rendered.contains("review gate：全部放行"));
        assert!(rendered.contains("证据充分"));
    }

    #[test]
    fn render_runtime_dashboard_shows_wide_worker_matrix_and_summary() {
        let mut state = RuntimeViewState::new("session-5", "宽屏渲染");
        state.apply(&RuntimeEvent::WorkerDispatched {
            agent_id: "implementer-1".to_string(),
            role: "implementer".to_string(),
            title: "补齐摘要".to_string(),
            worktree_path: "/tmp/demo".into(),
        });
        state.apply(&RuntimeEvent::TodoStateChanged {
            todo_id: "todo-1".to_string(),
            title: "覆盖 worker 面板".to_string(),
            status: TodoStatus::Verified,
            message: "验证完成".to_string(),
            commit_hash: None,
        });
        state.apply(&RuntimeEvent::ReviewGateReady {
            report: Box::new(sample_review_report(ApplyDecision::AllowFull)),
        });
        state.apply(&RuntimeEvent::SummaryReady {
            summary: Box::new(sample_summary("宽屏 dashboard 就绪")),
        });

        let screen = render_to_text(&state, 120, 36);
        let normalized = normalize_rendered_text(&screen);
        assert!(normalized.contains("补齐摘要"), "{screen}");
        assert!(normalized.contains("todo-1"), "{screen}");
        assert!(normalized.contains("结论：全部放行"), "{screen}");
        assert!(normalized.contains("宽屏dashboard就绪"), "{screen}");
    }

    #[test]
    fn render_runtime_dashboard_shows_compact_planning_view_without_workers() {
        let mut state = RuntimeViewState::new("session-6", "紧凑渲染");
        state.apply(&RuntimeEvent::PhaseChanged {
            phase: "规划中".to_string(),
        });
        state.apply(&RuntimeEvent::CommanderNote {
            message: "正在生成 todo".to_string(),
        });
        state.apply(&RuntimeEvent::TodoStateChanged {
            todo_id: "todo-1".to_string(),
            title: "补齐紧凑视图".to_string(),
            status: TodoStatus::Pending,
            message: "等待执行".to_string(),
            commit_hash: None,
        });

        let screen = render_to_text(&state, 80, 20);
        let normalized = normalize_rendered_text(&screen);
        assert!(normalized.contains("当前阶段：规划中"), "{screen}");
        assert!(normalized.contains("todo-1"), "{screen}");
        assert!(normalized.contains("等待最终摘要"), "{screen}");
        assert!(
            normalized.contains("最近决策：todo-1补齐紧凑视图->待执行/等待执行"),
            "{screen}"
        );
    }
}
