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
    BlockedReason, BrainRiskLevel, BrainState, FinalSummary, ReviewGateReport, RuntimeEvent,
    SchedulerSnapshot, TodoStatus, UiMode, WorkerQueueState, WorkerResult, WorkerStatus,
};

const MAX_EXECUTION_LINES: usize = 160;

/// Dashboard 里展示的单个 worker 视图状态。
#[derive(Debug, Clone)]
pub struct WorkerView {
    pub todo_id: Option<String>,
    pub role: String,
    pub title: String,
    pub status: WorkerStatus,
    pub queue_state: WorkerQueueState,
    pub blocked_reason: Option<BlockedReason>,
    pub lane: Option<usize>,
    pub attempts: usize,
    pub started_at: Option<Instant>,
    pub finished_at: Option<Instant>,
    pub phase_label: String,
    pub latest_brain_decision: Option<String>,
    pub last_event: String,
    pub worktree_path: String,
}

#[derive(Debug, Clone)]
pub struct ExecutionLogEntry {
    pub source: String,
    pub message: String,
}

/// 运行态共享视图模型。
/// 旧 CLI Rich UI 和 v6 AppShell 都复用这份状态，避免出现两套运行态解释逻辑。
#[derive(Debug, Clone)]
pub struct RuntimeViewState {
    pub session_id: String,
    pub task: String,
    pub phase: String,
    pub brain: Option<BrainState>,
    pub scheduler_snapshot: Option<SchedulerSnapshot>,
    pub current_user_stage: String,
    pub current_user_message: String,
    pub next_user_step: String,
    pub started_at: Instant,
    pub commander_notes: Vec<String>,
    pub workers: BTreeMap<String, WorkerView>,
    pub todos: BTreeMap<String, (String, TodoStatus, String)>,
    pub review_report: Option<ReviewGateReport>,
    pub summary: Option<FinalSummary>,
    pub graph_summary: Option<String>,
    pub apply_status: Option<String>,
    pub verify_status: Option<String>,
    pub deliverable_paths: Vec<String>,
    pub execution_lines: Vec<ExecutionLogEntry>,
    pub active_worker: Option<String>,
}

