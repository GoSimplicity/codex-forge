use std::fs;
use std::io::{self, IsTerminal, Stdout};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Row, Table, Tabs, Wrap};
use tokio::sync::{mpsc, watch};

use crate::app::{resolve_plan_config, resolve_run_config};
use crate::cli::{ApplyModeArg, PlanArgs, RunArgs, SharedTaskArgs, UiModeArg};
use crate::config::{LoadedProjectConfig, load_project_config};
use crate::doctor::run_doctor;
use crate::model::{ApplyMode, DoctorReport, RuntimeEvent, SessionManifest, SessionPreset};
use crate::orchestrator::{EmbeddedRunOutcome, plan_session_embedded, run_session_embedded};
use crate::replay::replay_session_embedded;
use crate::resources::{ResourceCatalog, load_resource_catalog};
use crate::session::load_session;
use crate::ui::{RuntimeViewState, describe_runtime_event, render_runtime_dashboard};
use crate::workspace::{remember_target_dir, resolve_target_dir};

const MAX_LOG_LINES: usize = 240;
const MAX_NOTICE_LINES: usize = 8;

/// v5 终端产品主导航。保留固定 5 个页面，确保演示路径稳定可记忆。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Home,
    Compose,
    Run,
    History,
    Result,
}

impl Route {
    fn label(self) -> &'static str {
        match self {
            Self::Home => "首页",
            Self::Compose => "新任务",
            Self::Run => "执行",
            Self::History => "历史",
            Self::Result => "结果",
        }
    }

    fn all() -> [Self; 5] {
        [
            Self::Home,
            Self::Compose,
            Self::Run,
            Self::History,
            Self::Result,
        ]
    }
}

/// “新任务”页的表单字段定义。
/// 这里把终端产品中所有可配项显式列出来，便于统一渲染、编辑和命令预览。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormField {
    TargetDir,
    ConfigPath,
    Task,
    RoleSet,
    Workers,
    MaxRetries,
    Model,
    ApplyMode,
    Preset,
    FailFast,
    CleanupSuccess,
    ResumeSession,
}

impl FormField {
    fn label(self) -> &'static str {
        match self {
            Self::TargetDir => "目标仓库",
            Self::ConfigPath => "配置文件",
            Self::Task => "任务描述",
            Self::RoleSet => "角色集合",
            Self::Workers => "并发 Worker",
            Self::MaxRetries => "最大重试",
            Self::Model => "模型",
            Self::ApplyMode => "应用模式",
            Self::Preset => "运行预设",
            Self::FailFast => "Fail Fast",
            Self::CleanupSuccess => "清理成功 worktree",
            Self::ResumeSession => "恢复 Session",
        }
    }

    fn all() -> [Self; 12] {
        [
            Self::TargetDir,
            Self::ConfigPath,
            Self::Task,
            Self::RoleSet,
            Self::Workers,
            Self::MaxRetries,
            Self::Model,
            Self::ApplyMode,
            Self::Preset,
            Self::FailFast,
            Self::CleanupSuccess,
            Self::ResumeSession,
        ]
    }
}

/// 终端壳层可直接触发的动作。
/// 这些动作最终会映射到内嵌 doctor / plan / run / replay 执行链路。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellAction {
    Doctor,
    Plan,
    Run,
    ReplaySelected,
}

impl ShellAction {
    fn label(self) -> &'static str {
        match self {
            Self::Doctor => "Doctor",
            Self::Plan => "Plan",
            Self::Run => "Run",
            Self::ReplaySelected => "Replay",
        }
    }

    fn supports_stop(self) -> bool {
        matches!(self, Self::Run | Self::ReplaySelected)
    }
}

/// 顶部状态栏与结果提示统一使用的动作状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandState {
    Running,
    Succeeded,
    Failed,
    Stopped,
}

impl CommandState {
    fn label(self) -> &'static str {
        match self {
            Self::Running => "运行中",
            Self::Succeeded => "成功",
            Self::Failed => "失败",
            Self::Stopped => "已停止",
        }
    }

    fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }
}

/// “执行”页的三种子视图：
/// - Dashboard：实时态势总览
/// - Timeline：事件流观察
/// - Summary：收敛与交付摘要
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunSubview {
    Dashboard,
    Timeline,
    Summary,
}

impl RunSubview {
    fn label(self) -> &'static str {
        match self {
            Self::Dashboard => "实时 Dashboard",
            Self::Timeline => "Timeline",
            Self::Summary => "Summary",
        }
    }

    fn all() -> [Self; 3] {
        [Self::Dashboard, Self::Timeline, Self::Summary]
    }
}

/// 后台任务推回到 UI 线程的统一事件。
/// AppShell 不直接关心底层实现细节，只消费这层抽象后的运行消息。
#[derive(Debug)]
enum RunnerEvent {
    Line(String),
    Runtime(RuntimeEvent),
    Doctor(DoctorReport),
    Finished {
        state: CommandState,
        manifest: Option<SessionManifest>,
    },
}

/// 当前正在运行的内嵌动作状态。
/// 除了日志缓冲外，还持有一个可选的 cancel sender，用于在 UI 层触发安全停止。
#[derive(Debug)]
struct ActiveCommand {
    action: ShellAction,
    commandline: String,
    state: CommandState,
    started_at: Instant,
    output: Vec<String>,
    cancel_tx: Option<watch::Sender<bool>>,
    rx: mpsc::UnboundedReceiver<RunnerEvent>,
}

#[derive(Debug, Clone)]
struct SessionSummary {
    id: String,
    created_at: String,
    task: String,
    status: String,
    result: String,
}

#[derive(Debug, Clone)]
struct ProjectContext {
    target_dir: PathBuf,
    display_target: String,
    config_source: String,
    verification_commands: Vec<String>,
    role_sets: Vec<String>,
    rule_source: String,
    reviewer_rule_source: String,
    sessions: Vec<SessionSummary>,
    last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct FormState {
    target_dir: String,
    config_path: String,
    task: String,
    role_set: String,
    workers: String,
    max_retries: String,
    model: String,
    apply_mode: ApplyMode,
    preset: Option<SessionPreset>,
    fail_fast: bool,
    cleanup_success: bool,
    resume_session_id: String,
}

impl Default for FormState {
    fn default() -> Self {
        Self {
            target_dir: String::new(),
            config_path: String::new(),
            task: String::new(),
            role_set: "default".to_string(),
            workers: "4".to_string(),
            max_retries: "2".to_string(),
            model: String::new(),
            apply_mode: ApplyMode::AutoSafe,
            preset: Some(SessionPreset::FeatureDemo),
            fail_fast: false,
            cleanup_success: true,
            resume_session_id: String::new(),
        }
    }
}

struct AppShell {
    route: Route,
    selected_field: usize,
    editing: bool,
    history_index: usize,
    notices: Vec<String>,
    form: FormState,
    project: ProjectContext,
    selected_session: Option<SessionManifest>,
    runtime_state: Option<RuntimeViewState>,
    run_subview: RunSubview,
    last_doctor_report: Option<DoctorReport>,
    active_command: Option<ActiveCommand>,
    should_quit: bool,
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

pub async fn run_app_shell(initial_target_dir: Option<PathBuf>) -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("v5 主界面需要在交互式终端中运行");
    }

