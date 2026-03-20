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
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Tabs, Wrap};
use tokio::sync::{mpsc, watch};
use unicode_width::UnicodeWidthChar;

use crate::app::{resolve_plan_config, resolve_run_config};
use crate::cli::{ApplyModeArg, PlanArgs, RunArgs, SharedTaskArgs, ThinkingModeArg, UiModeArg};
use crate::config::{LoadedProjectConfig, load_project_config};
use crate::doctor::run_doctor;
use crate::model::{
    ApplyMode, DoctorReport, RuntimeEvent, SessionManifest, SessionPreset, ThinkingMode,
};
use crate::orchestrator::{EmbeddedRunOutcome, plan_session_embedded, run_session_embedded};
use crate::replay::replay_session_embedded;
use crate::resources::{ResourceCatalog, load_resource_catalog};
use crate::session::load_session;
use crate::time::format_beijing;
use crate::ui::{RuntimeViewState, describe_runtime_event, render_runtime_dashboard};
use crate::workspace::{remember_target_dir, resolve_target_dir};

const MAX_LOG_LINES: usize = 240;
const MAX_NOTICE_LINES: usize = 8;

/// v5 终端产品主导航。保留固定 5 个页面，确保演示路径稳定可记忆。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Start,
    Run,
    History,
}

impl Route {
    fn label(self) -> &'static str {
        match self {
            Self::Start => "开始",
            Self::Run => "执行中",
            Self::History => "历史结果",
        }
    }

    fn all() -> [Self; 3] {
        [Self::Start, Self::Run, Self::History]
    }
}