impl RuntimeViewState {
    pub fn new(session_id: &str, task: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            task: task.to_string(),
            phase: "初始化".to_string(),
            brain: None,
            scheduler_snapshot: None,
            current_user_stage: "准备开始".to_string(),
            current_user_message: "等待生成方案或开始执行。".to_string(),
            next_user_step: "先在开始页确认任务，再选择“先看方案”或“开始执行”。".to_string(),
            started_at: Instant::now(),
            commander_notes: Vec::new(),
            workers: BTreeMap::new(),
            todos: BTreeMap::new(),
            review_report: None,
            summary: None,
            graph_summary: None,
            apply_status: None,
            verify_status: None,
            deliverable_paths: Vec::new(),
            execution_lines: Vec::new(),
            active_worker: None,
        }
    }

    pub fn set_identity(&mut self, session_id: impl Into<String>, task: impl Into<String>) {
        self.session_id = session_id.into();
        self.task = task.into();
    }

    pub fn execution_entry_texts(&self, limit: usize) -> Vec<String> {
        self.execution_lines
            .iter()
            .rev()
            .take(limit)
            .rev()
            .map(|entry| format!("[{}] {}", entry.source, entry.message))
            .collect()
    }

    pub fn scheduler_snapshot(&self) -> SchedulerSnapshot {
        self.scheduler_snapshot
            .clone()
            .unwrap_or_else(|| SchedulerSnapshot {
                total_nodes: self.workers.len(),
                queued_count: self
                    .workers
                    .values()
                    .filter(|worker| worker.queue_state == WorkerQueueState::Queued)
                    .count(),
                ready_count: self
                    .workers
                    .values()
                    .filter(|worker| worker.queue_state == WorkerQueueState::Queued)
                    .count(),
                running_count: self
                    .workers
                    .values()
                    .filter(|worker| worker.status == WorkerStatus::Running)
                    .count(),
                blocked_dependency_count: self
                    .workers
                    .values()
                    .filter(|worker| {
                        worker.blocked_reason.as_ref().is_some_and(|reason| {
                            matches!(
                                reason.kind,
                                crate::model::BlockedReasonKind::WaitingDependencies
                            )
                        })
                    })
                    .count(),
                blocked_role_limit_count: self
                    .workers
                    .values()
                    .filter(|worker| {
                        worker.blocked_reason.as_ref().is_some_and(|reason| {
                            matches!(
                                reason.kind,
                                crate::model::BlockedReasonKind::RoleConcurrencyLimit
                            )
                        })
                    })
                    .count(),
                blocked_upstream_failed_count: self
                    .workers
                    .values()
                    .filter(|worker| {
                        worker.blocked_reason.as_ref().is_some_and(|reason| {
                            matches!(reason.kind, crate::model::BlockedReasonKind::UpstreamFailed)
                        })
                    })
                    .count(),
                finished_count: self
                    .workers
                    .values()
                    .filter(|worker| worker.queue_state == WorkerQueueState::Finished)
                    .count(),
                idle_slots: 0,
                critical_path_remaining: 0,
            })
    }

    pub fn apply(&mut self, event: &RuntimeEvent) {
        // 所有 RuntimeEvent 都在这里被折叠成“终端可渲染的稳定状态”。
        match event {
            RuntimeEvent::BrainStarted { state } => {
                self.brain = Some(state.as_ref().clone());
                self.current_user_stage = "Brain 已接管".to_string();
                self.current_user_message = truncate(&state.objective, 72);
                self.next_user_step = "观察 Brain 的调度决策和 agent 队列态变化。".to_string();
                push_execution_line(
                    &mut self.execution_lines,
                    "Brain",
                    format!("接管控制平面：{}", truncate(&state.objective, 120)),
                );
            }
            RuntimeEvent::BrainThought { thought } => {
                if let Some(brain) = self.brain.as_mut() {
                    brain.latest_thought = thought.clone();
                }
                self.current_user_message = truncate(thought, 72);
                push_note(
                    &mut self.commander_notes,
                    &format!("Brain：{}", truncate(thought, 72)),
                );
                push_execution_line(&mut self.execution_lines, "Brain", truncate(thought, 120));
            }
            RuntimeEvent::BrainDecisionMade { decision } => {
                let decision = decision.as_ref();
                let brain = self.brain.get_or_insert_with(|| BrainState {
                    status: "在线".to_string(),
                    objective: "统一指挥多 agent".to_string(),
                    current_focus: decision.summary.clone(),
                    latest_thought: decision.rationale.clone(),
                    latest_decision: decision.summary.clone(),
                    risk_level: decision.risk_level,
                    needs_user_attention: false,
                });
                brain.current_focus = decision.summary.clone();
                brain.latest_decision = decision.summary.clone();
                brain.latest_thought = decision.rationale.clone();
                brain.risk_level = decision.risk_level;
                brain.needs_user_attention = decision.risk_level == BrainRiskLevel::High;
                self.current_user_stage = "Brain 决策中".to_string();
                self.current_user_message = truncate(&decision.summary, 72);
                self.next_user_step = "继续观察被派发的 agent、阻塞原因和验证收口。".to_string();
                for agent_id in &decision.target_agents {
                    if let Some(worker) = self.workers.get_mut(agent_id) {
                        worker.latest_brain_decision = Some(decision.summary.clone());
                    }
                }
                push_note(
                    &mut self.commander_notes,
                    &format!("Brain 决策：{}", truncate(&decision.summary, 72)),
                );
                push_execution_line(
                    &mut self.execution_lines,
                    "Brain",
                    format!(
                        "{} / {}",
                        decision.action.label(),
                        truncate(&decision.rationale, 88)
                    ),
                );
            }
            RuntimeEvent::BrainEscalationRaised { message } => {
                let brain = self.brain.get_or_insert_with(|| BrainState {
                    status: "在线".to_string(),
                    objective: "统一指挥多 agent".to_string(),
                    current_focus: "等待升级处理".to_string(),
                    latest_thought: message.clone(),
                    latest_decision: "升级给用户".to_string(),
                    risk_level: BrainRiskLevel::High,
                    needs_user_attention: true,
                });
                brain.current_focus = "等待升级处理".to_string();
                brain.latest_thought = message.clone();
                brain.latest_decision = "升级给用户".to_string();
                brain.risk_level = BrainRiskLevel::High;
                brain.needs_user_attention = true;
                self.current_user_stage = "等待处理".to_string();
                self.current_user_message = truncate(message, 72);
                self.next_user_step = "优先处理 Brain 抛出的阻断或风险。".to_string();
                push_execution_line(&mut self.execution_lines, "Brain", truncate(message, 120));
            }
            RuntimeEvent::SchedulerSnapshotUpdated { snapshot } => {
                self.scheduler_snapshot = Some(snapshot.as_ref().clone());
            }
            RuntimeEvent::PhaseChanged { phase } => {
                self.phase = phase.clone();
                self.current_user_stage = user_stage_from_phase(phase).to_string();
                self.current_user_message = user_message_from_phase(phase).to_string();
                self.next_user_step = user_next_step_from_phase(phase).to_string();
                push_execution_line(
                    &mut self.execution_lines,
                    "阶段",
                    format!("进入 {}。", truncate(phase, 48)),
                );
            }
            RuntimeEvent::CommanderNote { message } => {
                push_note(&mut self.commander_notes, message);
                self.current_user_message = truncate(message, 72);
                push_execution_line(&mut self.execution_lines, "指挥", truncate(message, 120));
            }
            RuntimeEvent::GraphReady {
                nodes,
                dependencies,
            } => {
                self.graph_summary = Some(format!("节点 {} / 依赖 {}", nodes, dependencies));
                self.current_user_stage = "方案已完成".to_string();
                self.current_user_message =
                    format!("已拆出 {} 个执行节点，主依赖 {} 条。", nodes, dependencies);
                self.next_user_step = "查看方案是否合理，再决定是否开始执行。".to_string();
                push_execution_line(
                    &mut self.execution_lines,
                    "规划",
                    format!("执行图已生成：节点 {nodes} / 依赖 {dependencies}"),
                );
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
                self.current_user_stage = user_stage_from_todo_status(*status).to_string();
                self.current_user_message =
                    format!("{} 目前处于{}。", truncate(title, 24), status.label());
                self.next_user_step = user_next_step_from_todo_status(*status).to_string();
                push_execution_line(
                    &mut self.execution_lines,
                    "Todo",
                    format!(
                        "{} {} -> {} / {}",
                        todo_id,
                        truncate(title, 24),
                        status.label(),
                        truncate(message, 80)
                    ),
                );
            }
            RuntimeEvent::WorkerQueued {
                agent_id,
                role,
                title,
                todo_id,
                lane,
            } => {
                upsert_worker(
                    &mut self.workers,
                    agent_id,
                    role,
                    title,
                    todo_id.clone(),
                    WorkerStatus::Pending,
                    WorkerQueueState::Queued,
                );
                if let Some(worker) = self.workers.get_mut(agent_id) {
                    worker.blocked_reason = None;
                    worker.lane = Some(*lane);
                    worker.phase_label = "等待 Brain 派发".to_string();
                    worker.last_event = format!("已进入可派发队列（位次 {}）", lane);
                }
                push_execution_line(
                    &mut self.execution_lines,
                    "队列",
                    format!("{agent_id} 已进入可派发队列 / 位次 {lane}"),
                );
            }
            RuntimeEvent::WorkerBlocked {
                agent_id,
                role,
                title,
                todo_id,
                reason,
            } => {
                upsert_worker(
                    &mut self.workers,
                    agent_id,
                    role,
                    title,
                    todo_id.clone(),
                    WorkerStatus::Pending,
                    WorkerQueueState::Blocked,
                );
                if let Some(worker) = self.workers.get_mut(agent_id) {
                    worker.blocked_reason = Some(reason.clone());
                    worker.lane = None;
                    worker.phase_label = reason.label().to_string();
                    worker.last_event = truncate(&reason.detail, 72);
                }
                push_execution_line(
                    &mut self.execution_lines,
                    "阻塞",
                    format!(
                        "{agent_id} / {} / {}",
                        reason.label(),
                        truncate(&reason.detail, 80)
                    ),
                );
            }
            RuntimeEvent::WorkerRequeued { agent_id, reason } => {
                if let Some(worker) = self.workers.get_mut(agent_id) {
                    worker.queue_state = WorkerQueueState::Queued;
                    worker.blocked_reason = None;
                    worker.phase_label = "重新进入可派发队列".to_string();
                    worker.last_event = truncate(reason, 72);
                }
                push_execution_line(
                    &mut self.execution_lines,
                    "队列",
                    format!("{agent_id} 已重新入队 / {}", truncate(reason, 88)),
                );
            }
            RuntimeEvent::WorkerDispatched {
                agent_id,
                role,
                title,
                worktree_path,
            } => {
                upsert_worker(
                    &mut self.workers,
                    agent_id,
                    role,
                    title,
                    None,
                    WorkerStatus::Running,
                    WorkerQueueState::Running,
                );
                if let Some(worker) = self.workers.get_mut(agent_id) {
                    worker.status = WorkerStatus::Running;
                    worker.queue_state = WorkerQueueState::Running;
                    worker.blocked_reason = None;
                    worker.lane = None;
                    worker.phase_label = "执行中".to_string();
                    worker.started_at.get_or_insert_with(Instant::now);
                    worker.last_event = "已开始处理".to_string();
                    worker.worktree_path = worktree_path.display().to_string();
                }
                self.active_worker = Some(agent_id.clone());
                self.current_user_stage = "开始执行".to_string();
                self.current_user_message = format!("{} 正在处理“{}”。", role, truncate(title, 28));
                self.next_user_step = "等待子任务推进，再看审阅与验证结果。".to_string();
                push_execution_line(
                    &mut self.execution_lines,
                    agent_id.as_str(),
                    format!("{role} 已启动：{}", truncate(title, 80)),
                );
            }
            RuntimeEvent::WorkerUpdate {
                agent_id,
                kind,
                message,
            } => {
                self.active_worker = Some(agent_id.clone());
                if let Some(worker) = self.workers.get_mut(agent_id) {
                    worker.last_event = summarize_worker_update(kind, message)
                        .unwrap_or_else(|| truncate(message, 72));
                    worker.phase_label = "执行中".to_string();
                }
                if let Some(note) = summarize_worker_update(kind, message) {
                    push_note(&mut self.commander_notes, &note);
                    self.current_user_message = note.clone();
                    push_execution_line(
                        &mut self.execution_lines,
                        agent_id.as_str(),
                        truncate(&note, 120),
                    );
                }
            }
            RuntimeEvent::WorkerOutput {
                agent_id,
                stream,
                message,
            } => {
                self.active_worker = Some(agent_id.clone());
                if let Some(worker) = self.workers.get_mut(agent_id) {
                    worker.last_event = format!(
                        "{} 输出：{}",
                        if stream == "stderr" {
                            "错误流"
                        } else {
                            "标准流"
                        },
                        truncate(message, 56)
                    );
                    worker.phase_label = "命令执行中".to_string();
                }
                push_execution_line(
                    &mut self.execution_lines,
                    format!("{agent_id}/{stream}"),
                    truncate(message, 120),
                );
            }
            RuntimeEvent::HandoffReady {
                agent_id,
                handoff_path,
            } => {
                self.active_worker = Some(agent_id.clone());
                if let Some(worker) = self.workers.get_mut(agent_id) {
                    worker.last_event = "已产出交接稿".to_string();
                    worker.phase_label = "等待整合".to_string();
                }
                push_unique_path(
                    &mut self.deliverable_paths,
                    handoff_path.display().to_string(),
                );
                self.current_user_stage = "阶段产物已生成".to_string();
                self.current_user_message = "某个子任务已经交付了可供整合的结果。".to_string();
                self.next_user_step = "继续等待审阅、应用和验证收口。".to_string();
                push_execution_line(
                    &mut self.execution_lines,
                    agent_id.as_str(),
                    "已产出交接稿".to_string(),
                );
            }
            RuntimeEvent::MemoryViewReady {
                agent_id,
                memory_view_path,
                entries,
            } => {
                self.active_worker = Some(agent_id.clone());
                if let Some(worker) = self.workers.get_mut(agent_id) {
                    worker.last_event = format!("已整理上下文（{} 条）", entries);
                    worker.phase_label = "整理上下文".to_string();
                }
                push_unique_path(
                    &mut self.deliverable_paths,
                    memory_view_path.display().to_string(),
                );
                push_execution_line(
                    &mut self.execution_lines,
                    agent_id.as_str(),
                    format!("共享上下文已整理（{} 条）", entries),
                );
            }
            RuntimeEvent::WorkerFinished { result } => {
                upsert_result(&mut self.workers, result);
                self.active_worker = Some(result.agent_id.clone());
                self.current_user_message = format!(
                    "子任务“{}”已{}。",
                    truncate(&result.task_title, 24),
                    result.status.label()
                );
                push_execution_line(
                    &mut self.execution_lines,
                    result.agent_id.as_str(),
                    format!(
                        "子任务“{}”已{}",
                        truncate(&result.task_title, 40),
                        result.status.label()
                    ),
                );
            }
            RuntimeEvent::ApplyPlanReady { mode, operations } => {
                self.apply_status = Some(format!("计划：{} / {} 个 patch", mode, operations));
                self.current_user_stage = "准备落地".to_string();
                self.current_user_message =
                    format!("已形成结果落地计划，共 {} 个 patch。", operations);
                self.next_user_step = "等待审阅关卡决定哪些结果可以进入目标仓库。".to_string();
                push_execution_line(
                    &mut self.execution_lines,
                    "交付",
                    format!("已形成落地计划：{} / {} 个 patch", mode, operations),
                );
            }
            RuntimeEvent::ReviewGateReady { report } => {
                self.review_report = Some(report.as_ref().clone());
                self.apply_status = Some(format!("review gate：{}", report.decision.label()));
                self.current_user_stage = "审阅完成".to_string();
                self.current_user_message = format!("审阅结论：{}。", report.decision.label());
                self.next_user_step = "继续查看应用结果和验证结论。".to_string();
                push_execution_line(
                    &mut self.execution_lines,
                    "审阅",
                    format!("审阅结论：{}", report.decision.label()),
                );
            }
            RuntimeEvent::ApplyUpdate { message } => {
                self.apply_status = Some(message.clone());
                self.current_user_stage = "结果落地中".to_string();
                self.current_user_message = truncate(message, 72);
                self.next_user_step = "等待应用完成，再看验证是否通过。".to_string();
                push_execution_line(&mut self.execution_lines, "交付", truncate(message, 120));
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
                self.current_user_stage = "验证完成".to_string();
                self.current_user_message =
                    format!("{} 已{}。", stage, if *success { "通过" } else { "失败" });
                self.next_user_step = if *success {
                    "等待最终交付摘要。".to_string()
                } else {
                    "查看失败原因，并决定是否继续修正。".to_string()
                };
                push_execution_line(
                    &mut self.execution_lines,
                    "验证",
                    format!(
                        "{} / {} / {}",
                        stage,
                        if *success { "通过" } else { "失败" },
                        truncate(message, 96)
                    ),
                );
            }
            RuntimeEvent::MemoryUpdated {
                scope,
                reason,
                entries,
                ..
            } => {
                push_note(
                    &mut self.commander_notes,
                    &format!("memory / {scope} / {reason} / {entries} 条"),
                );
                push_execution_line(
                    &mut self.execution_lines,
                    "记忆",
                    format!("{scope} / {reason} / {entries} 条"),
                );
            }
            RuntimeEvent::SummaryReady { summary } => {
                self.summary = Some(summary.as_ref().clone());
                self.current_user_stage = "已完成".to_string();
                self.current_user_message = truncate(&summary.overview, 72);
                self.next_user_step =
                    "去“最终交付”查看结果和产物目录，或到历史页继续优化。".to_string();
                push_execution_line(
                    &mut self.execution_lines,
                    "总结",
                    truncate(&summary.overview, 120),
                );
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
    let snapshot = state.scheduler_snapshot();
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Min(12),
            Constraint::Length(8),
            Constraint::Length(7),
        ])
        .split(area);

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
            "Brain：{}   风险：{}   阶段：{}   已耗时：{}s",
            state
                .brain
                .as_ref()
                .map(|brain| truncate(&brain.current_focus, 28))
                .unwrap_or_else(|| "等待接管".to_string()),
            state
                .brain
                .as_ref()
                .map(|brain| brain.risk_level.label())
                .unwrap_or("低"),
            state.current_user_stage,
            state.started_at.elapsed().as_secs()
        )),
        Line::from(format!(
            "并行：ready {} / running {} / dep-block {} / role-block {} / upstream-fail {} / idle {} / critical {}",
            snapshot.ready_count,
            snapshot.running_count,
            snapshot.blocked_dependency_count,
            snapshot.blocked_role_limit_count,
            snapshot.blocked_upstream_failed_count,
            snapshot.idle_slots,
            snapshot.critical_path_remaining
        )),
        Line::from(format!("现在在做：{}", truncate(&state.current_user_message, 110))),
        Line::from(format!("下一步：{}", truncate(&state.next_user_step, 110))),
    ])
    .block(Block::default().title(title).borders(Borders::ALL))
    .wrap(Wrap { trim: true });
    frame.render_widget(header, sections[0]);

    let middle_sections = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
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
                let runtime_hint = worker
                    .blocked_reason
                    .as_ref()
                    .map(|reason| format!("{} / {}", reason.label(), truncate(&reason.detail, 28)))
                    .unwrap_or_else(|| truncate(&worker.last_event, 28));
                Row::new(vec![
                    Cell::from(truncate(agent_id, 14)),
                    Cell::from(worker.todo_id.clone().unwrap_or_else(|| "-".to_string())),
                    Cell::from(worker.queue_state.label()),
                    Cell::from(truncate(&worker.title, 14)),
                    Cell::from(truncate(&worker.phase_label, 14)),
                    Cell::from(
                        worker
                            .lane
                            .map(|lane| lane.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                    ),
                    Cell::from(format!(
                        "{}s",
                        worker
                            .started_at
                            .map(|started| started.elapsed().as_secs())
                            .unwrap_or(0)
                    )),
                    Cell::from(runtime_hint),
                ])
                .style(style_for_worker(worker))
            })
            .collect::<Vec<_>>();

        let workers_table = Table::new(
            worker_rows,
            [
                Constraint::Length(14),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(14),
                Constraint::Length(14),
                Constraint::Length(6),
                Constraint::Length(6),
                Constraint::Min(20),
            ],
        )
        .header(
            Row::new(vec![
                "Agent",
                "Todo",
                "队列态",
                "任务",
                "阶段",
                "Lane",
                "耗时",
                "最近变化/阻塞",
            ])
            .style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(Block::default().title("Agent Matrix").borders(Borders::ALL))
        .column_spacing(1);
        frame.render_widget(workers_table, middle_sections[0]);
    }
    let right_sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Min(8),
        ])
        .split(middle_sections[1]);
    frame.render_widget(
        Paragraph::new(brain_panel_lines(state, area.width))
            .block(Block::default().title("Brain").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        right_sections[0],
    );
    frame.render_widget(
        Paragraph::new(scheduler_panel_lines(state, area.width))
            .block(Block::default().title("并行态势").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        right_sections[1],
    );
    let mut todo_items = Vec::new();
    if let Some(report) = &state.review_report {
        todo_items.push(ListItem::new(Line::from(format!(
            "结论：{}",
            report.decision.label()
        ))));
        todo_items.push(ListItem::new(Line::from(format!(
            "说明：{}",
            truncate(
                report
                    .confidence_reasoning
                    .as_deref()
                    .unwrap_or("无补充说明"),
                18
            )
        ))));
    }
    todo_items.extend(
        state
            .todos
            .iter()
            .take(4)
            .map(|(todo_id, (title, status, message))| {
                ListItem::new(Line::from(format!(
                    "{} {} / {} / {}",
                    todo_id,
                    truncate(title, 16),
                    status.label(),
                    truncate(message, 20)
                )))
            }),
    );
    frame.render_widget(
        List::new(todo_items).block(Block::default().title("Todo / Gate").borders(Borders::ALL)),
        right_sections[2],
    );

    let note_items = state
        .commander_notes
        .iter()
        .rev()
        .map(|note| ListItem::new(Line::from(Span::raw(note.clone()))))
        .collect::<Vec<_>>();
    let notes =
        List::new(note_items).block(Block::default().title("过程摘要").borders(Borders::ALL));
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
                "目标目录交付：{}",
                if matches!(summary.apply_status, crate::model::ApplyStatus::Applied) {
                    "已交付".to_string()
                } else {
                    "未交付".to_string()
                }
            )),
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
        .block(Block::default().title("交付状态").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(footer, sections[3]);
}