    let initial_dir = if let Some(path) = initial_target_dir {
        path
    } else {
        resolve_target_dir(None)?.path
    };

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut guard = TerminalGuard { terminal };
    let mut app = AppShell::bootstrap(initial_dir)?;

    loop {
        app.poll_command_output()?;
        guard.terminal.draw(|frame| app.render(frame))?;

        if app.should_quit {
            break;
        }

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            app.handle_key(key).await?;
        }
    }

    Ok(())
}

impl AppShell {
    fn bootstrap(initial_target_dir: PathBuf) -> Result<Self> {
        let mut form = FormState {
            target_dir: initial_target_dir.display().to_string(),
            ..FormState::default()
        };
        let initial_target = PathBuf::from(form.target_dir.clone());
        let project = load_project_context(&initial_target, None, &mut form, true)?;
        let selected_session = project
            .sessions
            .first()
            .and_then(|item| load_session(&project.target_dir, Some(&item.id)).ok());

        Ok(Self {
            route: Route::Home,
            selected_field: 0,
            editing: false,
            history_index: 0,
            notices: vec![
                "v5 已启用：`codex-forge` 裸命令直接进入主界面。".to_string(),
                "快捷键：1-5 切页，e 编辑，r 运行，p 规划，d 预检，l 回放，s 停止，[ / ] / Tab 切执行子视图。".to_string(),
            ],
            form,
            project,
            selected_session,
            runtime_state: None,
            run_subview: RunSubview::Dashboard,
            last_doctor_report: None,
            active_command: None,
            should_quit: false,
        })
    }