/// “新任务”页的表单字段定义。
/// 这里把终端产品中所有可配项显式列出来，便于统一渲染、编辑和命令预览。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormField {
    TargetDir,
    ConfigPath,
    Task,
    ThinkingMode,
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
            Self::ThinkingMode => "任务强度",
            Self::RoleSet => "协作模板",
            Self::Workers => "并发 Worker",
            Self::MaxRetries => "最大重试",
            Self::Model => "模型",
            Self::ApplyMode => "结果落地",
            Self::Preset => "演示预设",
            Self::FailFast => "Fail Fast",
            Self::CleanupSuccess => "清理成功 worktree",
            Self::ResumeSession => "继续上次会话",
        }
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
            Self::Doctor => "检查环境",
            Self::Plan => "先看方案",
            Self::Run => "开始执行",
            Self::ReplaySelected => "回放过程",
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
            Self::Dashboard => "实时态势",
            Self::Timeline => "事件流",
            Self::Summary => "交付摘要",
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
        manifest: Box<Option<SessionManifest>>,
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
    sessions: Vec<SessionSummary>,
    last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct FormState {
    target_dir: String,
    config_path: String,
    task: String,
    thinking_mode: ThinkingMode,
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
            thinking_mode: ThinkingMode::Balanced,
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

#[derive(Debug, Clone)]
struct EditState {
    field: FormField,
    buffer: String,
    cursor: usize,
    preferred_column: Option<usize>,
}

struct AppShell {
    route: Route,
    history_return_route: Route,
    run_return_route: Route,
    selected_field: usize,
    advanced_settings_open: bool,
    edit_state: Option<EditState>,
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
            route: Route::Start,
            history_return_route: Route::Start,
            run_return_route: Route::Start,
            selected_field: 0,
            advanced_settings_open: false,
            edit_state: None,
            history_index: 0,
            notices: vec![
                "默认只需要两步：先写任务，再选择“先看方案”或“开始执行”。".to_string(),
                "按 `a` 展开高级设置；旧的 CLI 参数和运行能力仍然保留兼容。".to_string(),
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
            .constraints(shell_layout_constraints(area))
            .split(area);

        frame.render_widget(self.header_widget(area.width), layout[0]);
        frame.render_widget(self.tabs_widget(area.width), layout[1]);
        self.render_body(frame, layout[2]);
        frame.render_widget(self.footer_widget(area.width), layout[3]);

        if self.edit_state.is_some() {
            let popup = centered_rect(
                popup_percent(area.width, 70, 96),
                popup_percent(area.height, 22, 90),
                area,
            );
            frame.render_widget(Clear, popup);
            self.render_edit_popup(frame, popup);
        }
    }

    fn header_widget(&self, width: u16) -> Paragraph<'static> {
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
        let doctor_status = self
            .last_doctor_report
            .as_ref()
            .map(|report| format!("环境：{} / {}", report.readiness.label(), report.summary))
            .unwrap_or_else(|| "环境：未检查".to_string());
        let advanced_status = if self.advanced_settings_open {
            "高级设置：已展开"
        } else {
            "高级设置：已收起"
        };

        let lines = if width < 72 {
            vec![
                Line::from(vec![
                    Span::styled(
                        "◢ CF V5 ◣",
                        Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        thinking_mode_user_title(self.form.thinking_mode),
                        Style::default()
                            .fg(thinking_mode_color(self.form.thinking_mode))
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(format!(
                    "{} | {} | {}",
                    truncate(&self.project.display_target, 20),
                    truncate(advanced_status, 12),
                    truncate(&status, 16)
                )),
            ]
        } else {
            vec![
                Line::from(vec![
                    Span::styled(
                        "◢ CODEX-FORGE V5 ◣",
                        Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled("低负担协作入口", Style::default().fg(Color::Yellow)),
                    Span::raw("  "),
                    Span::styled(
                        format!(
                            "{} / {}",
                            thinking_mode_user_title(self.form.thinking_mode),
                            thinking_mode_user_hint(self.form.thinking_mode)
                        ),
                        Style::default()
                            .fg(thinking_mode_color(self.form.thinking_mode))
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(format!(
                    "目标仓库：{}  |  {}",
                    truncate(&self.project.display_target, 56),
                    doctor_status,
                )),
                Line::from(format!(
                    "会话数：{}   {}   {}",
                    self.project.sessions.len(),
                    advanced_status,
                    status
                )),
            ]
        };

        Paragraph::new(lines)
            .block(Block::default().title("总览").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn tabs_widget(&self, width: u16) -> Tabs<'static> {
        let titles = route_titles(width)
            .into_iter()
            .map(Line::from)
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

    fn footer_widget(&self, width: u16) -> Paragraph<'_> {
        let mut lines = contextual_help_lines(
            self.route,
            self.edit_state.as_ref(),
            self.run_subview,
            width,
        );
        lines.push(Line::from(""));
        let notice_limit = if width < 72 {
            1
        } else if width < 100 {
            2
        } else {
            4
        };
        lines.extend(
            self.notices
                .iter()
                .rev()
                .take(notice_limit)
                .map(|item| Line::from(item.clone())),
        );
        Paragraph::new(lines)
            .block(Block::default().title("操作 / 提示").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn render_body(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        if self.route == Route::Run {
            self.render_run_route(frame, area);
            return;
        }

        let sections = split_main_sections(area);

        match self.route {
            Route::Start => self.render_start_route(frame, sections),
            Route::Run => unreachable!("run route 由 render_run_route 单独渲染"),
            Route::History => {
                frame.render_widget(self.history_left_widget(), sections[0]);
                frame.render_widget(self.history_right_widget(), sections[1]);
            }
        }
    }

    fn render_start_route(&self, frame: &mut ratatui::Frame<'_>, sections: Vec<Rect>) {
        frame.render_widget(self.start_main_widget(), sections[0]);
        if self.advanced_settings_open {
            let right_sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                .split(sections[1]);
            frame.render_widget(self.advanced_settings_widget(), right_sections[0]);
            frame.render_widget(self.advanced_details_widget(), right_sections[1]);
        } else {
            frame.render_widget(self.start_side_widget(), sections[1]);
        }
    }

    fn render_run_route(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints(run_route_constraints(area))
            .split(area);

        frame.render_widget(self.run_subview_tabs(area.width), sections[0]);

        // 执行页始终保持“一个顶部子视图栏 + 一个主面板 + 一个底部状态日志”结构，
        // 这样用户切换视图时不会丢失全局控制信息。
        match self.run_subview {
            RunSubview::Dashboard => {
                if let Some(runtime_state) = &self.runtime_state {
                    render_runtime_dashboard(frame, sections[1], runtime_state, "实时态势");
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

    fn run_subview_tabs(&self, width: u16) -> Tabs<'static> {
        let titles = run_subview_titles(width)
            .into_iter()
            .map(Line::from)
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

    fn start_main_widget(&self) -> Paragraph<'_> {
        let primary_preview = self.command_preview_lines(ShellAction::Run);
        let plan_preview = self.command_preview_lines(ShellAction::Plan);
        let mut lines = vec![
            Line::from(Span::styled(
                "新任务",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "任务：{}",
                if self.form.task.trim().is_empty() {
                    "还没有输入任务，按 `e` 开始写".to_string()
                } else {
                    truncate(&self.form.task, 160)
                }
            )),
            Line::from(format!(
                "任务强度：{} / {}",
                thinking_mode_user_title(self.form.thinking_mode),
                thinking_mode_user_hint(self.form.thinking_mode)
            )),
            Line::from(format!(
                "强度说明：{}",
                self.form.thinking_mode.description()
            )),
            Line::from(format!(
                "默认系统会自动处理：{}",
                advanced_settings_summary(&self.form)
            )),
            Line::from(""),
            Line::from(Span::styled(
                "主路径",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("1) 按 `e` 写下你要完成的任务"),
            Line::from("   编辑中：`Esc` 保存退出，`Ctrl+P` 直接先看方案，`Ctrl+R` 直接开始"),
            Line::from(format!("2) 按 `p` 先看方案：{}", plan_preview.summary)),
            Line::from(format!("3) 按 `r` 直接开始：{}", primary_preview.summary)),
            Line::from("4) 需要更细控制时，再按 `a` 展开高级设置"),
            Line::from(""),
            Line::from(format!("目标仓库：{}", self.project.display_target)),
            Line::from(format!("配置来源：{}", self.project.config_source)),
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
                "最近环境检查：{} / {}",
                report.readiness.label(),
                report.summary
            )));
        }

        Paragraph::new(lines)
            .block(Block::default().title("任务主路径").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn start_side_widget(&self) -> Paragraph<'_> {
        let preview = self.command_preview_lines(ShellAction::Run);
        let mut lines = vec![
            Line::from(Span::styled(
                "默认模式",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("默认只暴露最少决策：任务、任务强度、先方案/直接执行。"),
            Line::from("其它并发、模板、落地策略默认自动处理。"),
            Line::from("按 `a` 才展开高级设置；不展开也能直接运行。"),
            Line::from("任务编辑时也能直接操作：`Esc` 保存，`Ctrl+P` 规划，`Ctrl+R` 执行。"),
            Line::from(""),
            Line::from(Span::styled(
                "即将执行",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(truncate(&preview.summary, 120)),
            Line::from(""),
            Line::from(Span::styled(
                "最近 Session",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
        ];

        if self.project.sessions.is_empty() {
            lines.push(Line::from("还没有会话记录。"));
        } else {
            for item in self.project.sessions.iter().take(6) {
                lines.push(Line::from(format!(
                    "{}  {}  {}  / {}",
                    item.created_at,
                    truncate(&item.status, 8),
                    truncate(&item.task, 32),
                    truncate(&item.result, 14)
                )));
            }
        }

        Paragraph::new(lines)
            .block(Block::default().title("默认摘要").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn advanced_settings_widget(&self) -> List<'_> {
        let items = advanced_fields()
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

        List::new(items).block(
            Block::default()
                .title("高级设置（按 `a` 收起）")
                .borders(Borders::ALL),
        )
    }

    fn advanced_details_widget(&self) -> Paragraph<'_> {
        let preview = self.command_preview_lines(ShellAction::Run);
        let lines = vec![
            Line::from(format!("验证命令：{}", verification_summary(&self.project))),
            Line::from(format!(
                "可选协作模板：{}",
                if self.project.role_sets.is_empty() {
                    "无".to_string()
                } else {
                    truncate(&self.project.role_sets.join("、"), 100)
                }
            )),
            Line::from(""),
            Line::from("字段操作：↑↓ 选择，←→ 切换，e/Enter 编辑。"),
            Line::from("任务通常不用来这里调；这里只放速度、风险和兼容项。"),
            Line::from(""),
            Line::from(Span::styled(
                "完整命令预览",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(truncate(&preview.commandline, 120)),
        ];
        Paragraph::new(lines)
            .block(Block::default().title("高级说明").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn run_placeholder_widget(&self) -> Paragraph<'_> {
        let mut lines = vec![
            Line::from("当前没有正在进行的协作执行。"),
            Line::from(format!(
                "任务强度：{} / {}",
                thinking_mode_user_title(self.form.thinking_mode),
                thinking_mode_user_hint(self.form.thinking_mode)
            )),
            Line::from("按 `d` 检查环境，按 `p` 先看方案，按 `r` 直接开始。"),
            Line::from("开始后，可在“实时态势 / 事件流 / 交付摘要”之间切换。"),
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
        Paragraph::new(self.run_log_lines())
            .block(
                Block::default()
                    .title("执行状态 / 事件")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false })
    }

    fn run_log_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Some(command) = &self.active_command {
            let hotkey_hint = if command.state.is_running() && command.action.supports_stop() {
                "热键：Tab / [ / ] 切换，s 停止"
            } else if command.state.is_running() {
                "热键：Tab / [ / ] 切换，当前动作暂不支持中途停止"
            } else {
                "热键：Tab / [ / ] 切换，Esc 返回上一级"
            };
            lines.push(Line::from(format!(
                "子视图：{}   {}",
                self.run_subview.label(),
                hotkey_hint
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
                "子视图：{}   热键：Tab / [ / ] 切换，Esc 返回上一级",
                self.run_subview.label()
            )));
            lines.push(Line::from("当前没有运行中的动作。"));
            lines.push(Line::from("可在任意页面直接按 `d` / `p` / `r` 启动。"));
            lines.push(Line::from("历史页可按 `l` 回放已完成会话。"));
            lines.push(Line::from(
                "运行中可按 `s` 发送停止信号（开始执行 / 回放过程支持）。",
            ));
        }
        lines
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
            lines.push(Line::from(format!("历史会话：{}", session.id)));
            lines.push(Line::from(""));
            if session.timeline_events.is_empty() {
                lines.push(Line::from("该会话还没有事件流记录。"));
            } else {
                for item in session.timeline_events.iter().rev().take(36).rev() {
                    lines.push(Line::from(format!(
                        "{}  {} / {}",
                        format_beijing(item.ts, "%H:%M:%S"),
                        item.title,
                        truncate(&item.detail, 88)
                    )));
                }
            }
        } else {
            lines.push(Line::from("暂无可展示的事件流。"));
            lines.push(Line::from("先执行或回放一次任务，即可在这里回看关键过程。"));
        }

        Paragraph::new(lines)
            .block(Block::default().title("事件流").borders(Borders::ALL))
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
            lines.push(Line::from("交付摘要尚未生成。"));
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
                "可先切到“实时态势”查看 worker / todo / review 的当前进展。",
            ));
        } else if let Some(session) = &self.selected_session {
            lines.push(Line::from(format!("历史会话：{}", session.id)));
            lines.push(Line::from(format!("状态：{}", session.status.label())));
            lines.push(Line::from("该会话还没有最终交付摘要。"));
        } else {
            lines.push(Line::from("暂无交付摘要。"));
            lines.push(Line::from("先运行或回放一个会话，再来这里查看最终结果。"));
        }

        Paragraph::new(lines)
            .block(Block::default().title("交付摘要").borders(Borders::ALL))
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
        List::new(items).block(Block::default().title("历史会话").borders(Borders::ALL))
    }

    fn history_right_widget(&self) -> Paragraph<'_> {
        let lines = if let Some(session) = &self.selected_session {
            session_snapshot_lines(session)
        } else {
            vec![Line::from("按 Enter 打开选中的会话详情。")]
        };
        Paragraph::new(lines)
            .block(Block::default().title("历史结果详情").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn render_edit_popup(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let Some(edit) = &self.edit_state else {
            return;
        };
        let header_height = if edit.field == FormField::Task { 4 } else { 3 };
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(header_height), Constraint::Min(6)])
            .split(area);

        let help = if edit.field == FormField::Task {
            "任务描述支持多行输入；Enter 换行，Esc 保存退出，Ctrl+P 先看方案，Ctrl+R 直接执行。"
        } else {
            "方向键移动，Backspace/Delete 删除，Enter 保存，Esc 取消。"
        };
        let header_lines = vec![
            Line::from(Span::styled(
                format!("编辑字段：{}", edit.field.label()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "字符数：{}   光标：{}",
                edit.buffer.chars().count(),
                edit.cursor
            )),
            Line::from(help),
        ];
        frame.render_widget(
            Paragraph::new(header_lines)
                .block(Block::default().title("编辑模式").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            sections[0],
        );

        let input_text = if edit.buffer.is_empty() {
            " ".to_string()
        } else {
            edit.buffer.clone()
        };
        frame.render_widget(
            Paragraph::new(input_text)
                .block(Block::default().title("内容").borders(Borders::ALL))
                .wrap(Wrap { trim: false }),
            sections[1],
        );

        let inner_width = sections[1].width.saturating_sub(2) as usize;
        let (cursor_row, cursor_col) =
            wrapped_cursor_row_col(&edit.buffer, edit.cursor, inner_width);
        let x = sections[1]
            .x
            .saturating_add(1)
            .saturating_add(cursor_col as u16)
            .min(sections[1].right().saturating_sub(2));
        let y = sections[1]
            .y
            .saturating_add(1)
            .saturating_add(cursor_row as u16)
            .min(sections[1].bottom().saturating_sub(2));
        frame.set_cursor_position(Position::new(x, y));
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }

        if self.edit_state.is_some() {
            return self.handle_edit_key(key).await;
        }

        match key.code {
            KeyCode::Char('q') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('1') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate_via_tab(Route::Start)
            }
            KeyCode::Char('2') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate_to(Route::Run)
            }
            KeyCode::Char('3') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate_via_tab(Route::History)
            }
            KeyCode::Esc => self.handle_global_back(),
            KeyCode::Char('a') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_advanced_settings()
            }
            KeyCode::Char('g') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.refresh_project(true)?
            }
            KeyCode::Char('m') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.form.thinking_mode = cycle_thinking_mode(self.form.thinking_mode, true)
            }
            KeyCode::Char('d') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_action(ShellAction::Doctor).await?
            }
            KeyCode::Char('p') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_action(ShellAction::Plan).await?
            }
            KeyCode::Char('r') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_action(ShellAction::Run).await?
            }
            KeyCode::Char('l') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_action(ShellAction::ReplaySelected).await?
            }
            KeyCode::Char('s') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.stop_active_command()
            }
            KeyCode::Char('[') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cycle_run_subview(false)
            }
            KeyCode::Char(']') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cycle_run_subview(true)
            }
            KeyCode::Tab => self.cycle_run_subview(true),
            KeyCode::BackTab => self.cycle_run_subview(false),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Left => self.cycle_current_field(false),
            KeyCode::Right => self.cycle_current_field(true),
            KeyCode::Enter => match self.route {
                Route::Start => {
                    if self.advanced_settings_open {
                        let field = advanced_fields()[self.selected_field];
                        if field_is_editable(field) {
                            self.start_editing(field);
                        } else {
                            self.cycle_current_field(true);
                        }
                    } else {
                        self.start_editing(FormField::Task);
                    }
                }
                Route::History => self.open_history_selection()?,
                _ => {}
            },
            KeyCode::Char('e') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                match self.route {
                    Route::Start => {
                        if self.advanced_settings_open {
                            let field = advanced_fields()[self.selected_field];
                            if field_is_editable(field) {
                                self.start_editing(field);
                            } else {
                                self.cycle_current_field(true);
                            }
                        } else {
                            self.start_editing(FormField::Task);
                        }
                    }
                    Route::History => self.open_history_selection()?,
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_edit_key(&mut self, key: KeyEvent) -> Result<()> {
        let mut commit = false;
        let mut notice = None;
        let mut action_after_commit = None;

        {
            let Some(edit) = &mut self.edit_state else {
                return Ok(());
            };
            let multiline = edit.field == FormField::Task;
            match key.code {
                KeyCode::Esc if multiline => {
                    commit = true;
                    notice = Some("任务已保存，可按 `p` 先看方案，或按 `r` 直接开始。");
                }
                KeyCode::Esc => {
                    self.edit_state = None;
                    return Ok(());
                }
                KeyCode::Enter if multiline && !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    insert_char_at_cursor(edit, '\n');
                }
                KeyCode::Enter => {
                    commit = true;
                }
                KeyCode::Backspace => backspace_at_cursor(edit),
                KeyCode::Delete => delete_at_cursor(edit),
                KeyCode::Left => move_cursor_horizontal(edit, false),
                KeyCode::Right => move_cursor_horizontal(edit, true),
                KeyCode::Up => move_cursor_vertical(edit, false),
                KeyCode::Down => move_cursor_vertical(edit, true),
                KeyCode::Home => move_cursor_line_edge(edit, false),
                KeyCode::End => move_cursor_line_edge(edit, true),
                KeyCode::Tab if multiline => {
                    insert_char_at_cursor(edit, ' ');
                    insert_char_at_cursor(edit, ' ');
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    commit = true;
                    notice = Some("内容已保存。");
                }
                KeyCode::Char('p')
                    if multiline && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    commit = true;
                    action_after_commit = Some(ShellAction::Plan);
                }
                KeyCode::Char('r')
                    if multiline && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    commit = true;
                    action_after_commit = Some(ShellAction::Run);
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    insert_char_at_cursor(edit, ch);
                }
                _ => {}
            }
        }

        if commit {
            self.commit_edit()?;
            if let Some(message) = notice {
                self.push_notice(message);
            }
            if let Some(action) = action_after_commit {
                self.start_action(action).await?;
            }
        }

        Ok(())
    }

    fn move_selection(&mut self, delta: isize) {
        match self.route {
            Route::Start if self.advanced_settings_open => {
                let len = advanced_fields().len() as isize;
                self.selected_field =
                    ((self.selected_field as isize + delta).rem_euclid(len)) as usize;
            }
            Route::History => {
                let len = self.project.sessions.len().max(1) as isize;
                self.history_index =
                    ((self.history_index as isize + delta).rem_euclid(len)) as usize;
                if let Some(item) = self.project.sessions.get(self.history_index) {
                    self.selected_session =
                        load_session(&self.project.target_dir, Some(&item.id)).ok();
                }
            }
            _ => {}
        }
    }

    fn cycle_current_field(&mut self, forward: bool) {
        if self.route != Route::Start || !self.advanced_settings_open {
            return;
        }

        match advanced_fields()[self.selected_field] {
            FormField::ThinkingMode => {
                self.form.thinking_mode = cycle_thinking_mode(self.form.thinking_mode, forward);
            }
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

    fn toggle_advanced_settings(&mut self) {
        if self.is_command_running() {
            self.push_notice("当前动作仍在执行；请先等待结束，或按 `s` 停止后再调整高级设置。");
            self.navigate_to(Route::Run);
            return;
        }

        if self.route != Route::Start {
            self.advanced_settings_open = true;
            self.selected_field = 0;
            self.navigate_to(Route::Start);
            self.push_notice("已返回开始页，并展开高级设置。");
            return;
        }

        let opening = !self.advanced_settings_open;
        self.advanced_settings_open = opening;
        self.selected_field = 0;
        self.push_notice(if opening {
            "已展开高级设置：现在可以调整模板、并发和结果落地策略。"
        } else {
            "已收起高级设置：返回默认低负担模式。"
        });
    }

    async fn start_action(&mut self, action: ShellAction) -> Result<()> {
        if self
            .active_command
            .as_ref()
            .is_some_and(|command| command.state.is_running())
        {
            self.push_notice("已有动作在执行，请等待当前命令结束。");
            self.navigate_to(Route::Run);
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
        self.navigate_to(Route::Run);
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
                        finished = Some((command.action, state, *manifest));
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
                    && matches!(action, ShellAction::Run | ShellAction::Plan)
                {
                    self.navigate_to(Route::History);
                } else {
                    self.navigate_to(Route::Run);
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
            self.push_notice("当前动作暂不支持安全停止；目前支持“开始执行”和“回放过程”。");
            self.navigate_to(Route::Run);
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
                self.navigate_to(Route::Run);
            }
            Err(_) => self.push_notice("停止信号发送失败，当前动作可能已经结束。"),
        }
    }

    fn navigate_to(&mut self, route: Route) {
        self.history_return_route =
            next_history_return_route(self.route, self.history_return_route, route);
        self.run_return_route = next_run_return_route(self.route, self.run_return_route, route);
        self.route = route;
    }

    fn navigate_via_tab(&mut self, route: Route) {
        if self.is_command_running() && route != Route::Run {
            self.push_notice("当前动作仍在执行；请留在执行页查看进度，或按 `s` 停止。");
            self.navigate_to(Route::Run);
            return;
        }
        self.navigate_to(route);
    }

    fn leave_history(&mut self) {
        let target = history_back_route(self.history_return_route);
        self.route = target;
        self.push_notice(match target {
            Route::Start => "已返回开始页。",
            Route::Run => "已返回执行页。",
            Route::History => "已返回历史页。",
        });
    }

    fn handle_global_back(&mut self) {
        match self.route {
            Route::Start => {
                if self.advanced_settings_open {
                    self.advanced_settings_open = false;
                    self.selected_field = 0;
                    self.push_notice("已收起高级设置。");
                }
            }
            Route::Run => {
                if self
                    .active_command
                    .as_ref()
                    .is_some_and(|command| command.state.is_running())
                {
                    self.push_notice("当前动作仍在执行；如需中止请按 `s`。");
                } else {
                    let target = run_back_route(self.run_return_route);
                    self.route = target;
                    self.push_notice(match target {
                        Route::Start => "已返回开始页。",
                        Route::Run => "已返回执行页。",
                        Route::History => "已返回历史页。",
                    });
                }
            }
            Route::History => self.leave_history(),
        }
    }

    fn is_command_running(&self) -> bool {
        self.active_command
            .as_ref()
            .is_some_and(|command| command.state.is_running())
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
            self.navigate_to(Route::History);
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
            FormField::Task => {
                if self.form.task.trim().is_empty() {
                    "—".to_string()
                } else {
                    truncate(&self.form.task.replace('\n', " ⏎ "), 72)
                }
            }
            FormField::ThinkingMode => format!(
                "{} / {}",
                thinking_mode_user_title(self.form.thinking_mode),
                thinking_mode_user_hint(self.form.thinking_mode)
            ),
            FormField::RoleSet => self.form.role_set.clone(),
            FormField::Workers => self.form.workers.clone(),
            FormField::MaxRetries => self.form.max_retries.clone(),
            FormField::Model => empty_to_dash(&self.form.model),
            FormField::ApplyMode => apply_mode_user_label(self.form.apply_mode).to_string(),
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

    fn start_editing(&mut self, field: FormField) {
        let current = match field {
            FormField::TargetDir => self.form.target_dir.clone(),
            FormField::ConfigPath => self.form.config_path.clone(),
            FormField::Task => self.form.task.clone(),
            FormField::Workers => self.form.workers.clone(),
            FormField::MaxRetries => self.form.max_retries.clone(),
            FormField::Model => self.form.model.clone(),
            FormField::ResumeSession => self.form.resume_session_id.clone(),
            _ => return,
        };
        self.edit_state = Some(EditState {
            field,
            cursor: current.chars().count(),
            buffer: current,
            preferred_column: None,
        });
    }

    fn commit_edit(&mut self) -> Result<()> {
        let Some(edit) = self.edit_state.take() else {
            return Ok(());
        };
        match edit.field {
            FormField::TargetDir => self.form.target_dir = edit.buffer.trim().to_string(),
            FormField::ConfigPath => self.form.config_path = edit.buffer.trim().to_string(),
            FormField::Task => self.form.task = edit.buffer.trim().to_string(),
            FormField::Workers => self.form.workers = edit.buffer.trim().to_string(),
            FormField::MaxRetries => self.form.max_retries = edit.buffer.trim().to_string(),
            FormField::Model => self.form.model = edit.buffer.trim().to_string(),
            FormField::ResumeSession => {
                self.form.resume_session_id = edit.buffer.trim().to_string()
            }
            _ => {}
        }
        if matches!(edit.field, FormField::TargetDir | FormField::ConfigPath) {
            self.refresh_project(true)?;
        }
        Ok(())
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
    summary: String,
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
    let summary = build_action_summary(target_dir, form, action, selected_session);

    CommandPreview {
        args,
        commandline,
        summary,
    }
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
                let _ = tx.send(RunnerEvent::Finished {
                    state,
                    manifest: Box::new(manifest),
                });
            }
            Err(error) => {
                let _ = tx.send(RunnerEvent::Line(format!("执行失败：{error:#}")));
                let _ = tx.send(RunnerEvent::Finished {
                    state: CommandState::Failed,
                    manifest: Box::new(None),
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
            thinking_mode: Some(match form.thinking_mode {
                ThinkingMode::Quick => ThinkingModeArg::Quick,
                ThinkingMode::Balanced => ThinkingModeArg::Balanced,
                ThinkingMode::HardThink => ThinkingModeArg::HardThink,
            }),
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
            thinking_mode: Some(match form.thinking_mode {
                ThinkingMode::Quick => ThinkingModeArg::Quick,
                ThinkingMode::Balanced => ThinkingModeArg::Balanced,
                ThinkingMode::HardThink => ThinkingModeArg::HardThink,
            }),
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
    if override_defaults {
        form.thinking_mode = loaded.settings.thinking_mode;
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
                created_at: format_beijing(manifest.created_at, "%m-%d %H:%M"),
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
    args.push("--thinking-mode".to_string());
    args.push(form.thinking_mode.label().to_string());
}

fn build_action_summary(
    _target_dir: &Path,
    form: &FormState,
    action: ShellAction,
    selected_session: Option<&SessionManifest>,
) -> String {
    match action {
        ShellAction::Doctor => "检查当前仓库的环境、配置和验证条件".to_string(),
        ShellAction::Plan => format!(
            "先把“{}”拆成方案和待办清单，再决定怎么执行",
            summarize_task(&form.task)
        ),
        ShellAction::Run => format!(
            "直接开始处理“{}”，默认使用 {}",
            summarize_task(&form.task),
            advanced_settings_summary(form)
        ),
        ShellAction::ReplaySelected => format!(
            "回看 {} 的关键过程和结果",
            selected_session
                .map(|session| truncate(&session.id, 18))
                .unwrap_or_else(|| "最近一次会话".to_string())
        ),
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

fn summarize_task(task: &str) -> String {
    if task.trim().is_empty() {
        "当前任务".to_string()
    } else {
        truncate(task, 24)
    }
}

fn thinking_mode_user_title(mode: ThinkingMode) -> &'static str {
    match mode {
        ThinkingMode::Quick => "快速推进",
        ThinkingMode::Balanced => "平衡模式",
        ThinkingMode::HardThink => "复杂任务",
    }
}

fn thinking_mode_user_hint(mode: ThinkingMode) -> &'static str {
    match mode {
        ThinkingMode::Quick => "更快出结果",
        ThinkingMode::Balanced => "默认推荐",
        ThinkingMode::HardThink => "更重视边界和风险",
    }
}

fn apply_mode_user_label(mode: ApplyMode) -> &'static str {
    match mode {
        ApplyMode::AutoSafe => "自动安全落地",
        ApplyMode::Bundle => "只输出变更包",
        ApplyMode::None => "只做方案和审阅",
    }
}

fn advanced_fields() -> &'static [FormField] {
    &[
        FormField::TargetDir,
        FormField::ConfigPath,
        FormField::ThinkingMode,
        FormField::RoleSet,
        FormField::Workers,
        FormField::MaxRetries,
        FormField::Model,
        FormField::ApplyMode,
        FormField::Preset,
        FormField::FailFast,
        FormField::CleanupSuccess,
        FormField::ResumeSession,
    ]
}

fn advanced_settings_summary(form: &FormState) -> String {
    let mut items = vec![
        format!("{} 个 worker", form.workers),
        form.role_set.clone(),
        apply_mode_user_label(form.apply_mode).to_string(),
    ];
    if !form.max_retries.trim().is_empty() {
        items.push(format!("重试 {}", form.max_retries));
    }
    if let Some(preset) = form.preset {
        items.push(format!("预设 {}", preset.label()));
    }
    truncate(&items.join(" / "), 72)
}

fn verification_summary(project: &ProjectContext) -> String {
    if project.verification_commands.is_empty() {
        "未配置".to_string()
    } else {
        truncate(&project.verification_commands.join("  |  "), 100)
    }
}

fn field_is_editable(field: FormField) -> bool {
    matches!(
        field,
        FormField::TargetDir
            | FormField::ConfigPath
            | FormField::Task
            | FormField::Workers
            | FormField::MaxRetries
            | FormField::Model
            | FormField::ResumeSession
    )
}

fn cycle_thinking_mode(current: ThinkingMode, forward: bool) -> ThinkingMode {
    let modes = [
        ThinkingMode::Quick,
        ThinkingMode::Balanced,
        ThinkingMode::HardThink,
    ];
    let index = modes.iter().position(|item| *item == current).unwrap_or(1);
    modes[cycle_index(index, modes.len(), forward)]
}

fn thinking_mode_color(mode: ThinkingMode) -> Color {
    match mode {
        ThinkingMode::Quick => Color::LightBlue,
        ThinkingMode::Balanced => Color::LightGreen,
        ThinkingMode::HardThink => Color::LightMagenta,
    }
}

fn contextual_help_lines(
    route: Route,
    edit_state: Option<&EditState>,
    run_subview: RunSubview,
    width: u16,
) -> Vec<Line<'static>> {
    if let Some(edit) = edit_state {
        let hint = if edit.field == FormField::Task {
            if width < 72 {
                "编辑中：Esc 保存，Ctrl+P 规划，Ctrl+R 执行。"
            } else {
                "编辑中：Enter 换行，Esc 保存退出，Ctrl+P 先看方案，Ctrl+R 直接执行。"
            }
        } else if width < 72 {
            "编辑中：Enter 保存，Esc 取消。"
        } else {
            "编辑中：Enter 保存，Esc 取消，Backspace/Delete 删除。"
        };
        return vec![Line::from(hint)];
    }

    let compact = width < 72;
    match route {
        Route::Start => vec![Line::from(if compact {
            "开始页：e 写任务，a 高级，Esc 收起高级。"
        } else {
            "开始页：`e` 写任务，`m` 切任务强度，`a` 展开高级设置，`Esc` 收起高级，`d` / `p` / `r` 直接执行。"
        })],
        Route::Run => vec![Line::from(format!(
            "{}：{}",
            if compact {
                "执行页：Esc 返回上一级"
            } else {
                "执行页，`Esc` 返回上一级，Tab/[ ] 切换，s 停止"
            },
            run_subview.label()
        ))],
        Route::History => vec![Line::from(if compact {
            "历史页：↑↓ 预览，Enter 打开，Esc 返回上一级。"
        } else {
            "历史页：↑↓ 预览会话，Enter 打开结果详情，`l` 回放当前项，`Esc` 返回上一级。"
        })],
    }
}

fn shell_layout_constraints(area: Rect) -> Vec<Constraint> {
    if area.height < 22 {
        vec![
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(4),
        ]
    } else {
        vec![
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(6),
        ]
    }
}

fn route_titles(width: u16) -> Vec<&'static str> {
    if width < 60 {
        vec!["始", "执", "历"]
    } else if width < 90 {
        vec!["开始", "执行中", "历史结果"]
    } else {
        Route::all().iter().map(|route| route.label()).collect()
    }
}

fn run_subview_titles(width: u16) -> Vec<&'static str> {
    if width < 68 {
        vec!["态势", "事件", "摘要"]
    } else {
        RunSubview::all().iter().map(|item| item.label()).collect()
    }
}

fn split_main_sections(area: Rect) -> Vec<Rect> {
    let direction = if area.width < 110 {
        Direction::Vertical
    } else {
        Direction::Horizontal
    };
    let constraints = if matches!(direction, Direction::Horizontal) {
        vec![Constraint::Percentage(48), Constraint::Percentage(52)]
    } else if area.height < 18 {
        vec![Constraint::Percentage(52), Constraint::Percentage(48)]
    } else {
        vec![Constraint::Percentage(46), Constraint::Percentage(54)]
    };
    Layout::default()
        .direction(direction)
        .constraints(constraints)
        .split(area)
        .to_vec()
}

fn run_route_constraints(area: Rect) -> Vec<Constraint> {
    if area.height < 18 {
        vec![
            Constraint::Length(3),
            Constraint::Percentage(62),
            Constraint::Percentage(38),
        ]
    } else if area.height < 26 {
        vec![
            Constraint::Length(3),
            Constraint::Percentage(68),
            Constraint::Percentage(32),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Min(16),
            Constraint::Length(9),
        ]
    }
}

fn popup_percent(size: u16, desired: u16, max: u16) -> u16 {
    if size < 60 {
        max
    } else if size < 90 {
        desired.max(80)
    } else {
        desired
    }
}

fn shell_escape(value: &str) -> String {
    if value.contains(' ') {
        format!("{value:?}")
    } else {
        value.to_string()
    }
}

fn next_history_return_route(current: Route, stored: Route, next: Route) -> Route {
    if next == Route::History {
        if current == Route::History {
            stored
        } else {
            current
        }
    } else {
        next
    }
}

fn next_run_return_route(current: Route, stored: Route, next: Route) -> Route {
    if next == Route::Run {
        if current == Route::Run {
            stored
        } else {
            current
        }
    } else {
        stored
    }
}

fn history_back_route(history_return_route: Route) -> Route {
    if history_return_route == Route::History {
        Route::Start
    } else {
        history_return_route
    }
}

fn run_back_route(run_return_route: Route) -> Route {
    if run_return_route == Route::Run {
        Route::Start
    } else {
        run_return_route
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

fn cursor_row_col(text: &str, cursor: usize) -> (usize, usize) {
    let lines = line_spans(text);
    for (line_index, (start, len)) in lines.iter().enumerate() {
        if cursor <= start + len {
            return (line_index, cursor.saturating_sub(*start));
        }
    }
    let last_line = lines.len().saturating_sub(1);
    let (start, len) = lines[last_line];
    (last_line, cursor.saturating_sub(start).min(len))
}

fn wrapped_cursor_row_col(text: &str, cursor: usize, max_width: usize) -> (usize, usize) {
    if max_width == 0 {
        return (0, 0);
    }

    let mut row = 0usize;
    let mut col = 0usize;

    for ch in text.chars().take(cursor) {
        if ch == '\n' {
            row += 1;
            col = 0;
            continue;
        }

        let ch_width = UnicodeWidthChar::width(ch)
            .unwrap_or(1)
            .max(1)
            .min(max_width);

        if col + ch_width > max_width {
            row += 1;
            col = 0;
        }

        col += ch_width;
        if col >= max_width {
            row += col / max_width;
            col %= max_width;
        }
    }

    (row, col)
}

fn line_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for line in text.split('\n') {
        let len = line.chars().count();
        spans.push((start, len));
        start += len + 1;
    }
    if spans.is_empty() {
        spans.push((0, 0));
    }
    spans
}

fn char_to_byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or_else(|| text.len())
}

fn insert_char_at_cursor(edit: &mut EditState, ch: char) {
    let byte_index = char_to_byte_index(&edit.buffer, edit.cursor);
    edit.buffer.insert(byte_index, ch);
    edit.cursor += 1;
    edit.preferred_column = None;
}

fn backspace_at_cursor(edit: &mut EditState) {
    if edit.cursor == 0 {
        return;
    }
    let start = char_to_byte_index(&edit.buffer, edit.cursor - 1);
    let end = char_to_byte_index(&edit.buffer, edit.cursor);
    edit.buffer.replace_range(start..end, "");
    edit.cursor -= 1;
    edit.preferred_column = None;
}

fn delete_at_cursor(edit: &mut EditState) {
    let total = edit.buffer.chars().count();
    if edit.cursor >= total {
        return;
    }
    let start = char_to_byte_index(&edit.buffer, edit.cursor);
    let end = char_to_byte_index(&edit.buffer, edit.cursor + 1);
    edit.buffer.replace_range(start..end, "");
    edit.preferred_column = None;
}

fn move_cursor_horizontal(edit: &mut EditState, forward: bool) {
    let total = edit.buffer.chars().count();
    if forward {
        edit.cursor = (edit.cursor + 1).min(total);
    } else {
        edit.cursor = edit.cursor.saturating_sub(1);
    }
    edit.preferred_column = None;
}

fn move_cursor_line_edge(edit: &mut EditState, end: bool) {
    let lines = line_spans(&edit.buffer);
    for (start, len) in lines {
        if edit.cursor <= start + len {
            edit.cursor = if end { start + len } else { start };
            edit.preferred_column = None;
            return;
        }
    }
}

fn move_cursor_vertical(edit: &mut EditState, forward: bool) {
    let lines = line_spans(&edit.buffer);
    let (current_row, current_col) = cursor_row_col(&edit.buffer, edit.cursor);
    let target_row = if forward {
        (current_row + 1).min(lines.len().saturating_sub(1))
    } else {
        current_row.saturating_sub(1)
    };
    let desired_col = edit.preferred_column.unwrap_or(current_col);
    let (target_start, target_len) = lines[target_row];
    edit.cursor = target_start + desired_col.min(target_len);
    edit.preferred_column = Some(desired_col);
}

fn session_snapshot_lines(session: &SessionManifest) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(format!("会话：{}", session.id)),
        Line::from(format!("任务：{}", truncate(&session.task, 100))),
        Line::from(format!("状态：{}", session.status.label())),
        Line::from(format!(
            "创建时间：{}",
            format_beijing(session.created_at, "%Y-%m-%d %H:%M:%S")
        )),
        Line::from(format!("协作模板：{}", session.role_set)),
        Line::from(format!(
            "任务强度：{}",
            thinking_mode_user_title(session.thinking_mode)
        )),
        Line::from(format!(
            "结果落地：{}",
            apply_mode_user_label(session.apply_mode)
        )),
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
            "环境检查：{} / {}",
            report.readiness.label(),
            report.summary
        )));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveCommand, AppShell, CommandState, FormField, FormState, ProjectContext, Route,
        RunSubview, ShellAction, build_command_preview, contextual_help_lines, history_back_route,
        next_history_return_route, next_run_return_route, prepare_runtime_state, route_titles,
        run_back_route, run_subview_titles, split_main_sections, wrapped_cursor_row_col,
    };
    use crate::model::{ApplyMode, SessionPreset, ThinkingMode};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use std::path::{Path, PathBuf};
    use std::time::Instant;
    use tokio::sync::{mpsc, watch};

    fn test_shell() -> AppShell {
        AppShell {
            route: Route::Start,
            history_return_route: Route::Start,
            run_return_route: Route::Start,
            selected_field: 0,
            advanced_settings_open: false,
            edit_state: None,
            history_index: 0,
            notices: Vec::new(),
            form: FormState::default(),
            project: ProjectContext {
                target_dir: PathBuf::from("/tmp/demo"),
                display_target: "/tmp/demo".to_string(),
                config_source: "test".to_string(),
                verification_commands: Vec::new(),
                role_sets: vec!["default".to_string()],
                sessions: Vec::new(),
                last_error: None,
            },
            selected_session: None,
            runtime_state: None,
            run_subview: RunSubview::Dashboard,
            last_doctor_report: None,
            active_command: None,
            should_quit: false,
        }
    }

    fn finished_command(action: ShellAction) -> ActiveCommand {
        let (_tx, rx) = mpsc::unbounded_channel();
        ActiveCommand {
            action,
            commandline: "codex-forge test".to_string(),
            state: CommandState::Succeeded,
            started_at: Instant::now(),
            output: Vec::new(),
            cancel_tx: None,
            rx,
        }
    }

    fn running_command(action: ShellAction) -> ActiveCommand {
        let (_tx, rx) = mpsc::unbounded_channel();
        let (cancel_tx, _cancel_rx) = watch::channel(false);
        ActiveCommand {
            action,
            commandline: "codex-forge test".to_string(),
            state: CommandState::Running,
            started_at: Instant::now(),
            output: Vec::new(),
            cancel_tx: Some(cancel_tx),
            rx,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    #[test]
    fn builds_run_command_from_form() {
        let form = FormState {
            target_dir: "/tmp/demo".to_string(),
            config_path: String::new(),
            task: "实现 v5".to_string(),
            thinking_mode: ThinkingMode::HardThink,
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
        assert!(preview.args.contains(&"--thinking-mode".to_string()));
        assert!(preview.args.contains(&"hard-think".to_string()));
        assert!(preview.args.contains(&"--resume".to_string()));
        assert!(preview.args.contains(&"abc".to_string()));
        assert!(preview.summary.contains("实现 v5"));
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

    #[test]
    fn wrapped_cursor_moves_to_next_visual_line() {
        assert_eq!(wrapped_cursor_row_col("abcdef", 6, 6), (1, 0));
        assert_eq!(wrapped_cursor_row_col("abcdefg", 7, 6), (1, 1));
    }

    #[test]
    fn wrapped_cursor_counts_wide_chars() {
        assert_eq!(wrapped_cursor_row_col("你好ab", 4, 6), (1, 0));
        assert_eq!(wrapped_cursor_row_col("你好abc", 5, 6), (1, 1));
    }

    #[test]
    fn uses_compact_titles_on_narrow_terminal() {
        assert_eq!(route_titles(50), vec!["始", "执", "历"]);
        assert_eq!(run_subview_titles(60), vec!["态势", "事件", "摘要"]);
    }

    #[test]
    fn stacks_main_sections_on_narrow_terminal() {
        let sections = split_main_sections(Rect::new(0, 0, 80, 30));
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].width, 80);
        assert_eq!(sections[1].width, 80);
    }

    #[test]
    fn entering_history_remembers_previous_route() {
        assert_eq!(
            next_history_return_route(Route::Run, Route::Start, Route::History),
            Route::Run
        );
        assert_eq!(
            next_history_return_route(Route::Start, Route::Run, Route::History),
            Route::Start
        );
    }

    #[test]
    fn staying_in_history_keeps_stored_return_route() {
        assert_eq!(
            next_history_return_route(Route::History, Route::Run, Route::History),
            Route::Run
        );
        assert_eq!(
            next_history_return_route(Route::History, Route::Start, Route::History),
            Route::Start
        );
    }

    #[test]
    fn history_back_route_uses_last_non_history_route() {
        assert_eq!(history_back_route(Route::Run), Route::Run);
        assert_eq!(history_back_route(Route::Start), Route::Start);
    }

    #[test]
    fn history_back_route_falls_back_to_start_for_invalid_state() {
        assert_eq!(history_back_route(Route::History), Route::Start);
    }

    #[test]
    fn entering_run_remembers_previous_route() {
        assert_eq!(
            next_run_return_route(Route::History, Route::Start, Route::Run),
            Route::History
        );
        assert_eq!(
            next_run_return_route(Route::Start, Route::History, Route::Run),
            Route::Start
        );
    }

    #[test]
    fn staying_in_run_keeps_stored_return_route() {
        assert_eq!(
            next_run_return_route(Route::Run, Route::History, Route::Run),
            Route::History
        );
        assert_eq!(
            next_run_return_route(Route::Run, Route::Start, Route::Run),
            Route::Start
        );
    }

    #[test]
    fn run_back_route_falls_back_to_start_for_invalid_state() {
        assert_eq!(run_back_route(Route::Run), Route::Start);
    }

    #[test]
    fn toggle_advanced_from_run_navigates_back_to_start() {
        let mut shell = test_shell();
        shell.route = Route::Run;

        shell.toggle_advanced_settings();

        assert_eq!(shell.route, Route::Start);
        assert!(shell.advanced_settings_open);
    }

    #[test]
    fn toggle_advanced_from_history_navigates_back_to_start() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.history_return_route = Route::Run;

        shell.toggle_advanced_settings();

        assert_eq!(shell.route, Route::Start);
        assert!(shell.advanced_settings_open);
    }

    #[test]
    fn toggle_advanced_from_history_keeps_panel_open() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.advanced_settings_open = true;

        shell.toggle_advanced_settings();

        assert_eq!(shell.route, Route::Start);
        assert!(shell.advanced_settings_open);
    }

    #[test]
    fn toggle_advanced_is_blocked_while_command_is_running() {
        let mut shell = test_shell();
        shell.route = Route::Run;
        shell.active_command = Some(running_command(ShellAction::Run));

        shell.toggle_advanced_settings();

        assert_eq!(shell.route, Route::Run);
        assert!(!shell.advanced_settings_open);
        assert!(
            shell
                .notices
                .last()
                .is_some_and(|message| message.contains("当前动作仍在执行"))
        );
    }

    #[test]
    fn global_back_closes_advanced_on_start() {
        let mut shell = test_shell();
        shell.advanced_settings_open = true;

        shell.handle_global_back();

        assert_eq!(shell.route, Route::Start);
        assert!(!shell.advanced_settings_open);
    }

    #[test]
    fn global_back_from_run_returns_to_start_when_idle() {
        let mut shell = test_shell();
        shell.route = Route::Run;
        shell.active_command = Some(finished_command(ShellAction::Doctor));

        shell.handle_global_back();

        assert_eq!(shell.route, Route::Start);
    }

    #[test]
    fn global_back_from_run_returns_to_history_after_doctor_from_history() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.navigate_to(Route::Run);
        shell.active_command = Some(finished_command(ShellAction::Doctor));

        shell.handle_global_back();

        assert_eq!(shell.route, Route::History);
    }

    #[test]
    fn global_back_from_run_returns_to_history_after_replay() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.navigate_to(Route::Run);
        shell.active_command = Some(finished_command(ShellAction::ReplaySelected));

        shell.handle_global_back();

        assert_eq!(shell.route, Route::History);
    }

    #[test]
    fn tab_navigation_is_blocked_while_command_is_running() {
        let mut shell = test_shell();
        shell.route = Route::Run;
        shell.active_command = Some(running_command(ShellAction::Run));

        shell.navigate_via_tab(Route::History);

        assert_eq!(shell.route, Route::Run);
        assert!(
            shell
                .notices
                .last()
                .is_some_and(|message| message.contains("当前动作仍在执行"))
        );
    }

    #[test]
    fn global_back_from_run_does_not_hide_running_command() {
        let mut shell = test_shell();
        shell.route = Route::Run;
        shell.active_command = Some(running_command(ShellAction::Run));

        shell.handle_global_back();

        assert_eq!(shell.route, Route::Run);
        assert!(
            shell
                .notices
                .last()
                .is_some_and(|message| message.contains("当前动作仍在执行"))
        );
    }

    #[test]
    fn global_back_from_history_uses_return_route() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.history_return_route = Route::Run;

        shell.handle_global_back();

        assert_eq!(shell.route, Route::Run);
    }

    #[test]
    fn help_texts_expose_global_back_rules() {
        let start = contextual_help_lines(Route::Start, None, RunSubview::Dashboard, 120)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let run = contextual_help_lines(Route::Run, None, RunSubview::Timeline, 120)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let history = contextual_help_lines(Route::History, None, RunSubview::Summary, 120)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(start.contains("Esc"));
        assert!(run.contains("Esc"));
        assert!(history.contains("Esc"));
    }

    #[test]
    fn compact_history_help_mentions_previous_level() {
        let history = contextual_help_lines(Route::History, None, RunSubview::Summary, 60)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(history.contains("返回上一级"));
    }

    #[test]
    fn idle_run_log_mentions_replay_and_back_navigation() {
        let shell = test_shell();
        let text = shell
            .run_log_lines()
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("Esc 返回上一级"));
        assert!(text.contains("历史页可按 `l` 回放已完成会话"));
    }

    #[test]
    fn finished_run_log_switches_to_back_hint() {
        let mut shell = test_shell();
        shell.active_command = Some(finished_command(ShellAction::ReplaySelected));

        let text = shell
            .run_log_lines()
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("Esc 返回上一级"));
        assert!(!text.contains("s 停止"));
    }

    #[test]
    fn shell_action_stop_support_matches_expected_paths() {
        assert!(!ShellAction::Doctor.supports_stop());
        assert!(!ShellAction::Plan.supports_stop());
        assert!(ShellAction::Run.supports_stop());
        assert!(ShellAction::ReplaySelected.supports_stop());
    }

    #[test]
    fn prepare_runtime_state_matches_action_kind() {
        let form = FormState {
            task: "实现审计".to_string(),
            ..FormState::default()
        };

        assert!(prepare_runtime_state(ShellAction::Doctor, &form, None).is_none());
        assert!(prepare_runtime_state(ShellAction::Plan, &form, None).is_some());
        assert!(prepare_runtime_state(ShellAction::Run, &form, None).is_some());
    }

    #[test]
    fn replay_runtime_state_requires_selected_session() {
        let form = FormState {
            task: "回放".to_string(),
            ..FormState::default()
        };

        assert!(prepare_runtime_state(ShellAction::ReplaySelected, &form, None).is_none());
    }

    #[test]
    fn replay_finish_stays_on_run_route() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.navigate_to(Route::Run);
        let (tx, rx) = mpsc::unbounded_channel();
        shell.active_command = Some(ActiveCommand {
            action: ShellAction::ReplaySelected,
            commandline: "codex-forge replay".to_string(),
            state: CommandState::Running,
            started_at: Instant::now(),
            output: Vec::new(),
            cancel_tx: None,
            rx,
        });

        let _ = tx.send(super::RunnerEvent::Finished {
            state: CommandState::Succeeded,
            manifest: Box::new(None),
        });

        shell.poll_command_output().unwrap();

        assert_eq!(shell.route, Route::Run);
    }

    #[tokio::test]
    async fn key_sequence_edits_task_and_saves_with_escape() {
        let mut shell = test_shell();

        shell.handle_key(key(KeyCode::Char('e'))).await.unwrap();
        shell.handle_key(key(KeyCode::Char('修'))).await.unwrap();
        shell.handle_key(key(KeyCode::Char('复'))).await.unwrap();
        shell.handle_key(key(KeyCode::Esc)).await.unwrap();

        assert_eq!(shell.form.task, "修复");
        assert!(shell.edit_state.is_none());
        assert!(
            shell
                .notices
                .last()
                .is_some_and(|message| message.contains("任务已保存"))
        );
    }

    #[tokio::test]
    async fn key_sequence_history_to_advanced_then_escape_returns_cleanly() {
        let mut shell = test_shell();

        shell.handle_key(key(KeyCode::Char('3'))).await.unwrap();
        assert_eq!(shell.route, Route::History);

        shell.handle_key(key(KeyCode::Char('a'))).await.unwrap();
        assert_eq!(shell.route, Route::Start);
        assert!(shell.advanced_settings_open);

        shell.handle_key(key(KeyCode::Esc)).await.unwrap();
        assert_eq!(shell.route, Route::Start);
        assert!(!shell.advanced_settings_open);
    }

    #[tokio::test]
    async fn key_sequence_tab_and_brackets_cycle_run_subviews() {
        let mut shell = test_shell();
        shell.route = Route::Run;

        shell.handle_key(key(KeyCode::Tab)).await.unwrap();
        assert_eq!(shell.run_subview, RunSubview::Timeline);

        shell.handle_key(key(KeyCode::Char(']'))).await.unwrap();
        assert_eq!(shell.run_subview, RunSubview::Summary);

        shell.handle_key(key(KeyCode::Char('['))).await.unwrap();
        assert_eq!(shell.run_subview, RunSubview::Timeline);
    }

    #[tokio::test]
    async fn key_sequence_running_command_blocks_history_and_advanced_hotkeys() {
        let mut shell = test_shell();
        shell.route = Route::Run;
        shell.active_command = Some(running_command(ShellAction::Run));

        shell.handle_key(key(KeyCode::Char('3'))).await.unwrap();
        assert_eq!(shell.route, Route::Run);

        shell.handle_key(key(KeyCode::Char('a'))).await.unwrap();
        assert_eq!(shell.route, Route::Run);
        assert!(!shell.advanced_settings_open);
        assert!(
            shell
                .notices
                .last()
                .is_some_and(|message| message.contains("当前动作仍在执行"))
        );
    }

    #[tokio::test]
    async fn key_sequence_non_task_escape_cancels_without_committing() {
        let mut shell = test_shell();
        shell.advanced_settings_open = true;
        shell.start_editing(FormField::Workers);

        shell.handle_key(key(KeyCode::Char('9'))).await.unwrap();
        shell.handle_key(key(KeyCode::Esc)).await.unwrap();

        assert_eq!(shell.form.workers, "4");
        assert!(shell.edit_state.is_none());
    }

    #[tokio::test]
    async fn key_sequence_ctrl_shortcuts_are_ignored_outside_edit_mode() {
        let mut shell = test_shell();

        shell.handle_key(ctrl_key('p')).await.unwrap();
        shell.handle_key(ctrl_key('r')).await.unwrap();

        assert_eq!(shell.route, Route::Start);
        assert!(shell.active_command.is_none());
    }
}