fn render_runtime_dashboard_compact(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &RuntimeViewState,
    title: &str,
) {
    let snapshot = state.scheduler_snapshot();
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints(compact_dashboard_constraints(area))
        .split(area);

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
                "{} | ready{} run{} block{}",
                truncate(&state.current_user_stage, 10),
                snapshot.ready_count,
                snapshot.running_count,
                snapshot.blocked_dependency_count
                    + snapshot.blocked_role_limit_count
                    + snapshot.blocked_upstream_failed_count
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
                "阶段：{}   ready：{}   running：{}   blocked：{}   {}s",
                state.current_user_stage,
                snapshot.ready_count,
                snapshot.running_count,
                snapshot.blocked_dependency_count
                    + snapshot.blocked_role_limit_count
                    + snapshot.blocked_upstream_failed_count,
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
                let detail = worker
                    .blocked_reason
                    .as_ref()
                    .map(|reason| truncate(&reason.detail, if area.width < 72 { 18 } else { 32 }))
                    .unwrap_or_else(|| {
                        truncate(&worker.last_event, if area.width < 72 { 18 } else { 32 })
                    });
                ListItem::new(Line::from(format!(
                    "{} / {} / {} / {}",
                    truncate(agent_id, 10),
                    worker.queue_state.label(),
                    truncate(&worker.phase_label, 10),
                    detail
                )))
                .style(style_for_worker(worker))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(worker_items).block(Block::default().title("Agent").borders(Borders::ALL)),
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
    let snapshot = state.scheduler_snapshot();
    let todo_limit = if width < 72 { 1 } else { 2 };
    let mut lines = vec![
        Line::from(format!(
            "Brain：{}",
            truncate(
                &state
                    .brain
                    .as_ref()
                    .map(|brain| brain.current_focus.clone())
                    .unwrap_or_else(|| "等待接管".to_string()),
                if width < 72 { 20 } else { 44 }
            )
        )),
        Line::from(format!(
            "并行：ready {} / run {} / block {}",
            snapshot.ready_count,
            snapshot.running_count,
            snapshot.blocked_dependency_count
                + snapshot.blocked_role_limit_count
                + snapshot.blocked_upstream_failed_count
        )),
        Line::from(format!(
            "现在在做：{}",
            truncate(
                &state.current_user_message,
                if width < 72 { 24 } else { 48 }
            )
        )),
        Line::from(format!(
            "Apply：{}",
            truncate(
                &state
                    .apply_status
                    .clone()
                    .unwrap_or_else(|| "等待".to_string()),
                if width < 72 { 24 } else { 48 }
            )
        )),
        Line::from(format!(
            "Verify：{}",
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
    if let Some(brain) = &state.brain {
        lines.push(Line::from(format!(
            "风险：{} / {}",
            brain.risk_level.label(),
            if brain.needs_user_attention {
                "需处理"
            } else {
                "自动推进"
            }
        )));
    }
    lines
}

fn planning_activity_lines(state: &RuntimeViewState, compact: bool) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(format!("当前阶段：{}", state.current_user_stage)),
        Line::from(format!(
            "现在在做：{}",
            truncate(&state.current_user_message, if compact { 28 } else { 72 })
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
            Line::from(format!("下一步：{}", state.next_user_step)),
            Line::from(format!(
                "最近决策：{}",
                truncate(note, if width < 72 { 26 } else { 56 })
            )),
        ]
    } else {
        vec![Line::from(format!("下一步：{}", state.next_user_step))]
    }
}

fn brain_panel_lines(state: &RuntimeViewState, width: u16) -> Vec<Line<'static>> {
    if let Some(brain) = &state.brain {
        vec![
            Line::from(format!("状态：{}", brain.status)),
            Line::from(format!(
                "目标：{}",
                truncate(&brain.objective, if width < 120 { 36 } else { 52 })
            )),
            Line::from(format!(
                "焦点：{}",
                truncate(&brain.current_focus, if width < 120 { 36 } else { 52 })
            )),
            Line::from(format!(
                "思考：{}",
                truncate(&brain.latest_thought, if width < 120 { 36 } else { 52 })
            )),
            Line::from(format!(
                "决策：{}",
                truncate(&brain.latest_decision, if width < 120 { 36 } else { 52 })
            )),
            Line::from(format!(
                "风险：{} / {}",
                brain.risk_level.label(),
                if brain.needs_user_attention {
                    "需要人工介入"
                } else {
                    "自动推进中"
                }
            )),
        ]
    } else {
        vec![
            Line::from("状态：等待 Brain 接管"),
            Line::from("目标：尚未进入运行期控制"),
        ]
    }
}

fn scheduler_panel_lines(state: &RuntimeViewState, width: u16) -> Vec<Line<'static>> {
    let snapshot = state.scheduler_snapshot();
    let mut lines = vec![
        Line::from(format!(
            "ready {} / running {} / queued {}",
            snapshot.ready_count, snapshot.running_count, snapshot.queued_count
        )),
        Line::from(format!(
            "blocked：依赖 {} / 角色 {} / 上游失败 {}",
            snapshot.blocked_dependency_count,
            snapshot.blocked_role_limit_count,
            snapshot.blocked_upstream_failed_count
        )),
        Line::from(format!(
            "idle slots：{} / critical path：{}",
            snapshot.idle_slots, snapshot.critical_path_remaining
        )),
    ];
    if let Some(active_worker) = state.active_worker.as_deref() {
        lines.push(Line::from(format!("当前焦点：{active_worker}")));
    }
    if let Some(report) = &state.review_report {
        lines.push(Line::from(format!("结论：{}", report.decision.label())));
        lines.push(Line::from(format!(
            "说明：{}",
            truncate(
                report
                    .confidence_reasoning
                    .as_deref()
                    .unwrap_or("无补充说明"),
                if width < 120 { 36 } else { 52 }
            )
        )));
    } else {
        lines.push(Line::from("Gate：等待 reviewer"));
    }
    lines
}