    fn render(&self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(6),
            ])
            .split(area);

        frame.render_widget(self.header_widget(), layout[0]);
        frame.render_widget(self.tabs_widget(), layout[1]);
        self.render_body(frame, layout[2]);
        frame.render_widget(self.footer_widget(), layout[3]);

        if self.editing {
            let popup = centered_rect(70, 22, area);
            frame.render_widget(Clear, popup);
            frame.render_widget(self.edit_popup(), popup);
        }
    }

    fn header_widget(&self) -> Paragraph<'static> {
        let status = self
            .active_command
            .as_ref()
            .map(|command| {
                format!(
                    "动作：{} / {} / {}s",
                    command.action.label(),
                    command.state.label(),
                    command.started_at.elapsed().as_secs()
                )
            })
            .unwrap_or_else(|| "动作：空闲".to_string());

        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "◢ CODEX-FORGE V5 ◣",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled("Terminal Product Shell", Style::default().fg(Color::Yellow)),
            ]),
            Line::from(format!(
                "目标仓库：{}",
                truncate(&self.project.display_target, 100)
            )),
            Line::from(format!(
                "会话数：{}   角色集合：{}   {}",
                self.project.sessions.len(),
                self.form.role_set,
                status
            )),
        ])
        .block(Block::default().title("总览").borders(Borders::ALL))
        .wrap(Wrap { trim: true })
    }

    fn tabs_widget(&self) -> Tabs<'static> {
        let titles = Route::all()
            .iter()
            .map(|route| Line::from(route.label()))
            .collect::<Vec<_>>();
        Tabs::new(titles)
            .select(
                Route::all()
                    .iter()
                    .position(|item| *item == self.route)
                    .unwrap_or(0),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .block(Block::default().borders(Borders::ALL).title("导航"))
    }

    fn footer_widget(&self) -> Paragraph<'_> {
        let lines = self
            .notices
            .iter()
            .rev()
            .map(|item| Line::from(item.clone()))
            .collect::<Vec<_>>();
        Paragraph::new(lines)
            .block(Block::default().title("操作 / 提示").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn render_body(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        if self.route == Route::Run {
            self.render_run_route(frame, area);
            return;
        }

        let sections = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
            .split(area);

        match self.route {
            Route::Home => {
                frame.render_widget(self.home_left_widget(), sections[0]);
                frame.render_widget(self.home_right_widget(), sections[1]);
            }
            Route::Compose => {
                frame.render_widget(self.compose_left_widget(), sections[0]);
                frame.render_widget(self.compose_right_widget(), sections[1]);
            }
            Route::Run => unreachable!("run route 由 render_run_route 单独渲染"),
            Route::History => {
                frame.render_widget(self.history_left_widget(), sections[0]);
                frame.render_widget(self.history_right_widget(), sections[1]);
            }
            Route::Result => {
                frame.render_widget(self.result_left_widget(), sections[0]);
                frame.render_widget(self.result_right_widget(), sections[1]);
            }
        }
    }

    fn render_run_route(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(16),
                Constraint::Length(9),
            ])
            .split(area);

        frame.render_widget(self.run_subview_tabs(), sections[0]);

        // 执行页始终保持“一个顶部子视图栏 + 一个主面板 + 一个底部状态日志”结构，
        // 这样用户切换视图时不会丢失全局控制信息。
        match self.run_subview {
            RunSubview::Dashboard => {
                if let Some(runtime_state) = &self.runtime_state {
                    render_runtime_dashboard(frame, sections[1], runtime_state, "实时 Dashboard");
                } else {
                    frame.render_widget(self.run_placeholder_widget(), sections[1]);
                }
            }
            RunSubview::Timeline => {
                frame.render_widget(self.run_timeline_widget(), sections[1]);
            }
            RunSubview::Summary => {
                frame.render_widget(self.run_summary_widget(), sections[1]);
            }
        }

        frame.render_widget(self.run_log_widget(), sections[2]);
    }

    fn run_subview_tabs(&self) -> Tabs<'static> {
        let titles = RunSubview::all()
            .iter()
            .map(|item| Line::from(item.label()))
            .collect::<Vec<_>>();
        Tabs::new(titles)
            .select(
                RunSubview::all()
                    .iter()
                    .position(|item| *item == self.run_subview)
                    .unwrap_or(0),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .block(Block::default().borders(Borders::ALL).title("执行子视图"))
    }

    fn home_left_widget(&self) -> Paragraph<'_> {
        let mut lines = vec![
            Line::from(Span::styled(
                "演示闭环",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("1. 在“新任务”页改仓库、任务和运行参数"),
            Line::from("2. 按 `d` 做 doctor 预检"),
            Line::from("3. 按 `p` 生成计划，按 `r` 直接运行"),
            Line::from("4. 在“执行”页看日志，在“结果”页看 summary"),
            Line::from("5. 在“历史”页随时回放之前的 session"),
            Line::from(""),
            Line::from(format!("配置来源：{}", self.project.config_source)),
            Line::from(format!("全局规则：{}", self.project.rule_source)),
            Line::from(format!(
                "Reviewer 规则：{}",
                self.project.reviewer_rule_source
            )),
        ];
        if let Some(error) = &self.project.last_error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("当前告警：{}", truncate(error, 120)),
                Style::default().fg(Color::Red),
            )));
        }
        if let Some(report) = &self.last_doctor_report {
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "最近 Doctor：{} / {}",
                report.readiness.label(),
                report.summary
            )));
        }

        Paragraph::new(lines)
            .block(Block::default().title("产品入口").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn home_right_widget(&self) -> Table<'_> {
        let rows = self
            .project
            .sessions
            .iter()
            .take(8)
            .map(|item| {
                Row::new(vec![
                    item.id.clone(),
                    item.status.clone(),
                    truncate(&item.task, 30),
                    truncate(&item.result, 14),
                ])
            })
            .collect::<Vec<_>>();

        Table::new(
            rows,
            [
                Constraint::Length(18),
                Constraint::Length(10),
                Constraint::Length(32),
                Constraint::Min(12),
            ],
        )
        .header(
            Row::new(vec!["Session", "状态", "任务", "结果"]).style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(Block::default().title("最近 Session").borders(Borders::ALL))
        .column_spacing(1)
    }

    fn compose_left_widget(&self) -> List<'_> {
        let items = FormField::all()
            .iter()
            .enumerate()
            .map(|(index, field)| {
                let selected = index == self.selected_field;
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<18}", field.label()), style),
                    Span::raw(self.field_value(*field)),
                ]))
            })
            .collect::<Vec<_>>();

        List::new(items).block(Block::default().title("配置表单").borders(Borders::ALL))
    }

    fn compose_right_widget(&self) -> Paragraph<'_> {
        let preview = self.command_preview_lines(ShellAction::Run);
        let lines = vec![
            Line::from(format!("命中配置：{}", self.project.config_source)),
            Line::from(format!(
                "验证命令：{}",
                if self.project.verification_commands.is_empty() {
                    "无".to_string()
                } else {
                    truncate(&self.project.verification_commands.join("  |  "), 100)
                }
            )),
            Line::from(format!(
                "可选 role_set：{}",
                if self.project.role_sets.is_empty() {
                    "无".to_string()
                } else {
                    truncate(&self.project.role_sets.join("、"), 100)
                }
            )),
            Line::from(""),
            Line::from(Span::styled(
                "即将执行",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(preview.commandline),
            Line::from(""),
            Line::from("字段操作：↑↓ 选择，←→ 切换选项，e/Enter 编辑，Esc 结束编辑"),
            Line::from("动作快捷键：d doctor，p plan，r run，g 刷新项目上下文"),
        ];

        Paragraph::new(lines)
            .block(Block::default().title("配置预览").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn run_placeholder_widget(&self) -> Paragraph<'_> {
        let mut lines = vec![
            Line::from("当前没有运行中的内嵌会话。"),
            Line::from("按 `d` 运行 doctor，按 `p` 先看计划，按 `r` 直接执行。"),
            Line::from("一旦启动，可在 Dashboard / Timeline / Summary 三个子视图间切换。"),
        ];
        if let Some(session) = &self.selected_session {
            lines.push(Line::from(""));
            lines.extend(session_snapshot_lines(session));
        }
        Paragraph::new(lines)
            .block(Block::default().title("执行态空面板").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn run_log_widget(&self) -> Paragraph<'_> {
        let mut lines = Vec::new();
        if let Some(command) = &self.active_command {
            lines.push(Line::from(format!(
                "子视图：{}   热键：Tab / [ / ] 切换，s 停止",
                self.run_subview.label()
            )));
            lines.push(Line::from(format!("动作：{}", command.action.label())));
            lines.push(Line::from(format!("状态：{}", command.state.label())));
            lines.push(Line::from(format!(
                "命令：{}",
                truncate(&command.commandline, 120)
            )));
            lines.push(Line::from(""));
            for line in command.output.iter().rev().take(8).rev() {
                lines.push(Line::from(line.clone()));
            }
        } else {
            lines.push(Line::from(format!(
                "子视图：{}   热键：Tab / [ / ] 切换",
                self.run_subview.label()
            )));
            lines.push(Line::from("当前没有运行中的动作。"));
            lines.push(Line::from("可在任意页面直接按 `d` / `p` / `r` 启动。"));
            lines.push(Line::from(
                "运行中可按 `s` 发送停止信号（Run / Replay 支持）。",
            ));
        }
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title("执行状态 / 日志")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false })
    }

    fn run_timeline_widget(&self) -> Paragraph<'_> {
        let mut lines = Vec::new();
        if let Some(command) = &self.active_command {
            // 运行中优先展示实时事件流；结束后则回退到 session 自带的 timeline 摘要。
            lines.push(Line::from(format!(
                "实时事件流 / {} / {}",
                command.action.label(),
                command.state.label()
            )));
            lines.push(Line::from(""));
            for line in command.output.iter().rev().take(36).rev() {
                lines.push(Line::from(line.clone()));
            }
        } else if let Some(session) = &self.selected_session {
            lines.push(Line::from(format!("Session：{}", session.id)));
            lines.push(Line::from(""));
            if session.timeline_events.is_empty() {
                lines.push(Line::from("该 session 还没有 timeline 事件。"));
            } else {
                for item in session.timeline_events.iter().rev().take(36).rev() {
                    lines.push(Line::from(format!(
                        "{}  {} / {}",
                        item.ts.format("%H:%M:%S"),
                        item.title,
                        truncate(&item.detail, 88)
                    )));
                }
            }
        } else {
            lines.push(Line::from("暂无可展示的 timeline。"));
            lines.push(Line::from(
                "先执行一次 Run / Plan / Replay，即可在这里回看事件流。",
            ));
        }

        Paragraph::new(lines)
            .block(Block::default().title("Timeline").borders(Borders::ALL))
            .wrap(Wrap { trim: false })
    }

    fn run_summary_widget(&self) -> Paragraph<'_> {
        let mut lines = Vec::new();
        // Summary 视图优先用当前运行态的实时 summary；
        // 如果当前没有，再回退到已选 session 的 final summary，兼容历史查看与 replay 后查看。
        if let Some(summary) = self
            .runtime_state
            .as_ref()
            .and_then(|state| state.summary.as_ref())
            .or_else(|| {
                self.selected_session
                    .as_ref()
                    .and_then(|session| session.final_summary.as_ref())
            })
        {
            lines.push(Line::from(Span::styled(
                "最终收敛摘要",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(summary.overview.clone()));
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "结果：{}   Apply：{}   可信度：{}",
                summary.result_status.label(),
                summary.apply_status.label(),
                summary.trust_level.label()
            )));
            lines.push(Line::from(format!(
                "接收文件：{}",
                if summary.accepted_files.is_empty() {
                    "无".to_string()
                } else {
                    truncate(&summary.accepted_files.join("；"), 120)
                }
            )));
            lines.push(Line::from(format!(
                "人工复核：{}",
                if summary.manual_review_files.is_empty() {
                    "无".to_string()
                } else {
                    truncate(&summary.manual_review_files.join("；"), 120)
                }
            )));
            lines.push(Line::from(format!(
                "开放风险：{}",
                if summary.open_risks.is_empty() {
                    "无".to_string()
                } else {
                    truncate(&summary.open_risks.join("；"), 120)
                }
            )));
        } else if let Some(runtime_state) = &self.runtime_state {
            lines.push(Line::from("Summary 尚未生成。"));
            lines.push(Line::from(format!("当前阶段：{}", runtime_state.phase)));
            lines.push(Line::from(format!(
                "Apply：{}",
                runtime_state
                    .apply_status
                    .clone()
                    .unwrap_or_else(|| "等待".to_string())
            )));
            lines.push(Line::from(format!(
                "验证：{}",
                runtime_state
                    .verify_status
                    .clone()
                    .unwrap_or_else(|| "等待".to_string())
            )));
            lines.push(Line::from(
                "可先切到 Dashboard 查看 worker / todo / review 实时态势。",
            ));
        } else if let Some(session) = &self.selected_session {
            lines.push(Line::from(format!("Session：{}", session.id)));
            lines.push(Line::from(format!("状态：{}", session.status.label())));
            lines.push(Line::from("该 session 还没有 final summary。"));
        } else {
            lines.push(Line::from("暂无 summary。"));
            lines.push(Line::from(
                "先运行或回放一个 session，再来这里看最终收敛结果。",
            ));
        }

        Paragraph::new(lines)
            .block(Block::default().title("Summary").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn history_left_widget(&self) -> List<'_> {
        let items = self
            .project
            .sessions
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let style = if index == self.history_index {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{}  ", item.created_at), style),
                    Span::raw(format!(
                        "{} / {} / {}",
                        truncate(&item.id, 18),
                        item.status,
                        truncate(&item.task, 20)
                    )),
                ]))
            })
            .collect::<Vec<_>>();
        List::new(items).block(Block::default().title("历史 Session").borders(Borders::ALL))
    }

    fn history_right_widget(&self) -> Paragraph<'_> {
        let lines = if let Some(session) = &self.selected_session {
            session_snapshot_lines(session)
        } else {
            vec![Line::from("按 Enter 打开选中的 session 详情。")]
        };
        Paragraph::new(lines)
            .block(Block::default().title("Session 详情").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn result_left_widget(&self) -> Paragraph<'_> {
        let lines = if let Some(session) = &self.selected_session {
            session_snapshot_lines(session)
        } else {
            vec![Line::from("尚未载入结果。")]
        };
        Paragraph::new(lines)
            .block(Block::default().title("结果摘要").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn result_right_widget(&self) -> Paragraph<'_> {
        let mut lines = Vec::new();
        if let Some(session) = &self.selected_session {
            if let Some(summary) = &session.final_summary {
                lines.push(Line::from(format!("总览：{}", summary.overview)));
                lines.push(Line::from(format!(
                    "结果：{}",
                    summary.result_status.label()
                )));
                lines.push(Line::from(format!(
                    "Apply：{}",
                    summary.apply_status.label()
                )));
                lines.push(Line::from(format!(
                    "可信度：{}",
                    summary.trust_level.label()
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(format!(
                    "接收文件：{}",
                    if summary.accepted_files.is_empty() {
                        "无".to_string()
                    } else {
                        truncate(&summary.accepted_files.join("；"), 100)
                    }
                )));
                lines.push(Line::from(format!(
                    "待人工复核：{}",
                    if summary.manual_review_files.is_empty() {
                        "无".to_string()
                    } else {
                        truncate(&summary.manual_review_files.join("；"), 100)
                    }
                )));
                lines.push(Line::from(format!(
                    "开放风险：{}",
                    if summary.open_risks.is_empty() {
                        "无".to_string()
                    } else {
                        truncate(&summary.open_risks.join("；"), 100)
                    }
                )));
            } else {
                lines.push(Line::from("该 session 还没有 final summary。"));
            }
        } else {
            lines.push(Line::from("请先从历史页选择一个 session。"));
        }

        Paragraph::new(lines)
            .block(Block::default().title("交付维度").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn edit_popup(&self) -> Paragraph<'_> {
        let field = FormField::all()[self.selected_field];
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("编辑字段：{}", field.label()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(self.field_value(field)),
            Line::from(""),
            Line::from("Enter / Esc 结束编辑，Backspace 删除，直接输入字符。"),
        ])
        .block(Block::default().title("编辑模式").borders(Borders::ALL))
        .wrap(Wrap { trim: true })
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }

        if self.editing {
            return self.handle_edit_key(key);
        }

        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
            }
            KeyCode::Char('1') => self.route = Route::Home,
            KeyCode::Char('2') => self.route = Route::Compose,
            KeyCode::Char('3') => self.route = Route::Run,
            KeyCode::Char('4') => self.route = Route::History,
            KeyCode::Char('5') => self.route = Route::Result,
            KeyCode::Char('g') => self.refresh_project(true)?,
            KeyCode::Char('d') => self.start_action(ShellAction::Doctor).await?,
            KeyCode::Char('p') => self.start_action(ShellAction::Plan).await?,
            KeyCode::Char('r') => self.start_action(ShellAction::Run).await?,
            KeyCode::Char('l') => self.start_action(ShellAction::ReplaySelected).await?,
            KeyCode::Char('s') => self.stop_active_command(),
            KeyCode::Char('[') => self.cycle_run_subview(false),
            KeyCode::Char(']') | KeyCode::Tab => self.cycle_run_subview(true),
            KeyCode::BackTab => self.cycle_run_subview(false),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Left => self.cycle_current_field(false),
            KeyCode::Right => self.cycle_current_field(true),
            KeyCode::Enter | KeyCode::Char('e') => match self.route {
                Route::Compose => {
                    if field_is_editable(FormField::all()[self.selected_field]) {
                        self.editing = true;
                    } else {
                        self.cycle_current_field(true);
                    }
                }
                Route::History => self.open_history_selection()?,
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    fn handle_edit_key(&mut self, key: KeyEvent) -> Result<()> {
        let field = FormField::all()[self.selected_field];
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.editing = false;
                if matches!(field, FormField::TargetDir | FormField::ConfigPath) {
                    self.refresh_project(true)?;
                }
            }
            KeyCode::Backspace => {
                self.field_buffer_mut(field).pop();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.field_buffer_mut(field).push(ch);
            }
            _ => {}
        }
        Ok(())
    }

    fn move_selection(&mut self, delta: isize) {
        match self.route {
            Route::Compose => {
                let len = FormField::all().len() as isize;
                self.selected_field =
                    ((self.selected_field as isize + delta).rem_euclid(len)) as usize;
            }
            Route::History => {
                let len = self.project.sessions.len().max(1) as isize;
                self.history_index =
                    ((self.history_index as isize + delta).rem_euclid(len)) as usize;
            }
            _ => {}
        }
    }

    fn cycle_current_field(&mut self, forward: bool) {
        if self.route != Route::Compose {
            return;
        }

        match FormField::all()[self.selected_field] {
            FormField::RoleSet => {
                if !self.project.role_sets.is_empty() {
                    let current = self
                        .project
                        .role_sets
                        .iter()
                        .position(|item| item == &self.form.role_set)
                        .unwrap_or(0);
                    let next = cycle_index(current, self.project.role_sets.len(), forward);
                    self.form.role_set = self.project.role_sets[next].clone();
                }
            }
            FormField::ApplyMode => {
                let modes = [ApplyMode::AutoSafe, ApplyMode::Bundle, ApplyMode::None];
                let current = modes
                    .iter()
                    .position(|item| *item == self.form.apply_mode)
                    .unwrap_or(0);
                self.form.apply_mode = modes[cycle_index(current, modes.len(), forward)];
            }
            FormField::Preset => {
                let presets = [None, Some(SessionPreset::FeatureDemo)];
                let current = presets
                    .iter()
                    .position(|item| *item == self.form.preset)
                    .unwrap_or(0);
                self.form.preset = presets[cycle_index(current, presets.len(), forward)];
            }
            FormField::FailFast => {
                self.form.fail_fast = !self.form.fail_fast;
            }
            FormField::CleanupSuccess => {
                self.form.cleanup_success = !self.form.cleanup_success;
            }
            _ => {}
        }
    }

    async fn start_action(&mut self, action: ShellAction) -> Result<()> {
        if self
            .active_command
            .as_ref()
            .is_some_and(|command| command.state.is_running())
        {
            self.push_notice("已有动作在执行，请等待当前命令结束。");
            self.route = Route::Run;
            return Ok(());
        }

        let command_preview = self.command_preview_lines(action);
        let (tx, rx) = mpsc::unbounded_channel();
        // 只有 Run / Replay 接了真正的协作式停止链路；
        // Doctor / Plan 目前仍然走“一次跑完”的简单路径。
        let (cancel_tx, cancel_rx) = if action.supports_stop() {
            let (cancel_tx, cancel_rx) = watch::channel(false);
            (Some(cancel_tx), Some(cancel_rx))
        } else {
            (None, None)
        };
        let commandline = command_preview.commandline.clone();
        if let Some(runtime_state) =
            prepare_runtime_state(action, &self.form, self.selected_session.as_ref())
        {
            self.runtime_state = Some(runtime_state);
        }
        self.run_subview = match action {
            ShellAction::ReplaySelected => RunSubview::Timeline,
            _ => RunSubview::Dashboard,
        };
        spawn_embedded_action(
            action,
            self.project.target_dir.clone(),
            self.form.clone(),
            self.selected_session.clone(),
            tx,
            cancel_rx,
        );
        self.active_command = Some(ActiveCommand {
            action,
            commandline,
            state: CommandState::Running,
            started_at: Instant::now(),
            output: vec!["内嵌执行已启动，等待实时事件…".to_string()],
            cancel_tx,
            rx,
        });
        self.route = Route::Run;
        Ok(())
    }

    fn poll_command_output(&mut self) -> Result<()> {
        let mut finished = None;
        if let Some(command) = &mut self.active_command {
            // 这里持续把后台线程发回来的运行事件，折叠到 UI 可消费的本地状态里。
            while let Ok(event) = command.rx.try_recv() {
                match event {
                    RunnerEvent::Line(line) => {
                        push_command_output(&mut command.output, line);
                    }
                    RunnerEvent::Runtime(event) => {
                        if let Some(runtime_state) = &mut self.runtime_state {
                            runtime_state.apply(&event);
                        }
                        push_command_output(&mut command.output, describe_runtime_event(&event));
                    }
                    RunnerEvent::Doctor(report) => {
                        self.last_doctor_report = Some(report.clone());
                        for check in &report.checks {
                            push_command_output(
                                &mut command.output,
                                format!(
                                    "[{}] {} - {}",
                                    check.status.label(),
                                    check.name,
                                    check.detail
                                ),
                            );
                        }
                    }
                    RunnerEvent::Finished { state, manifest } => {
                        command.state = state;
                        finished = Some((command.action, state, manifest));
                    }
                }
            }
        }

        if let Some((action, state, manifest)) = finished {
            if let Some(manifest) = manifest {
                if let Some(runtime_state) = &mut self.runtime_state {
                    runtime_state.set_identity(manifest.id.clone(), manifest.task.clone());
                }
                self.selected_session = Some(manifest);
            }
            self.push_notice(&format!("{} 已结束：{}", action.label(), state.label()));
            self.refresh_project(false)?;
            if !self.project.sessions.is_empty() {
                self.history_index = 0;
                self.open_history_selection()?;
                if matches!(state, CommandState::Succeeded)
                    && matches!(
                        action,
                        ShellAction::Run | ShellAction::Plan | ShellAction::ReplaySelected
                    )
                {
                    self.route = Route::Result;
                } else {
                    self.route = Route::Run;
                }
            }
        }
        Ok(())
    }

    fn stop_active_command(&mut self) {
        let Some(command) = &mut self.active_command else {
            self.push_notice("当前没有运行中的动作。");
            return;
        };
        if !command.state.is_running() {
            self.push_notice("当前动作已经结束，无需再次停止。");
            return;
        }
        let Some(cancel_tx) = &command.cancel_tx else {
            self.push_notice("当前动作暂不支持安全停止；目前支持 Run / Replay。");
            self.route = Route::Run;
            return;
        };
        // UI 层只负责发送停止信号，不直接粗暴 abort 后台任务；
        // 真正的停止、收敛与清理由 orchestrator / worker 协作完成。
        match cancel_tx.send(true) {
            Ok(_) => {
                push_command_output(
                    &mut command.output,
                    "已发送停止信号，等待在跑 worker / replay 安全退出…".to_string(),
                );
                self.push_notice("已发送停止信号。");
                self.route = Route::Run;
            }
            Err(_) => self.push_notice("停止信号发送失败，当前动作可能已经结束。"),
        }
    }

    fn cycle_run_subview(&mut self, forward: bool) {
        if self.route != Route::Run {
            return;
        }
        let current = RunSubview::all()
            .iter()
            .position(|item| *item == self.run_subview)
            .unwrap_or(0);
        let next = cycle_index(current, RunSubview::all().len(), forward);
        self.run_subview = RunSubview::all()[next];
    }

    fn refresh_project(&mut self, override_defaults: bool) -> Result<()> {
        let target_dir = PathBuf::from(self.form.target_dir.clone());
        let config_path = self.form.config_path.clone();
        self.project = load_project_context(
            &target_dir,
            optional_path(&config_path),
            &mut self.form,
            override_defaults,
        )?;
        remember_target_dir(&self.project.target_dir)?;
        if self.history_index >= self.project.sessions.len() {
            self.history_index = 0;
        }
        if let Some(item) = self.project.sessions.get(self.history_index) {
            self.selected_session = load_session(&self.project.target_dir, Some(&item.id)).ok();
        }
        Ok(())
    }

    fn open_history_selection(&mut self) -> Result<()> {
        if let Some(item) = self.project.sessions.get(self.history_index) {
            self.selected_session = Some(load_session(&self.project.target_dir, Some(&item.id))?);
            self.route = Route::Result;
        }
        Ok(())
    }

    fn push_notice(&mut self, message: &str) {
        self.notices.push(message.to_string());
        if self.notices.len() > MAX_NOTICE_LINES {
            let overflow = self.notices.len() - MAX_NOTICE_LINES;
            self.notices.drain(0..overflow);
        }
    }

    fn field_value(&self, field: FormField) -> String {
        match field {
            FormField::TargetDir => self.form.target_dir.clone(),
            FormField::ConfigPath => empty_to_dash(&self.form.config_path),
            FormField::Task => empty_to_dash(&self.form.task),
            FormField::RoleSet => self.form.role_set.clone(),
            FormField::Workers => self.form.workers.clone(),
            FormField::MaxRetries => self.form.max_retries.clone(),
            FormField::Model => empty_to_dash(&self.form.model),
            FormField::ApplyMode => self.form.apply_mode.label().to_string(),
            FormField::Preset => self
                .form
                .preset
                .map(|item| item.label().to_string())
                .unwrap_or_else(|| "none".to_string()),
            FormField::FailFast => bool_label(self.form.fail_fast),
            FormField::CleanupSuccess => bool_label(self.form.cleanup_success),
            FormField::ResumeSession => empty_to_dash(&self.form.resume_session_id),
        }
    }

    fn field_buffer_mut(&mut self, field: FormField) -> &mut String {
        match field {
            FormField::TargetDir => &mut self.form.target_dir,
            FormField::ConfigPath => &mut self.form.config_path,
            FormField::Task => &mut self.form.task,
            FormField::RoleSet => &mut self.form.role_set,
            FormField::Workers => &mut self.form.workers,
            FormField::MaxRetries => &mut self.form.max_retries,
            FormField::Model => &mut self.form.model,
            FormField::ResumeSession => &mut self.form.resume_session_id,
            FormField::ApplyMode
            | FormField::Preset
            | FormField::FailFast
            | FormField::CleanupSuccess => unreachable!("非文本字段不应进入编辑"),
        }
    }

    fn command_preview_lines(&self, action: ShellAction) -> CommandPreview {
        build_command_preview(
            &self.project.target_dir,
            &self.form,
            action,
            self.selected_session.as_ref(),
        )
    }
}

#[derive(Debug)]
struct CommandPreview {
    #[cfg_attr(not(test), allow(dead_code))]
    args: Vec<String>,
    commandline: String,
}

fn build_command_preview(
    target_dir: &Path,
    form: &FormState,
    action: ShellAction,
    selected_session: Option<&SessionManifest>,
) -> CommandPreview {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("codex-forge"));
    let mut args = Vec::<String>::new();
    match action {
        ShellAction::Doctor => {
            args.push("doctor".to_string());
            args.push("--target-dir".to_string());
            args.push(target_dir.display().to_string());
            if let Some(path) = optional_path(&form.config_path) {
                args.push("--config".to_string());
                args.push(path.display().to_string());
            }
            args.push("--demo".to_string());
            args.push("--apply-mode".to_string());
            args.push(form.apply_mode.label().to_string());
        }
        ShellAction::Plan => {
            args.push("plan".to_string());
            args.push(form.task.clone());
            append_shared_args(&mut args, target_dir, form);
            args.push("--ui".to_string());
            args.push("minimal".to_string());
        }
        ShellAction::Run => {
            args.push("run".to_string());
            args.push(form.task.clone());
            append_shared_args(&mut args, target_dir, form);
            args.push("--ui".to_string());
            args.push("minimal".to_string());
            args.push("--apply-mode".to_string());
            args.push(form.apply_mode.label().to_string());
            args.push("--max-retries".to_string());
            args.push(form.max_retries.clone());
            if let Some(preset) = form.preset {
                args.push("--preset".to_string());
                args.push(preset.label().to_string());
            }
            if form.fail_fast {
                args.push("--fail-fast".to_string());
            }
            if form.cleanup_success {
                args.push("--cleanup-success".to_string());
            }
            if !form.resume_session_id.trim().is_empty() {
                args.push("--resume".to_string());
                args.push(form.resume_session_id.trim().to_string());
            }
        }
        ShellAction::ReplaySelected => {
            args.push("replay".to_string());
            if let Some(session) = selected_session {
                args.push(session.id.clone());
            }
            args.push("--target-dir".to_string());
            args.push(target_dir.display().to_string());
            args.push("--ui".to_string());
            args.push("minimal".to_string());
            args.push("--timeline".to_string());
        }
    }

    let commandline = format!(
        "{} {}",
        exe.display(),
        args.iter()
            .map(|item| shell_escape(item))
            .collect::<Vec<_>>()
            .join(" ")
    );

    CommandPreview { args, commandline }
}