pub fn describe_runtime_event(event: &RuntimeEvent) -> String {
    match event {
        RuntimeEvent::BrainStarted { state } => {
            format!("Brain 已接管：{}", truncate(&state.objective, 72))
        }
        RuntimeEvent::BrainThought { thought } => format!("Brain 思考：{}", truncate(thought, 72)),
        RuntimeEvent::BrainDecisionMade { decision } => format!(
            "Brain 决策：{} / {}",
            decision.action.label(),
            truncate(&decision.summary, 60)
        ),
        RuntimeEvent::BrainEscalationRaised { message } => {
            format!("Brain 升级：{}", truncate(message, 72))
        }
        RuntimeEvent::SchedulerSnapshotUpdated { snapshot } => format!(
            "调度快照：ready {} / running {} / 阻塞 {} / 关键路径 {}",
            snapshot.ready_count,
            snapshot.running_count,
            snapshot.blocked_dependency_count
                + snapshot.blocked_role_limit_count
                + snapshot.blocked_upstream_failed_count,
            snapshot.critical_path_remaining
        ),
        RuntimeEvent::PhaseChanged { phase } => {
            format!("进入{}阶段", user_stage_from_phase(phase))
        }
        RuntimeEvent::CommanderNote { message } => format!("说明：{}", truncate(message, 80)),
        RuntimeEvent::GraphReady {
            nodes,
            dependencies,
        } => format!("方案完成：共 {nodes} 个执行节点，依赖 {dependencies} 条"),
        RuntimeEvent::TodoStateChanged {
            title,
            status,
            message,
            ..
        } => format!("{}：{} / {}", title, status.label(), truncate(message, 60)),
        RuntimeEvent::WorkerQueued {
            role, lane, title, ..
        } => format!("{role} 入队：{} / 位次 {}", truncate(title, 42), lane),
        RuntimeEvent::WorkerBlocked { title, reason, .. } => format!(
            "{} 阻塞：{} / {}",
            truncate(title, 30),
            reason.label(),
            truncate(&reason.detail, 42)
        ),
        RuntimeEvent::WorkerRequeued { agent_id, reason } => {
            format!("{agent_id} 重新入队：{}", truncate(reason, 54))
        }
        RuntimeEvent::WorkerDispatched { role, title, .. } => {
            format!("{role} 开始处理：{}", truncate(title, 48))
        }
        RuntimeEvent::WorkerUpdate { kind, message, .. } => {
            summarize_worker_update(kind, message).unwrap_or_else(|| truncate(message, 80))
        }
        RuntimeEvent::WorkerOutput {
            agent_id,
            stream,
            message,
        } => format!(
            "{} {}：{}",
            agent_id,
            if stream == "stderr" {
                "错误流"
            } else {
                "标准流"
            },
            truncate(message, 80)
        ),
        RuntimeEvent::HandoffReady { .. } => "已生成阶段交接稿".to_string(),
        RuntimeEvent::MemoryViewReady { entries, .. } => {
            format!("已整理共享上下文（{} 条）", entries)
        }
        RuntimeEvent::WorkerFinished { result } => {
            format!("子任务“{}”已{}", result.task_title, result.status.label())
        }
        RuntimeEvent::ApplyPlanReady { mode, operations } => {
            format!("已形成落地计划：{} / {} 个 patch", mode, operations)
        }
        RuntimeEvent::ReviewGateReady { report } => format!(
            "审阅结论：{} / {}",
            report.decision.label(),
            report
                .confidence_reasoning
                .clone()
                .unwrap_or_else(|| "无补充说明".to_string())
        ),
        RuntimeEvent::ApplyUpdate { message } => format!("落地更新：{}", truncate(message, 80)),
        RuntimeEvent::VerificationReady {
            stage,
            success,
            message,
        } => format!(
            "{} 已{} / {}",
            stage,
            if *success { "通过" } else { "失败" },
            truncate(message, 60)
        ),
        RuntimeEvent::MemoryUpdated {
            scope,
            reason,
            entries,
            ..
        } => {
            format!("记忆已更新：{scope} / {reason} / {entries} 条")
        }
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
        RuntimeEvent::BrainStarted { state } => {
            println!("🧠 Brain 接管：{}", state.objective);
        }
        RuntimeEvent::BrainThought { thought } => {
            println!("💭 {}", truncate(thought, 120));
        }
        RuntimeEvent::BrainDecisionMade { decision } => {
            println!(
                "🧠 决策：{} / {}",
                decision.action.label(),
                truncate(&decision.summary, 120)
            );
        }
        RuntimeEvent::BrainEscalationRaised { message } => {
            println!("🚨 Brain 升级：{}", truncate(message, 120));
        }
        RuntimeEvent::SchedulerSnapshotUpdated { snapshot } => {
            println!(
                "📊 ready={} running={} blocked(dep={} role={} upstream={}) critical={}",
                snapshot.ready_count,
                snapshot.running_count,
                snapshot.blocked_dependency_count,
                snapshot.blocked_role_limit_count,
                snapshot.blocked_upstream_failed_count,
                snapshot.critical_path_remaining
            );
        }
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
        RuntimeEvent::WorkerQueued {
            agent_id,
            role,
            title,
            lane,
            ..
        } => {
            println!(
                "🗂️ {agent_id} / {role} 入队：{}（位次 {lane}）",
                truncate(title, 72)
            );
        }
        RuntimeEvent::WorkerBlocked {
            agent_id, reason, ..
        } => {
            println!(
                "⛔ {agent_id} 阻塞：{} / {}",
                reason.label(),
                truncate(&reason.detail, 120)
            );
        }
        RuntimeEvent::WorkerRequeued { agent_id, reason } => {
            println!("🔁 {agent_id} 重新入队：{}", truncate(reason, 120));
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
        RuntimeEvent::WorkerOutput {
            agent_id,
            stream,
            message,
        } => {
            println!(
                "📡 {agent_id} [{}] {}",
                if stream == "stderr" {
                    "stderr"
                } else {
                    "stdout"
                },
                truncate(message, 120)
            );
        }
        RuntimeEvent::HandoffReady {
            agent_id,
            handoff_path,
        } => {
            println!("📦 {agent_id} handoff 就绪：{}", handoff_path.display());
        }
        RuntimeEvent::MemoryViewReady {
            agent_id,
            memory_view_path,
            entries,
        } => {
            println!(
                "🧠 {agent_id} 共享记忆就绪：{}（{} 条）",
                memory_view_path.display(),
                entries
            );
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
        RuntimeEvent::MemoryUpdated {
            scope,
            reason,
            path,
            entries,
        } => {
            println!(
                "🧠 共享记忆更新：{scope} / {reason} / {}（{} 条）",
                path.display(),
                entries
            );
        }
        RuntimeEvent::SummaryReady { summary } => {
            println!("📦 最终摘要：{}", summary.overview);
        }
    }
}

fn upsert_result(workers: &mut BTreeMap<String, WorkerView>, result: &WorkerResult) {
    let previous = workers.get(&result.agent_id).cloned();
    workers.insert(
        result.agent_id.clone(),
        WorkerView {
            todo_id: previous.as_ref().and_then(|item| item.todo_id.clone()),
            role: result.role.clone(),
            title: result.task_title.clone(),
            status: result.status,
            queue_state: WorkerQueueState::Finished,
            blocked_reason: None,
            lane: None,
            attempts: result.attempts,
            started_at: previous.as_ref().and_then(|item| item.started_at),
            finished_at: Some(Instant::now()),
            phase_label: "已完成".to_string(),
            latest_brain_decision: previous.and_then(|item| item.latest_brain_decision),
            last_event: result
                .summary
                .clone()
                .or_else(|| result.diagnostic_summary.clone())
                .unwrap_or_else(|| truncate(&result.final_message, 72)),
            worktree_path: result.worktree_path.display().to_string(),
        },
    );
}

fn upsert_worker(
    workers: &mut BTreeMap<String, WorkerView>,
    agent_id: &str,
    role: &str,
    title: &str,
    todo_id: Option<String>,
    status: WorkerStatus,
    queue_state: WorkerQueueState,
) {
    let existing = workers.get(agent_id).cloned();
    workers.insert(
        agent_id.to_string(),
        WorkerView {
            todo_id: todo_id.or_else(|| existing.as_ref().and_then(|item| item.todo_id.clone())),
            role: role.to_string(),
            title: title.to_string(),
            status,
            queue_state,
            blocked_reason: existing
                .as_ref()
                .and_then(|item| item.blocked_reason.clone()),
            lane: existing.as_ref().and_then(|item| item.lane),
            attempts: existing.as_ref().map(|item| item.attempts).unwrap_or(0),
            started_at: existing.as_ref().and_then(|item| item.started_at),
            finished_at: existing.as_ref().and_then(|item| item.finished_at),
            phase_label: existing
                .as_ref()
                .map(|item| item.phase_label.clone())
                .unwrap_or_else(|| queue_state.label().to_string()),
            latest_brain_decision: existing
                .as_ref()
                .and_then(|item| item.latest_brain_decision.clone()),
            last_event: existing
                .as_ref()
                .map(|item| item.last_event.clone())
                .unwrap_or_else(|| queue_state.label().to_string()),
            worktree_path: existing
                .as_ref()
                .map(|item| item.worktree_path.clone())
                .unwrap_or_default(),
        },
    );
}

fn push_execution_line(
    lines: &mut Vec<ExecutionLogEntry>,
    source: impl Into<String>,
    message: impl Into<String>,
) {
    lines.push(ExecutionLogEntry {
        source: source.into(),
        message: message.into(),
    });
    if lines.len() > MAX_EXECUTION_LINES {
        let overflow = lines.len() - MAX_EXECUTION_LINES;
        lines.drain(0..overflow);
    }
}

fn user_stage_from_phase(phase: &str) -> &'static str {
    if phase.contains("规划") {
        "方案规划中"
    } else if phase.contains("执行") || phase.contains("调度") {
        "执行中"
    } else if phase.contains("审") {
        "审阅中"
    } else if phase.contains("应用") || phase.contains("集成") {
        "结果落地中"
    } else if phase.contains("验证") {
        "验证中"
    } else if phase.contains("总结") {
        "收尾中"
    } else {
        "准备中"
    }
}

fn user_message_from_phase(phase: &str) -> &'static str {
    if phase.contains("规划") {
        "系统正在把需求拆成可执行的方案和待办。"
    } else if phase.contains("执行") || phase.contains("调度") {
        "系统正在并行推进子任务，并持续收集结果。"
    } else if phase.contains("审") {
        "系统正在审阅结果，判断哪些改动可以放行。"
    } else if phase.contains("应用") || phase.contains("集成") {
        "系统正在把通过审阅的结果落到目标仓库。"
    } else if phase.contains("验证") {
        "系统正在验证改动是否真的可用。"
    } else if phase.contains("总结") {
        "系统正在整理最终交付摘要。"
    } else {
        "系统正在准备本轮任务。"
    }
}