fn prepare_runtime_state(
    action: ShellAction,
    form: &FormState,
    selected_session: Option<&SessionManifest>,
) -> Option<RuntimeViewState> {
    match action {
        ShellAction::Doctor => None,
        ShellAction::Plan | ShellAction::Run => Some(RuntimeViewState::new("准备中", &form.task)),
        ShellAction::ReplaySelected => {
            selected_session.map(|session| RuntimeViewState::new(&session.id, &session.task))
        }
    }
}

/// 统一裁剪日志缓存，避免执行页无限增长导致 TUI 卡顿。
fn push_command_output(output: &mut Vec<String>, line: String) {
    output.push(line);
    if output.len() > MAX_LOG_LINES {
        let overflow = output.len() - MAX_LOG_LINES;
        output.drain(0..overflow);
    }
}

fn spawn_embedded_action(
    action: ShellAction,
    target_dir: PathBuf,
    form: FormState,
    selected_session: Option<SessionManifest>,
    tx: mpsc::UnboundedSender<RunnerEvent>,
    stop_rx: Option<watch::Receiver<bool>>,
) {
    tokio::spawn(async move {
        // AppShell 只负责把用户动作转换成后台 future；
        // 具体执行仍然走现有 orchestrator / replay / doctor 逻辑，避免出现两套实现。
        let outcome = match action {
            ShellAction::Doctor => run_doctor_embedded(&target_dir, &form, tx.clone()).await,
            ShellAction::Plan => run_plan_embedded(&target_dir, &form, tx.clone()).await,
            ShellAction::Run => run_run_embedded(&target_dir, &form, tx.clone(), stop_rx).await,
            ShellAction::ReplaySelected => {
                let session_id = selected_session.as_ref().map(|session| session.id.as_str());
                replay_session_embedded(&target_dir, session_id, runtime_tx(&tx), stop_rx)
                    .await
                    .map(|(manifest, stopped)| {
                        (
                            if stopped {
                                CommandState::Stopped
                            } else {
                                CommandState::Succeeded
                            },
                            Some(manifest),
                        )
                    })
            }
        };

        match outcome {
            Ok((state, manifest)) => {
                let _ = tx.send(RunnerEvent::Finished { state, manifest });
            }
            Err(error) => {
                let _ = tx.send(RunnerEvent::Line(format!("执行失败：{error:#}")));
                let _ = tx.send(RunnerEvent::Finished {
                    state: CommandState::Failed,
                    manifest: None,
                });
            }
        }
    });
}

fn runtime_tx(tx: &mpsc::UnboundedSender<RunnerEvent>) -> mpsc::UnboundedSender<RuntimeEvent> {
    let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel::<RuntimeEvent>();
    let forward_tx = tx.clone();
    tokio::spawn(async move {
        // 把底层 RuntimeEvent 再封装成 RunnerEvent，统一回灌给 AppShell。
        while let Some(event) = runtime_rx.recv().await {
            let _ = forward_tx.send(RunnerEvent::Runtime(event));
        }
    });
    runtime_tx
}

async fn run_doctor_embedded(
    target_dir: &Path,
    form: &FormState,
    tx: mpsc::UnboundedSender<RunnerEvent>,
) -> Result<(CommandState, Option<SessionManifest>)> {
    let loaded = load_project_config(target_dir, optional_path(&form.config_path))?;
    let resources = load_resource_catalog(target_dir)?;
    let report = run_doctor(target_dir, &loaded, &resources, Some(form.apply_mode), true).await?;
    let ok = report.ok;
    let _ = tx.send(RunnerEvent::Doctor(report));
    if !ok {
        bail!("doctor 检查未通过");
    }
    Ok((CommandState::Succeeded, None))
}

async fn run_plan_embedded(
    target_dir: &Path,
    form: &FormState,
    tx: mpsc::UnboundedSender<RunnerEvent>,
) -> Result<(CommandState, Option<SessionManifest>)> {
    let args = build_plan_args(target_dir, form);
    let (config, roles) = resolve_plan_config(args)?;
    let manifest = plan_session_embedded(config, roles, runtime_tx(&tx)).await?;
    Ok((CommandState::Succeeded, Some(manifest)))
}