fn user_next_step_from_phase(phase: &str) -> &'static str {
    if phase.contains("规划") {
        "等待方案和待办清单生成完成。"
    } else if phase.contains("执行") || phase.contains("调度") {
        "重点看子任务推进、审阅和验证是否顺利。"
    } else if phase.contains("审") {
        "等待审阅结论，确认哪些结果能继续落地。"
    } else if phase.contains("应用") || phase.contains("集成") {
        "等待应用完成，再看验证结果。"
    } else if phase.contains("验证") {
        "等待验证结束，再查看最终交付。"
    } else if phase.contains("总结") {
        "等待交付摘要生成后回看结果。"
    } else {
        "先生成方案或开始执行。"
    }
}

fn user_stage_from_todo_status(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending | TodoStatus::Ready => "方案已就绪",
        TodoStatus::Running => "执行中",
        TodoStatus::InReview => "审阅中",
        TodoStatus::Verifying => "验证中",
        TodoStatus::Applied | TodoStatus::Committed => "结果已落地",
        TodoStatus::Verified => "验证通过",
        TodoStatus::Failed | TodoStatus::Blocked => "执行受阻",
        TodoStatus::NeedsManualFollowup => "待人工跟进",
    }
}

fn user_next_step_from_todo_status(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending | TodoStatus::Ready => "查看是否要开始执行。",
        TodoStatus::Running => "等待子任务完成，再看审阅意见。",
        TodoStatus::InReview => "等待审阅结论确认放行范围。",
        TodoStatus::Verifying => "等待验证结果确认是否可交付。",
        TodoStatus::Applied | TodoStatus::Committed => "继续查看验证与最终摘要。",
        TodoStatus::Verified => "可以转到最终交付页确认结果。",
        TodoStatus::Failed | TodoStatus::Blocked => "查看失败原因，决定是否继续修正。",
        TodoStatus::NeedsManualFollowup => "需要人工判断下一步动作。",
    }
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