async fn run_run_embedded(
    target_dir: &Path,
    form: &FormState,
    tx: mpsc::UnboundedSender<RunnerEvent>,
    stop_rx: Option<watch::Receiver<bool>>,
) -> Result<(CommandState, Option<SessionManifest>)> {
    let args = build_run_args(target_dir, form);
    let (config, roles) = resolve_run_config(args)?;
    // run_session_embedded 会返回“是否是用户主动停止”的结果，
    // 这样 TUI 可以把结束态区分为成功 / 失败 / 已停止，而不是全部塞成失败。
    let EmbeddedRunOutcome { manifest, stopped } =
        run_session_embedded(config, roles, runtime_tx(&tx), stop_rx).await?;
    Ok((
        if stopped {
            CommandState::Stopped
        } else {
            CommandState::Succeeded
        },
        Some(manifest),
    ))
}

fn build_plan_args(target_dir: &Path, form: &FormState) -> PlanArgs {
    PlanArgs {
        shared: SharedTaskArgs {
            task: form.task.clone(),
            config: optional_path(&form.config_path).map(PathBuf::from),
            workers: parse_usize(&form.workers),
            role_set: Some(form.role_set.clone()),
            model: empty_as_none(&form.model),
            ui: UiModeArg::Minimal,
            target_dir: Some(target_dir.to_path_buf()),
        },
        config_only: false,
    }
}

fn build_run_args(target_dir: &Path, form: &FormState) -> RunArgs {
    RunArgs {
        shared: SharedTaskArgs {
            task: form.task.clone(),
            config: optional_path(&form.config_path).map(PathBuf::from),
            workers: parse_usize(&form.workers),
            role_set: Some(form.role_set.clone()),
            model: empty_as_none(&form.model),
            ui: UiModeArg::Minimal,
            target_dir: Some(target_dir.to_path_buf()),
        },
        preset: form.preset.map(|preset| match preset {
            SessionPreset::FeatureDemo => crate::cli::PresetArg::FeatureDemo,
        }),
        resume: empty_as_none(&form.resume_session_id),
        apply_mode: Some(match form.apply_mode {
            ApplyMode::AutoSafe => ApplyModeArg::AutoSafe,
            ApplyMode::Bundle => ApplyModeArg::Bundle,
            ApplyMode::None => ApplyModeArg::None,
        }),
        max_retries: parse_usize(&form.max_retries),
        fail_fast: form.fail_fast,
        cleanup_success: form.cleanup_success,
    }
}

fn parse_usize(value: &str) -> Option<usize> {
    value.trim().parse::<usize>().ok()
}

fn empty_as_none(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn load_project_context(
    target_dir: &Path,
    explicit_config: Option<&Path>,
    form: &mut FormState,
    override_defaults: bool,
) -> Result<ProjectContext> {
    let resolved = resolve_target_dir(Some(target_dir))
        .or_else(|_| resolve_target_dir(None))
        .context("解析目标仓库失败")?;
    let display_target = resolved.path.display().to_string();
    let loaded = load_project_config(&resolved.path, explicit_config)?;
    let resources = load_resource_catalog(&resolved.path)?;
    hydrate_form_from_loaded(form, &loaded, &resources, override_defaults);
    let sessions = load_session_summaries(&resolved.path).unwrap_or_default();

    Ok(ProjectContext {
        target_dir: resolved.path,
        display_target,
        config_source: loaded
            .path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "使用内置默认值".to_string()),
        verification_commands: loaded.settings.verification_commands.clone(),
        role_sets: sorted_role_sets(&resources),
        rule_source: resources.rules.global_origin.describe(),
        reviewer_rule_source: resources
            .rules
            .reviewer_origin
            .as_ref()
            .map(|origin| origin.describe())
            .unwrap_or_else(|| "未配置".to_string()),
        sessions,
        last_error: None,
    })
}

fn hydrate_form_from_loaded(
    form: &mut FormState,
    loaded: &LoadedProjectConfig,
    resources: &ResourceCatalog,
    override_defaults: bool,
) {
    if override_defaults || form.role_set.trim().is_empty() {
        form.role_set = loaded.settings.role_set.clone();
    }
    if override_defaults || form.workers.trim().is_empty() {
        form.workers = loaded.settings.workers.to_string();
    }
    if override_defaults || form.max_retries.trim().is_empty() {
        form.max_retries = loaded.settings.max_retries.to_string();
    }
    if override_defaults || form.model.trim().is_empty() {
        form.model = loaded.settings.model.clone().unwrap_or_default();
    }
    if override_defaults {
        form.apply_mode = loaded.settings.apply_mode;
        form.fail_fast = loaded.settings.fail_fast;
        form.cleanup_success = loaded.settings.cleanup_success;
        form.preset = Some(SessionPreset::FeatureDemo);
    }

    if form.role_set.trim().is_empty() {
        form.role_set = resources
            .role_sets
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "default".to_string());
    }
}

fn load_session_summaries(target_dir: &Path) -> Result<Vec<SessionSummary>> {
    let sessions_root = session_root(target_dir);
    if !sessions_root.exists() {
        return Ok(Vec::new());
    }

    let mut items = fs::read_dir(&sessions_root)
        .with_context(|| format!("读取 session 根目录失败：{}", sessions_root.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|path| {
            let manifest_path = path.join("manifest.json");
            let raw = fs::read_to_string(&manifest_path).ok()?;
            let manifest = serde_json::from_str::<SessionManifest>(&raw).ok()?;
            Some(SessionSummary {
                id: manifest.id.clone(),
                created_at: manifest.created_at.format("%m-%d %H:%M").to_string(),
                task: manifest.task.clone(),
                status: manifest.status.label().to_string(),
                result: manifest
                    .final_summary
                    .as_ref()
                    .map(|summary| summary.result_status.label().to_string())
                    .unwrap_or_else(|| "未总结".to_string()),
            })
        })
        .collect::<Vec<_>>();
    items.sort_by(|left, right| right.id.cmp(&left.id));
    Ok(items)
}

fn session_root(target_dir: &Path) -> PathBuf {
    discover_repo_root(target_dir)
        .unwrap_or_else(|| target_dir.to_path_buf())
        .join(".codex-forge")
        .join("sessions")
}

fn discover_repo_root(target_dir: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(target_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

fn sorted_role_sets(resources: &ResourceCatalog) -> Vec<String> {
    let mut items = resources.role_sets.keys().cloned().collect::<Vec<_>>();
    items.sort();
    items
}

fn append_shared_args(args: &mut Vec<String>, target_dir: &Path, form: &FormState) {
    args.push("--target-dir".to_string());
    args.push(target_dir.display().to_string());
    if let Some(path) = optional_path(&form.config_path) {
        args.push("--config".to_string());
        args.push(path.display().to_string());
    }
    args.push("--workers".to_string());
    args.push(form.workers.clone());
    args.push("--role-set".to_string());
    args.push(form.role_set.clone());
    if !form.model.trim().is_empty() {
        args.push("--model".to_string());
        args.push(form.model.trim().to_string());
    }
}

fn optional_path(value: &str) -> Option<&Path> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(Path::new(trimmed))
    }
}

fn bool_label(value: bool) -> String {
    if value {
        "开启".to_string()
    } else {
        "关闭".to_string()
    }
}

fn empty_to_dash(value: &str) -> String {
    if value.trim().is_empty() {
        "—".to_string()
    } else {
        value.to_string()
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

fn field_is_editable(field: FormField) -> bool {
    matches!(
        field,
        FormField::TargetDir
            | FormField::ConfigPath
            | FormField::Task
            | FormField::RoleSet
            | FormField::Workers
            | FormField::MaxRetries
            | FormField::Model
            | FormField::ResumeSession
    )
}

fn shell_escape(value: &str) -> String {
    if value.contains(' ') {
        format!("{value:?}")
    } else {
        value.to_string()
    }
}

fn cycle_index(current: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if forward {
        (current + 1) % len
    } else {
        (current + len - 1) % len
    }
}

fn centered_rect(horizontal: u16, vertical: u16, area: Rect) -> Rect {
    let vertical_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - vertical) / 2),
            Constraint::Percentage(vertical),
            Constraint::Percentage((100 - vertical) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - horizontal) / 2),
            Constraint::Percentage(horizontal),
            Constraint::Percentage((100 - horizontal) / 2),
        ])
        .split(vertical_layout[1])[1]
}

fn session_snapshot_lines(session: &SessionManifest) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(format!("Session：{}", session.id)),
        Line::from(format!("任务：{}", truncate(&session.task, 100))),
        Line::from(format!("状态：{}", session.status.label())),
        Line::from(format!(
            "创建时间：{}",
            session.created_at.format("%Y-%m-%d %H:%M:%S")
        )),
        Line::from(format!("role_set：{}", session.role_set)),
        Line::from(format!("apply_mode：{}", session.apply_mode.label())),
    ];

    if let Some(summary) = &session.final_summary {
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "总览：{}",
            truncate(&summary.overview, 110)
        )));
        lines.push(Line::from(format!(
            "结果：{}",
            summary.result_status.label()
        )));
        lines.push(Line::from(format!(
            "Apply：{}",
            summary.apply_status.label()
        )));
        lines.push(Line::from(format!(
            "验证能力：{}",
            if summary.verified_capabilities.is_empty() {
                "无".to_string()
            } else {
                truncate(&summary.verified_capabilities.join("；"), 100)
            }
        )));
        lines.push(Line::from(format!(
            "开放风险：{}",
            if summary.open_risks.is_empty() {
                "无".to_string()
            } else {
                truncate(&summary.open_risks.join("；"), 100)
            }
        )));
    } else if let Some(report) = &session.doctor_report {
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "Doctor：{} / {}",
            report.readiness.label(),
            report.summary
        )));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::{FormState, ShellAction, build_command_preview};
    use crate::model::{ApplyMode, SessionPreset};
    use std::path::Path;

    #[test]
    fn builds_run_command_from_form() {
        let form = FormState {
            target_dir: "/tmp/demo".to_string(),
            config_path: String::new(),
            task: "实现 v5".to_string(),
            role_set: "default".to_string(),
            workers: "4".to_string(),
            max_retries: "2".to_string(),
            model: "gpt-5".to_string(),
            apply_mode: ApplyMode::Bundle,
            preset: Some(SessionPreset::FeatureDemo),
            fail_fast: true,
            cleanup_success: true,
            resume_session_id: "abc".to_string(),
        };

        let preview = build_command_preview(Path::new("/tmp/demo"), &form, ShellAction::Run, None);
        assert!(preview.args.contains(&"run".to_string()));
        assert!(preview.args.contains(&"实现 v5".to_string()));
        assert!(preview.args.contains(&"--apply-mode".to_string()));
        assert!(preview.args.contains(&"bundle".to_string()));
        assert!(preview.args.contains(&"--resume".to_string()));
        assert!(preview.args.contains(&"abc".to_string()));
    }

    #[test]
    fn builds_replay_command_with_timeline() {
        let form = FormState {
            task: "x".to_string(),
            ..FormState::default()
        };
        let preview = build_command_preview(
            Path::new("/tmp/demo"),
            &form,
            ShellAction::ReplaySelected,
            None,
        );
        assert!(preview.args.contains(&"replay".to_string()));
        assert!(preview.args.contains(&"--timeline".to_string()));
    }
}