fn style_for_worker(worker: &WorkerView) -> Style {
    if worker.queue_state == WorkerQueueState::Blocked {
        Style::default().fg(Color::Yellow)
    } else {
        style_for_status(worker.status)
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

fn push_unique_path(paths: &mut Vec<String>, path: String) {
    if paths.iter().any(|item| item == &path) {
        return;
    }
    paths.push(path);
    if paths.len() > 6 {
        let overflow = paths.len() - 6;
        paths.drain(0..overflow);
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

        assert!(rendered.contains("方案已就绪"));
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
        state.apply(&RuntimeEvent::WorkerOutput {
            agent_id: "implementer-1".to_string(),
            stream: "stdout".to_string(),
            message: "cargo test -q".to_string(),
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
        assert!(
            state
                .execution_entry_texts(12)
                .iter()
                .any(|line| line.contains("cargo test -q"))
        );
        assert_eq!(state.active_worker.as_deref(), Some("implementer-1"));
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
        assert!(fallback_summary.contains("下一步："));
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
        assert!(rendered.contains("审阅结论：全部放行"));
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
        assert!(normalized.contains("当前阶段：方案已就绪"), "{screen}");
        assert!(normalized.contains("todo-1"), "{screen}");
        assert!(
            normalized.contains("下一步：查看是否要开始执行。"),
            "{screen}"
        );
        assert!(
            normalized.contains("最近决策：todo-1补齐紧凑视图->待执行/等待执行"),
            "{screen}"
        );
    }
}
