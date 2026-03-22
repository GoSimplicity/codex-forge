use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal, Stdout};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;
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
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use unicode_width::UnicodeWidthChar;

use crate::app::build_review_fix_config;
use crate::app::{resolve_continue_config, resolve_plan_config, resolve_run_config};
use crate::apply::{deliver_accepted_files, deliver_selected_files_from_plan};
use crate::cli::{
    ApplyModeArg, ContinueArgs, ContinueModeArg, PlanArgs, RunArgs, SharedTaskArgs,
    ThinkingModeArg, UiModeArg,
};
use crate::config::{LoadedProjectConfig, load_project_config};
use crate::doctor::run_doctor;
use crate::model::{
    ApplyMode, ApplyPlan, ApplyStatus, DoctorReport, ManualDeliveryResult, ManualReviewFileRecord,
    ManualReviewFileStatus, ManualReviewState, RuntimeEvent, SessionManifest, SessionPreset,
    ThinkingMode, UiMode,
};
use crate::orchestrator::{EmbeddedRunOutcome, plan_session_embedded, run_session_embedded};
use crate::replay::replay_session_embedded;
use crate::resources::{ResourceCatalog, load_resource_catalog};
use crate::session::{
    cleanup_all_forge_artifacts, cleanup_session_lineage, load_session, reset_session_lineage,
    set_manual_delivery_result_for_loaded_session, set_manual_review_state_for_loaded_session,
};
use crate::time::format_beijing;
use crate::ui::{RuntimeViewState, describe_runtime_event, render_runtime_dashboard};
use crate::workspace::{remember_target_dir, resolve_target_dir};

const MAX_LOG_LINES: usize = 240;
const MAX_NOTICE_LINES: usize = 8;
const HISTORY_DETAIL_PAGE_LINES: usize = 220;
const EXIT_ESC_ARM_WINDOW: Duration = Duration::from_millis(1500);

/// v6 终端产品主导航。保留固定 3 个页面，确保演示路径稳定可记忆。
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
    ContinueFeedback,
    ReviewIssue,
    ContinueMode,
    FromPlanSession,
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
            Self::ContinueFeedback => "继续反馈",
            Self::ReviewIssue => "审查问题",
            Self::ContinueMode => "继续模式",
            Self::FromPlanSession => "执行方案会话",
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
    ContinueSelected,
    ReviewFixSelected,
    ReplaySelected,
}

impl ShellAction {
    fn label(self) -> &'static str {
        match self {
            Self::Doctor => "检查环境",
            Self::Plan => "先看方案",
            Self::Run => "开始执行",
            Self::ContinueSelected => "继续优化",
            Self::ReviewFixSelected => "修复当前文件",
            Self::ReplaySelected => "回放过程",
        }
    }

    fn requires_task(self) -> bool {
        matches!(self, Self::Plan | Self::Run)
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
            Self::Dashboard => "进度总览",
            Self::Timeline => "执行视图",
            Self::Summary => "最终交付",
        }
    }

    fn all() -> [Self; 3] {
        [Self::Dashboard, Self::Timeline, Self::Summary]
    }
}

/// 历史详情弹层里的细分阅读视图。
/// 目标是把 session 已落盘的计划、运行轨迹、worker 输出和关键产物完整串起来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryDetailTab {
    Overview,
    Plan,
    Runtime,
    Artifacts,
    Technical,
}

impl HistoryDetailTab {
    fn label(self) -> &'static str {
        match self {
            Self::Overview => "总览",
            Self::Plan => "方案",
            Self::Runtime => "过程",
            Self::Artifacts => "交付",
            Self::Technical => "技术细节",
        }
    }

    fn all() -> [Self; 5] {
        [
            Self::Overview,
            Self::Plan,
            Self::Runtime,
            Self::Artifacts,
            Self::Technical,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartFocus {
    TaskInput,
    Actions,
    AdvancedFields,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryFocus {
    Sessions,
    Actions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunFocus {
    Subviews,
    Actions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartAction {
    Doctor,
    Plan,
    Run,
    ToggleSettings,
}

impl StartAction {
    fn all() -> [Self; 4] {
        [Self::Doctor, Self::Plan, Self::Run, Self::ToggleSettings]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryAction {
    ExecutePlan,
    DeliverAccepted,
    ManualReview,
    Continue,
    EditFeedback,
    ContinueMode,
    Replay,
    Detail,
    ResetSelected,
    CleanSelected,
    CleanAll,
    BackToStart,
}

impl HistoryAction {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunAction {
    Back,
    Stop,
    ViewHistory,
    BackToStart,
}

impl RunAction {
    fn all() -> [Self; 4] {
        [Self::Back, Self::Stop, Self::ViewHistory, Self::BackToStart]
    }
}

#[derive(Debug, Clone)]
struct HistoryDetailState {
    session: SessionManifest,
    active_tab: HistoryDetailTab,
    summary: String,
    detail: String,
    page: usize,
    scroll: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualReviewFocus {
    Files,
    Actions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualReviewDiffView {
    Source,
    LatestFix,
    Compare,
}

impl ManualReviewDiffView {
    fn label(self) -> &'static str {
        match self {
            Self::Source => "原始候选",
            Self::LatestFix => "返修结果",
            Self::Compare => "前后对比",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualReviewAction {
    Approve,
    NeedsFix,
    EditIssue,
    StartFix,
    DeliverApproved,
    Close,
}

impl ManualReviewAction {
    fn all() -> [Self; 6] {
        [
            Self::Approve,
            Self::NeedsFix,
            Self::EditIssue,
            Self::StartFix,
            Self::DeliverApproved,
            Self::Close,
        ]
    }
}

#[derive(Debug, Clone)]
struct ManualReviewPopupState {
    session_id: String,
    state: ManualReviewState,
    file_index: usize,
    action_index: usize,
    focus: ManualReviewFocus,
    diff_view: ManualReviewDiffView,
    scroll: u16,
}

impl ManualReviewPopupState {
    fn selected_file(&self) -> Option<&ManualReviewFileRecord> {
        self.state.files.get(self.file_index)
    }

    fn selected_file_mut(&mut self) -> Option<&mut ManualReviewFileRecord> {
        self.state.files.get_mut(self.file_index)
    }
}

#[derive(Debug, Clone)]
struct PendingReviewFix {
    parent_session_id: String,
    target_file: String,
}

impl HistoryDetailState {
    fn from_session(session: &SessionManifest) -> Self {
        let active_tab = HistoryDetailTab::Overview;
        Self {
            session: session.clone(),
            active_tab,
            summary: build_history_detail_summary(session, active_tab),
            detail: build_history_detail_body(session, active_tab),
            page: 0,
            scroll: 0,
        }
    }

    fn session_id(&self) -> &str {
        &self.session.id
    }

    fn cycle_tab(&mut self, forward: bool) {
        let tabs = HistoryDetailTab::all();
        let current = tabs
            .iter()
            .position(|item| *item == self.active_tab)
            .unwrap_or(0);
        self.active_tab = tabs[cycle_index(current, tabs.len(), forward)];
        self.summary = build_history_detail_summary(&self.session, self.active_tab);
        self.detail = build_history_detail_body(&self.session, self.active_tab);
        self.page = 0;
        self.scroll = 0;
    }

    fn scroll_lines(&mut self, delta: i32) {
        if delta >= 0 {
            self.scroll = self.scroll.saturating_add(delta as u16);
        } else {
            self.scroll = self.scroll.saturating_sub((-delta) as u16);
        }
    }

    fn page_count(&self) -> usize {
        page_count_for_text(&self.detail, HISTORY_DETAIL_PAGE_LINES)
    }

    fn current_page_text(&self) -> String {
        page_text(&self.detail, self.page, HISTORY_DETAIL_PAGE_LINES)
    }

    fn page_summary(&self) -> String {
        let total = self.page_count();
        format!("第 {}/{} 页", self.page.saturating_add(1), total.max(1))
    }

    fn next_page(&mut self) {
        let max_index = self.page_count().saturating_sub(1);
        if self.page < max_index {
            self.page += 1;
        }
        self.scroll = 0;
    }

    fn previous_page(&mut self) {
        self.page = self.page.saturating_sub(1);
        self.scroll = 0;
    }

    fn first_page(&mut self) {
        self.page = 0;
        self.scroll = 0;
    }

    fn last_page(&mut self) {
        self.page = self.page_count().saturating_sub(1);
        self.scroll = 0;
    }
}

#[derive(Debug, Clone)]
enum ConfirmAction {
    ResetSelected { session_id: String },
    CleanSelected { session_id: String },
    CleanAll,
    Quit,
}

#[derive(Debug, Clone)]
struct ConfirmDialogState {
    action: ConfirmAction,
    title: String,
    lines: Vec<String>,
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
    state: CommandState,
    started_at: Instant,
    finished_at: Option<Instant>,
    stop_requested: bool,
    output: Vec<String>,
    cancel_tx: Option<watch::Sender<bool>>,
    rx: mpsc::UnboundedReceiver<RunnerEvent>,
}

#[derive(Debug, Clone)]
struct SessionSummary {
    id: String,
    created_at: String,
    task: String,
    stage_label: String,
    summary: String,
    mode_label: String,
    continuable: bool,
}

#[derive(Debug, Clone)]
struct ProjectContext {
    target_dir: PathBuf,
    display_target: String,
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
    continue_feedback: String,
    review_issue: String,
    continue_mode: ContinueModeArg,
    from_plan_session_id: String,
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
            continue_feedback: String::new(),
            review_issue: String::new(),
            continue_mode: ContinueModeArg::Auto,
            from_plan_session_id: String::new(),
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
    history_entries: Vec<String>,
    history_index: Option<usize>,
}

struct AppShell {
    route: Route,
    history_return_route: Route,
    run_return_route: Route,
    nav_focus: bool,
    nav_index: usize,
    start_focus: StartFocus,
    history_focus: HistoryFocus,
    run_focus: RunFocus,
    start_action_index: usize,
    history_action_index: usize,
    run_action_index: usize,
    selected_field: usize,
    advanced_settings_open: bool,
    edit_state: Option<EditState>,
    history_index: usize,
    notices: Vec<String>,
    form: FormState,
    project: ProjectContext,
    selected_session: Option<SessionManifest>,
    history_detail: Option<HistoryDetailState>,
    manual_review: Option<ManualReviewPopupState>,
    confirm_dialog: Option<ConfirmDialogState>,
    runtime_state: Option<RuntimeViewState>,
    run_subview: RunSubview,
    last_doctor_report: Option<DoctorReport>,
    active_command: Option<ActiveCommand>,
    pending_review_fix: Option<PendingReviewFix>,
    exit_esc_armed_at: Option<Instant>,
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
        bail!("v6 主界面需要在交互式终端中运行");
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
            nav_focus: false,
            nav_index: 0,
            start_focus: StartFocus::TaskInput,
            history_focus: HistoryFocus::Sessions,
            run_focus: RunFocus::Subviews,
            start_action_index: 0,
            history_action_index: 0,
            run_action_index: 0,
            selected_field: 0,
            advanced_settings_open: false,
            edit_state: None,
            history_index: 0,
            notices: vec![
                "默认主路径：先写任务，再用方向键和 Enter 选择“先看方案”或“开始执行”。".to_string(),
                "历史页右侧会直接列出“继续优化 / 回放 / 查看详情 / 清理”等动作。".to_string(),
            ],
            form,
            project,
            selected_session,
            history_detail: None,
            manual_review: None,
            confirm_dialog: None,
            runtime_state: None,
            run_subview: RunSubview::Dashboard,
            last_doctor_report: None,
            active_command: None,
            pending_review_fix: None,
            exit_esc_armed_at: None,
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

        if self.history_detail.is_some() {
            let popup = centered_rect(
                popup_percent(area.width, 90, 98),
                popup_percent(area.height, 88, 96),
                area,
            );
            frame.render_widget(Clear, popup);
            self.render_history_detail_popup(frame, popup);
        }

        if self.manual_review.is_some() {
            let popup = centered_rect(
                popup_percent(area.width, 94, 99),
                popup_percent(area.height, 90, 98),
                area,
            );
            frame.render_widget(Clear, popup);
            self.render_manual_review_popup(frame, popup);
        }

        if self.confirm_dialog.is_some() {
            let popup = centered_rect(
                popup_percent(area.width, 72, 94),
                popup_percent(area.height, 28, 54),
                area,
            );
            frame.render_widget(Clear, popup);
            self.render_confirm_popup(frame, popup);
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
                        "◢ CF V6 ◣",
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
                        "◢ CODEX-FORGE V6 ◣",
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
            .select(if self.nav_focus {
                self.nav_index.min(Route::all().len() - 1)
            } else {
                Route::all()
                    .iter()
                    .position(|item| *item == self.route)
                    .unwrap_or(0)
            })
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(if self.nav_focus {
                        "导航（已聚焦，←→ 切换，Enter 进入）"
                    } else {
                        "导航（按 ↑ 或 Tab 进入）"
                    })
                    .border_style(if self.nav_focus {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    }),
            )
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
            .block(Block::default().title("现在可做").borders(Borders::ALL))
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
                let right_sections = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(11), Constraint::Min(10)])
                    .split(sections[1]);
                frame.render_widget(self.history_actions_widget(), right_sections[0]);
                frame.render_widget(self.history_right_widget(), right_sections[1]);
            }
        }
    }

    fn render_start_route(&self, frame: &mut ratatui::Frame<'_>, sections: Vec<Rect>) {
        frame.render_widget(self.start_main_widget(), sections[0]);
        if self.advanced_settings_open {
            let right_sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(9),
                    Constraint::Percentage(48),
                    Constraint::Percentage(52),
                ])
                .split(sections[1]);
            frame.render_widget(self.start_actions_widget(), right_sections[0]);
            frame.render_widget(self.advanced_settings_widget(), right_sections[1]);
            frame.render_widget(self.advanced_details_widget(), right_sections[2]);
        } else {
            let right_sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(9), Constraint::Min(10)])
                .split(sections[1]);
            frame.render_widget(self.start_actions_widget(), right_sections[0]);
            frame.render_widget(self.start_recent_widget(), right_sections[1]);
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
                    render_runtime_dashboard(frame, sections[1], runtime_state, "进度总览");
                } else if self
                    .active_command
                    .as_ref()
                    .is_some_and(|command| command.action == ShellAction::Doctor)
                {
                    frame.render_widget(self.run_doctor_widget(), sections[1]);
                } else {
                    frame.render_widget(self.run_placeholder_widget(), sections[1]);
                }
            }
            RunSubview::Timeline => {
                let split = Layout::default()
                    .direction(if area.width < 120 {
                        Direction::Vertical
                    } else {
                        Direction::Horizontal
                    })
                    .constraints(if area.width < 120 {
                        vec![Constraint::Percentage(42), Constraint::Percentage(58)]
                    } else {
                        vec![Constraint::Percentage(40), Constraint::Percentage(60)]
                    })
                    .split(sections[1]);
                frame.render_widget(self.run_timeline_widget(), split[0]);
                frame.render_widget(self.run_timeline_detail_widget(), split[1]);
            }
            RunSubview::Summary => {
                frame.render_widget(self.run_summary_widget(), sections[1]);
            }
        }

        let bottom_sections = Layout::default()
            .direction(if area.width < 110 {
                Direction::Vertical
            } else {
                Direction::Horizontal
            })
            .constraints(if area.width < 110 {
                vec![Constraint::Length(8), Constraint::Min(6)]
            } else {
                vec![Constraint::Percentage(70), Constraint::Percentage(30)]
            })
            .split(sections[2]);
        frame.render_widget(self.run_log_widget(), bottom_sections[0]);
        frame.render_widget(self.run_actions_widget(), bottom_sections[1]);
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
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("执行视图")
                    .border_style(if self.run_focus == RunFocus::Subviews {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    }),
            )
    }

    fn start_main_widget(&self) -> Paragraph<'_> {
        let mut lines = vec![
            Line::from(Span::styled(
                "一句话写下你想完成的事",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(if self.form.task.trim().is_empty() {
                Span::styled(
                    "下一步：按 Enter 写任务。",
                    Style::default().fg(Color::LightRed),
                )
            } else {
                Span::styled(
                    truncate(&self.form.task, 160),
                    Style::default().fg(Color::White),
                )
            }),
            Line::from(if self.form.task.trim().is_empty() {
                Span::raw("")
            } else {
                Span::styled(
                    "下一步：右侧选“先看方案”或“开始执行”。",
                    Style::default().fg(Color::LightGreen),
                )
            }),
            Line::from(format!(
                "当前设置：{} / {}",
                thinking_mode_user_title(self.form.thinking_mode),
                advanced_settings_summary(&self.form)
            )),
            Line::from(""),
            Line::from("保存：`Ctrl+S`  快速定位：`Ctrl+P` / `Ctrl+R`（不会自动执行）"),
            Line::from(format!("仓库：{}", self.project.display_target)),
        ];
        if let Some(error) = &self.project.last_error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("阻塞：{}", truncate(error, 120)),
                Style::default().fg(Color::Red),
            )));
        }
        if let Some(report) = &self.last_doctor_report {
            lines.push(Line::from(format!(
                "最近检查：{} / {}",
                report.readiness.label(),
                report.summary
            )));
        }

        Paragraph::new(lines)
            .block(
                Block::default()
                    .title("任务输入")
                    .borders(Borders::ALL)
                    .border_style(if self.start_focus == StartFocus::TaskInput {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    }),
            )
            .wrap(Wrap { trim: true })
    }

    fn start_actions_widget(&self) -> List<'_> {
        let items = StartAction::all()
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let selected =
                    self.start_focus == StartFocus::Actions && index == self.start_action_index;
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else if matches!(*action, StartAction::Plan) {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let (title, detail) = self.start_action_line(*action);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<10}", title), style),
                    Span::raw(detail),
                ]))
            })
            .collect::<Vec<_>>();

        List::new(items).block(
            Block::default()
                .title("主操作")
                .borders(Borders::ALL)
                .border_style(if self.start_focus == StartFocus::Actions {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                }),
        )
    }

    fn start_recent_widget(&self) -> Paragraph<'_> {
        let preview = self.command_preview_lines(ShellAction::Run);
        let mut lines = vec![
            Line::from("建议流程"),
            Line::from(""),
            Line::from("1. 先生成方案"),
            Line::from("2. 看方案是否可接受"),
            Line::from("3. 再决定是否执行"),
            Line::from(""),
            Line::from(Span::styled(
                "当前执行来源",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(run_source_user_hint(&self.form)),
            Line::from(""),
            Line::from(Span::styled(
                "如果现在直接执行",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(truncate(&preview.summary, 120)),
            Line::from(""),
        ];

        if let Some(session) = &self.selected_session
            && session.is_plan_session()
            && let Some(plan) = &session.plan_todo
        {
            lines.push(Line::from(Span::styled(
                "最近方案",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(format!(
                "会话：{} / Todo {} 项",
                truncate(&session.id, 24),
                plan.todos.len()
            )));
            lines.push(Line::from(truncate(&plan.summary, 120)));
            lines.push(Line::from(""));
        }

        lines.extend([Line::from(Span::styled(
            "最近结果",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))]);

        if self.project.sessions.is_empty() {
            lines.push(Line::from("还没有会话记录。"));
        } else {
            for item in self.project.sessions.iter().take(3) {
                lines.push(Line::from(format!(
                    "{}  {} / {}",
                    item.created_at,
                    truncate(&item.mode_label, 4),
                    truncate(&item.task, 38),
                )));
            }
        }

        Paragraph::new(lines)
            .block(Block::default().title("低频信息").borders(Borders::ALL))
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
                .title("更多设置")
                .borders(Borders::ALL)
                .border_style(if self.start_focus == StartFocus::AdvancedFields {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                }),
        )
    }

    fn advanced_details_widget(&self) -> Paragraph<'_> {
        let preview = self.command_preview_lines(ShellAction::Run);
        let lines = vec![
            Line::from("这些都是低频选项。"),
            Line::from("用 ↑↓ 选字段，Enter 修改。"),
            Line::from(format!("验证：{}", verification_summary(&self.project))),
            Line::from(""),
            Line::from(format!("预览：{}", truncate(&preview.commandline, 120))),
        ];
        Paragraph::new(lines)
            .block(Block::default().title("低频说明").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn run_placeholder_widget(&self) -> Paragraph<'_> {
        let mut lines = vec![
            Line::from("现在没有任务在跑。"),
            Line::from("下一步：回开始页写任务，或去历史看上一次结果。"),
        ];
        if let Some(session) = &self.selected_session {
            lines.push(Line::from(""));
            lines.push(Line::from(format!("最近会话：{}", session.id)));
            lines.push(Line::from(format!("状态：{}", session.status.label())));
        }
        Paragraph::new(lines)
            .block(Block::default().title("当前状态").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn run_doctor_widget(&self) -> Paragraph<'_> {
        let mut lines = Vec::new();
        if let Some(report) = &self.last_doctor_report {
            lines.push(Line::from(Span::styled(
                format!(
                    "检查结论：{} / {}",
                    report.readiness.label(),
                    report.summary
                ),
                Style::default()
                    .fg(if report.ok {
                        Color::LightGreen
                    } else {
                        Color::LightRed
                    })
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            for check in &report.checks {
                lines.push(Line::from(format!(
                    "[{}] {} - {}",
                    check.status.label(),
                    check.name,
                    truncate(&check.detail, 96)
                )));
            }
        } else if let Some(command) = &self.active_command {
            lines.push(Line::from("正在检查环境、配置和验证条件…"));
            lines.push(Line::from(""));
            for line in command.output.iter().rev().take(8).rev() {
                lines.push(Line::from(line.clone()));
            }
        } else {
            lines.push(Line::from("还没有环境检查结果。"));
            lines.push(Line::from("下一步：回开始页执行“检查环境”。"));
        }

        Paragraph::new(lines)
            .block(Block::default().title("环境检查").borders(Borders::ALL))
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
            let elapsed_secs = command_elapsed_secs(command);
            lines.push(Line::from(format!(
                "现在在做：{} / {}",
                command.action.label(),
                command.state.label()
            )));
            lines.push(Line::from(if command.state.is_running() {
                format!("已运行：{}s", elapsed_secs)
            } else {
                format!("总耗时：{}s", elapsed_secs)
            }));
            if command.state.is_running() && command.stop_requested {
                lines.push(Line::from("停止请求已发送，系统正在等待安全收口。"));
            } else if command.state.is_running() && command.cancel_tx.is_some() {
                lines.push(Line::from(
                    "可以等待，也可以切去“开始”或“历史”；如需中止可按 `s`。",
                ));
            } else if command.state.is_running() {
                lines.push(Line::from("当前动作不支持中途停止，请等待它自然结束。"));
            } else {
                lines.push(Line::from("这次已经结束。可回上一级，或去历史查看结果。"));
            }
            lines.push(Line::from(""));
            for line in command.output.iter().rev().take(8).rev() {
                lines.push(Line::from(line.clone()));
            }
        } else {
            lines.push(Line::from("这里会显示你刚刚做了什么。"));
            lines.push(Line::from("现在没有运行中的动作。"));
            lines.push(Line::from("下一步：回开始页开始，或去历史查看结果。"));
        }
        lines
    }

    fn run_actions_widget(&self) -> List<'_> {
        let items = RunAction::all()
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let selected =
                    self.run_focus == RunFocus::Actions && index == self.run_action_index;
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let (title, detail) = self.run_action_line(*action);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<12}", title), style),
                    Span::raw(detail),
                ]))
            })
            .collect::<Vec<_>>();

        List::new(items).block(
            Block::default()
                .title("下一步")
                .borders(Borders::ALL)
                .border_style(if self.run_focus == RunFocus::Actions {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                }),
        )
    }

    fn run_timeline_widget(&self) -> Paragraph<'_> {
        let mut lines = Vec::new();
        if let Some(runtime_state) = &self.runtime_state {
            lines.push(Line::from(format!(
                "当前阶段：{}",
                runtime_state.current_user_stage
            )));
            lines.push(Line::from(format!(
                "现在在做：{}",
                runtime_state.current_user_message
            )));
            lines.push(Line::from(format!(
                "下一步：{}",
                runtime_state.next_user_step
            )));
            if let Some(active_worker) = runtime_state.active_worker.as_deref() {
                lines.push(Line::from(format!("当前焦点：{active_worker}")));
            }
            lines.push(Line::from(""));
            let execution_lines = runtime_state.execution_entry_texts(12);
            if execution_lines.is_empty() {
                lines.push(Line::from("执行流还没有新的事件。"));
            } else {
                for line in execution_lines {
                    lines.push(Line::from(truncate(&line, 96)));
                }
            }
        } else if let Some(session) = &self.selected_session {
            lines.push(Line::from(format!("历史会话：{}", session.id)));
            lines.push(Line::from(""));
            if session.timeline_events.is_empty() {
                lines.push(Line::from("该会话还没有执行记录。"));
            } else {
                for item in session.timeline_events.iter().rev().take(10).rev() {
                    lines.push(Line::from(format!(
                        "- {} / {} / {}",
                        format_beijing(item.ts, "%H:%M:%S"),
                        item.title,
                        truncate(&item.detail, 72)
                    )));
                }
            }
        } else {
            lines.push(Line::from("暂无可展示的执行流。"));
            lines.push(Line::from("先执行或回放一次任务，即可在这里回看关键推进。"));
        }

        Paragraph::new(lines)
            .block(Block::default().title("执行流").borders(Borders::ALL))
            .wrap(Wrap { trim: false })
    }

    fn run_timeline_detail_widget(&self) -> Paragraph<'_> {
        let mut lines = Vec::new();
        if let Some(runtime_state) = &self.runtime_state {
            lines.push(Line::from(format!(
                "技术细节 / {} / {}",
                runtime_state.phase, runtime_state.current_user_stage
            )));
            if let Some(brain) = &runtime_state.brain {
                lines.push(Line::from(format!(
                    "Brain：{} / 风险 {}",
                    truncate(&brain.current_focus, 72),
                    brain.risk_level.label()
                )));
            }
            lines.push(Line::from(""));
            if let Some(active_worker_id) = runtime_state.active_worker.as_deref()
                && let Some(worker) = runtime_state.workers.get(active_worker_id)
            {
                lines.push(Line::from(format!("焦点 Worker：{}", active_worker_id)));
                lines.push(Line::from(format!("角色：{}", worker.role)));
                lines.push(Line::from(format!("标题：{}", truncate(&worker.title, 88))));
                if let Some(todo_id) = worker.todo_id.as_deref() {
                    lines.push(Line::from(format!("Todo：{todo_id}")));
                }
                lines.push(Line::from(format!(
                    "队列态：{}",
                    worker.queue_state.label()
                )));
                lines.push(Line::from(format!("状态：{}", worker.status.label())));
                lines.push(Line::from(format!("阶段：{}", worker.phase_label)));
                if let Some(reason) = worker.blocked_reason.as_ref() {
                    lines.push(Line::from(format!(
                        "阻塞：{} / {}",
                        reason.label(),
                        truncate(&reason.detail, 72)
                    )));
                }
                lines.push(Line::from(format!(
                    "最近事件：{}",
                    truncate(&worker.last_event, 88)
                )));
                lines.push(Line::from(format!(
                    "Worktree：{}",
                    truncate(&worker.worktree_path, 88)
                )));
                lines.push(Line::from(""));
            }
            let execution_lines = runtime_state.execution_entry_texts(36);
            if execution_lines.is_empty() {
                lines.push(Line::from("当前还没有新的执行流细节。"));
            } else {
                for line in execution_lines {
                    lines.push(Line::from(line));
                }
            }
        } else if let Some(command) = &self.active_command {
            lines.push(Line::from(format!(
                "技术细节 / {} / {}",
                command.action.label(),
                command.state.label()
            )));
            lines.push(Line::from(""));
            for line in command.output.iter().rev().take(36).rev() {
                lines.push(Line::from(line.clone()));
            }
        } else if let Some(session) = &self.selected_session {
            lines.push(Line::from(format!("历史会话：{}", session.id)));
            lines.push(Line::from(format!(
                "timeline：{}",
                session.timeline_path.display()
            )));
            lines.push(Line::from(""));
            if session.timeline_events.is_empty() {
                lines.push(Line::from("该会话还没有事件流记录。"));
            } else {
                for item in session.timeline_events.iter().rev().take(20).rev() {
                    lines.push(Line::from(format!(
                        "{}  {} / {}",
                        format_beijing(item.ts, "%H:%M:%S"),
                        item.title,
                        truncate(&item.detail, 88)
                    )));
                }
            }
        } else {
            lines.push(Line::from("这里显示执行流明细、Worker 焦点和技术细节。"));
        }

        Paragraph::new(lines)
            .block(Block::default().title("技术细节").borders(Borders::ALL))
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
            let deliverables = self
                .selected_session
                .as_ref()
                .map(existing_deliverables)
                .unwrap_or_default();
            let system_artifacts = self
                .selected_session
                .as_ref()
                .map(existing_system_artifacts)
                .unwrap_or_default();
            lines.push(Line::from(Span::styled(
                "最终交付",
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
            if let Some(session) = self.selected_session.as_ref() {
                lines.push(Line::from(format!(
                    "目标目录交付：{} / {}",
                    delivery_status_label(session),
                    truncate(&delivery_status_detail(session), 120)
                )));
            }
            lines.push(Line::from(format!(
                "系统工件目录：{}",
                self.selected_session
                    .as_ref()
                    .map(|session| session.session_dir.display().to_string())
                    .unwrap_or_else(|| format!("{}/.codex-forge", self.project.display_target))
            )));
            if system_artifacts.is_empty() {
                lines.push(Line::from("系统工件：尚未落盘。"));
            } else {
                lines.push(Line::from("系统工件："));
                for path in system_artifacts {
                    lines.push(Line::from(format!("- {}", path.display())));
                }
            }
            if deliverables.is_empty() {
                let fallback = if let Some(session) = self.selected_session.as_ref() {
                    format!(
                        "用户导出件：当前会话尚未交付到目标目录。{}",
                        truncate(&delivery_status_detail(session), 96)
                    )
                } else {
                    "用户导出件：当前会话未导出到仓库根目录。".to_string()
                };
                lines.push(Line::from(fallback));
            } else {
                lines.push(Line::from("用户导出件："));
                for path in deliverables {
                    lines.push(Line::from(format!("- {}", path.display())));
                }
            }
        } else if let Some(runtime_state) = &self.runtime_state {
            lines.push(Line::from("交付摘要尚未生成。"));
            lines.push(Line::from(format!(
                "当前阶段：{}",
                runtime_state.current_user_stage
            )));
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
            lines.push(Line::from("可先切到“进度总览”或“执行视图”查看当前进展。"));
        } else if let Some(session) = &self.selected_session {
            lines.push(Line::from(format!("历史会话：{}", session.id)));
            lines.push(Line::from(format!("状态：{}", session.status.label())));
            lines.push(Line::from("这次还没有生成最终摘要。"));
        } else {
            lines.push(Line::from("这里会显示最后结果。"));
            lines.push(Line::from("先运行一次，或回放历史。"));
        }

        Paragraph::new(lines)
            .block(Block::default().title("最终交付").borders(Borders::ALL))
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
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(format!("{}  ", item.created_at), style),
                        Span::raw(format!(
                            "{} / {} / {}",
                            item.mode_label,
                            item.stage_label,
                            if item.continuable {
                                "可继续"
                            } else {
                                "进行中"
                            }
                        )),
                    ]),
                    Line::from(truncate(&item.task, 52)),
                    Line::from(truncate(&item.summary, 52)),
                ])
            })
            .collect::<Vec<_>>();
        List::new(items).block(
            Block::default()
                .title("历史会话")
                .borders(Borders::ALL)
                .border_style(if self.history_focus == HistoryFocus::Sessions {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                }),
        )
    }

    fn history_right_widget(&self) -> Paragraph<'_> {
        let lines = if let Some(session) = &self.selected_session {
            let next_step = if can_open_manual_review(session) {
                "下一步：右侧优先可选“人工审查”，逐文件处理 manual_review_files。"
            } else if can_deliver_accepted_files(session) {
                "下一步：右侧优先可选“交付已接收”，把 accepted_files 安全落地到目标目录。"
            } else if session.is_plan_session() {
                "下一步：右侧可选“执行此方案”“继续改方案”“回放过程”或“查看详情”。"
            } else {
                "下一步：右侧选“继续优化”“回放过程”或“查看详情”。"
            };
            let mut lines = vec![
                Line::from(format!("当前会话：{}", truncate(&session.task, 80))),
                Line::from(format!("状态：{}", session.status.label())),
                Line::from(format!("类型：{}", session.session_kind.label())),
                Line::from(format!(
                    "创建时间：{}",
                    format_beijing(session.created_at, "%m-%d %H:%M")
                )),
                Line::from(format!("交付物目录：{}", session.repo_root().display())),
                Line::from(format!(
                    "目标目录状态：{} / {}",
                    delivery_status_label(session),
                    truncate(&delivery_status_detail(session), 80)
                )),
                Line::from(""),
                Line::from(next_step),
                Line::from("详情页默认左侧是用户摘要，右侧才是技术细节。"),
            ];
            if let Some(plan) = &session.plan_todo {
                lines.push(Line::from(format!(
                    "方案摘要：{}",
                    truncate(&plan.summary, 96)
                )));
            }
            if let Some(summary) = &session.final_summary {
                lines.push(Line::from(format!(
                    "最终结论：{}",
                    truncate(&summary.overview, 96)
                )));
            }
            lines.push(Line::from(format!(
                "继续模式：{}",
                continue_mode_user_label(self.form.continue_mode)
            )));
            lines.push(Line::from(format!(
                "继续反馈：{}",
                if self.form.continue_feedback.trim().is_empty() {
                    "未填写；可以直接继续，也可以先在右侧动作区补充反馈。".to_string()
                } else {
                    truncate(&self.form.continue_feedback, 100)
                }
            )));
            lines
        } else {
            vec![
                Line::from("先在左侧选中一个会话。"),
                Line::from("右侧动作区会显示继续优化、回放、查看详情和清理入口。"),
                Line::from("选中后可按 Enter 查看详情，也可用 e / v / z / x 快速操作。"),
            ]
        };
        Paragraph::new(lines)
            .block(Block::default().title("当前会话").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn history_actions_widget(&self) -> List<'_> {
        let actions = available_history_actions(self.selected_session.as_ref());
        let items = actions
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let selected = self.history_focus == HistoryFocus::Actions
                    && index == self.history_action_index;
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let (title, detail) = self.history_action_line(*action);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<12}", title), style),
                    Span::raw(detail),
                ]))
            })
            .collect::<Vec<_>>();

        List::new(items).block(
            Block::default()
                .title("下一步")
                .borders(Borders::ALL)
                .border_style(if self.history_focus == HistoryFocus::Actions {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                }),
        )
    }

    fn render_history_detail_popup(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let Some(detail) = &self.history_detail else {
            return;
        };
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(6),
            ])
            .split(area);

        let active_label = detail.active_tab.label();
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    format!(
                        "详情 / {} / {} / {}",
                        truncate(detail.session_id(), 28),
                        active_label,
                        detail.page_summary()
                    ),
                    Style::default()
                        .fg(Color::LightGreen)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from("这里能看到计划、过程和产物。"),
                Line::from("Tab/←→ 切页，↑↓ 滚动，PgUp/PgDn 翻页，Esc 关闭。"),
            ])
            .block(Block::default().title("历史详情").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
            sections[0],
        );
        frame.render_widget(self.history_detail_tabs_widget(area.width), sections[1]);
        let content_sections = Layout::default()
            .direction(if area.width < 120 {
                Direction::Vertical
            } else {
                Direction::Horizontal
            })
            .constraints(if area.width < 120 {
                vec![Constraint::Percentage(36), Constraint::Percentage(64)]
            } else {
                vec![Constraint::Percentage(38), Constraint::Percentage(62)]
            })
            .split(sections[2]);

        frame.render_widget(
            Paragraph::new(detail.summary.clone())
                .block(Block::default().title("用户摘要").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            content_sections[0],
        );
        frame.render_widget(
            Paragraph::new(detail.current_page_text())
                .block(Block::default().title("技术细节").borders(Borders::ALL))
                .wrap(Wrap { trim: false })
                .scroll((detail.scroll, 0)),
            content_sections[1],
        );
        frame.render_widget(
            Paragraph::new(history_detail_shortcuts_lines())
                .block(Block::default().title("快捷键").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            sections[3],
        );
    }

    fn render_manual_review_popup(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let Some(review) = &self.manual_review else {
            return;
        };

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Min(12),
                Constraint::Length(5),
            ])
            .split(area);

        let header = vec![
            Line::from(Span::styled(
                format!(
                    "人工审查 / {} / {} / {}",
                    truncate(&review.session_id, 24),
                    review.diff_view.label(),
                    review
                        .selected_file()
                        .map(|item| item.status.label())
                        .unwrap_or("无文件")
                ),
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(
                review
                    .selected_file()
                    .map(|item| format!("当前文件：{}", item.path))
                    .unwrap_or_else(|| "当前没有可审查文件。".to_string()),
            ),
            Line::from("左侧选文件，右侧执行通过/修复/交付；`[` `]` 切换 diff 视图，`Esc` 关闭。"),
        ];
        frame.render_widget(
            Paragraph::new(header)
                .block(Block::default().title("人工审查").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            sections[0],
        );

        let content = Layout::default()
            .direction(if area.width < 150 {
                Direction::Vertical
            } else {
                Direction::Horizontal
            })
            .constraints(if area.width < 150 {
                vec![
                    Constraint::Length(10),
                    Constraint::Min(10),
                    Constraint::Length(9),
                ]
            } else {
                vec![
                    Constraint::Percentage(24),
                    Constraint::Percentage(56),
                    Constraint::Percentage(20),
                ]
            })
            .split(sections[1]);

        let file_items = review
            .state
            .files
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let selected =
                    review.focus == ManualReviewFocus::Files && index == review.file_index;
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(vec![
                    Line::from(Span::styled(truncate(&item.path, 40), style)),
                    Line::from(format!(
                        "{} / {}",
                        item.status.label(),
                        item.source_workers.join("、")
                    )),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(file_items).block(
                Block::default()
                    .title("待审文件")
                    .borders(Borders::ALL)
                    .border_style(if review.focus == ManualReviewFocus::Files {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    }),
            ),
            content[0],
        );

        frame.render_widget(
            Paragraph::new(build_manual_review_detail_text(
                self.project.target_dir.as_path(),
                &review.state,
                review.file_index,
                review.diff_view,
            ))
            .block(Block::default().title("Diff / 细节").borders(Borders::ALL))
            .wrap(Wrap { trim: false })
            .scroll((review.scroll, 0)),
            content[1],
        );

        let action_items = ManualReviewAction::all()
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let selected =
                    review.focus == ManualReviewFocus::Actions && index == review.action_index;
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let (title, detail) = self.manual_review_action_line(*action);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<10}", title), style),
                    Span::raw(detail),
                ]))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(action_items).block(
                Block::default()
                    .title("动作")
                    .borders(Borders::ALL)
                    .border_style(if review.focus == ManualReviewFocus::Actions {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    }),
            ),
            content[2],
        );

        frame.render_widget(
            Paragraph::new(vec![
                Line::from("快捷键：↑↓ 选文件/动作，←→ 切焦点，Enter 执行动作，a 通过，n 需修复，e 编辑问题，f 发起返修，d 交付已通过，r 刷新，PgUp/PgDn 滚动。"),
                Line::from(format!(
                    "审查状态：已通过 {} / 待修复 {} / 返修待复查 {}",
                    review
                        .state
                        .files
                        .iter()
                        .filter(|item| item.status == ManualReviewFileStatus::Approved)
                        .count(),
                    review
                        .state
                        .files
                        .iter()
                        .filter(|item| item.status == ManualReviewFileStatus::NeedsFix)
                        .count(),
                    review
                        .state
                        .files
                        .iter()
                        .filter(|item| item.status == ManualReviewFileStatus::FixedPendingReview)
                        .count(),
                )),
            ])
            .block(Block::default().title("提示").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
            sections[2],
        );
    }

    fn manual_review_action_line(&self, action: ManualReviewAction) -> (&'static str, String) {
        match action {
            ManualReviewAction::Approve => {
                ("通过文件", "当前文件审查通过，可进入交付集合。".to_string())
            }
            ManualReviewAction::NeedsFix => {
                ("标记问题", "把当前文件标成需修复，并记录问题。".to_string())
            }
            ManualReviewAction::EditIssue => (
                "编辑问题",
                self.manual_review
                    .as_ref()
                    .and_then(|review| review.selected_file())
                    .and_then(|item| item.issue_summary.clone())
                    .unwrap_or_else(|| "补充这次返修的具体问题描述。".to_string()),
            ),
            ManualReviewAction::StartFix => (
                "发起返修",
                "只基于当前文件启动 Codex 返修子会话。".to_string(),
            ),
            ManualReviewAction::DeliverApproved => (
                "交付已通过",
                "只把已人工通过的文件交付到目标目录。".to_string(),
            ),
            ManualReviewAction::Close => ("关闭审查", "返回历史页。".to_string()),
        }
    }

    fn current_manual_review_action(&self) -> ManualReviewAction {
        ManualReviewAction::all()[self
            .manual_review
            .as_ref()
            .map(|review| review.action_index.min(ManualReviewAction::all().len() - 1))
            .unwrap_or(0)]
    }

    fn open_manual_review_popup(&mut self) -> Result<()> {
        let Some(session_id) = self
            .selected_session
            .as_ref()
            .map(|session| session.id.clone())
        else {
            self.push_notice("请先在历史页选中一个会话。");
            return Ok(());
        };
        self.open_manual_review_popup_for(&session_id, None)
    }

    fn open_manual_review_popup_for(
        &mut self,
        session_id: &str,
        preferred_file: Option<&str>,
    ) -> Result<()> {
        let session = load_session(&self.project.target_dir, Some(session_id))?;
        if !can_open_manual_review(&session) {
            self.push_notice("当前会话没有需要人工审查的文件。");
            return Ok(());
        }
        let state = load_or_initialize_manual_review_state(&self.project.target_dir, &session)?;
        let file_index = preferred_file
            .or(state.selected_file.as_deref())
            .and_then(|path| state.files.iter().position(|item| item.path == path))
            .unwrap_or(0);
        self.selected_session = Some(session);
        if let Some(index) = self
            .project
            .sessions
            .iter()
            .position(|item| item.id == session_id)
        {
            self.history_index = index;
        }
        self.manual_review = Some(ManualReviewPopupState {
            session_id: session_id.to_string(),
            state,
            file_index,
            action_index: 0,
            focus: ManualReviewFocus::Files,
            diff_view: ManualReviewDiffView::Source,
            scroll: 0,
        });
        self.navigate_to(Route::History);
        Ok(())
    }

    async fn handle_manual_review_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.manual_review.is_none() {
            return Ok(());
        }
        match key.code {
            KeyCode::Esc => {
                self.manual_review = None;
            }
            KeyCode::Tab | KeyCode::Left | KeyCode::Right
                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let review = self.manual_review.as_mut().expect("review exists");
                review.focus = match review.focus {
                    ManualReviewFocus::Files => ManualReviewFocus::Actions,
                    ManualReviewFocus::Actions => ManualReviewFocus::Files,
                };
            }
            KeyCode::Up => {
                let review = self.manual_review.as_mut().expect("review exists");
                match review.focus {
                    ManualReviewFocus::Files => {
                        let len = review.state.files.len().max(1);
                        review.file_index = cycle_index(review.file_index, len, false);
                        review.scroll = 0;
                    }
                    ManualReviewFocus::Actions => {
                        review.action_index = cycle_index(
                            review.action_index,
                            ManualReviewAction::all().len(),
                            false,
                        );
                    }
                }
            }
            KeyCode::Down => {
                let review = self.manual_review.as_mut().expect("review exists");
                match review.focus {
                    ManualReviewFocus::Files => {
                        let len = review.state.files.len().max(1);
                        review.file_index = cycle_index(review.file_index, len, true);
                        review.scroll = 0;
                    }
                    ManualReviewFocus::Actions => {
                        review.action_index =
                            cycle_index(review.action_index, ManualReviewAction::all().len(), true);
                    }
                }
            }
            KeyCode::PageUp => {
                let review = self.manual_review.as_mut().expect("review exists");
                review.scroll = review.scroll.saturating_sub(12);
            }
            KeyCode::PageDown => {
                let review = self.manual_review.as_mut().expect("review exists");
                review.scroll = review.scroll.saturating_add(12);
            }
            KeyCode::Char('[') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let review = self.manual_review.as_mut().expect("review exists");
                review.diff_view = match review.diff_view {
                    ManualReviewDiffView::Source => ManualReviewDiffView::Compare,
                    ManualReviewDiffView::LatestFix => ManualReviewDiffView::Source,
                    ManualReviewDiffView::Compare => ManualReviewDiffView::LatestFix,
                };
                review.scroll = 0;
            }
            KeyCode::Char(']') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let review = self.manual_review.as_mut().expect("review exists");
                review.diff_view = match review.diff_view {
                    ManualReviewDiffView::Source => ManualReviewDiffView::LatestFix,
                    ManualReviewDiffView::LatestFix => ManualReviewDiffView::Compare,
                    ManualReviewDiffView::Compare => ManualReviewDiffView::Source,
                };
                review.scroll = 0;
            }
            KeyCode::Char('a') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.approve_current_review_file()?;
            }
            KeyCode::Char('n') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.mark_current_review_file_needs_fix()?;
            }
            KeyCode::Char('e') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_editing(FormField::ReviewIssue);
            }
            KeyCode::Char('f') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_manual_review_fix().await?;
            }
            KeyCode::Char('d') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.deliver_manual_review_approved().await?;
            }
            KeyCode::Char('r') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let review = self.manual_review.as_ref().expect("review exists");
                let session_id = review.session_id.clone();
                let preferred = review.selected_file().map(|item| item.path.clone());
                self.open_manual_review_popup_for(&session_id, preferred.as_deref())?;
            }
            KeyCode::Enter => match self.current_manual_review_action() {
                ManualReviewAction::Approve => self.approve_current_review_file()?,
                ManualReviewAction::NeedsFix => self.mark_current_review_file_needs_fix()?,
                ManualReviewAction::EditIssue => self.start_editing(FormField::ReviewIssue),
                ManualReviewAction::StartFix => self.start_manual_review_fix().await?,
                ManualReviewAction::DeliverApproved => {
                    self.deliver_manual_review_approved().await?
                }
                ManualReviewAction::Close => self.manual_review = None,
            },
            _ => {}
        }
        Ok(())
    }

    fn history_detail_tabs_widget(&self, width: u16) -> Tabs<'static> {
        let titles = history_detail_tab_titles(width)
            .into_iter()
            .map(Line::from)
            .collect::<Vec<_>>();
        let selected = self
            .history_detail
            .as_ref()
            .and_then(|detail| {
                HistoryDetailTab::all()
                    .iter()
                    .position(|item| *item == detail.active_tab)
            })
            .unwrap_or(0);
        Tabs::new(titles)
            .select(selected)
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .block(Block::default().borders(Borders::ALL).title("详情分页"))
    }

    fn render_confirm_popup(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let Some(confirm) = &self.confirm_dialog else {
            return;
        };

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(4)])
            .split(area);
        let lines = confirm
            .lines
            .iter()
            .cloned()
            .map(Line::from)
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines)
                .block(
                    Block::default()
                        .title(confirm.title.as_str())
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: true }),
            sections[0],
        );
        frame.render_widget(
            Paragraph::new(confirm_shortcuts_lines(&confirm.action))
                .block(Block::default().title("快捷键").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            sections[1],
        );
    }

    fn render_edit_popup(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let Some(edit) = &self.edit_state else {
            return;
        };
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Min(6),
                Constraint::Length(4),
            ])
            .split(area);

        let header_lines = vec![
            Line::from(Span::styled(
                format!("编辑字段：{}", edit.field.label()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "字符数：{}   光标：{}{}",
                edit.buffer.chars().count(),
                edit.cursor,
                if edit.field == FormField::Task && !edit.history_entries.is_empty() {
                    format!(
                        "   历史：{}/{}",
                        edit.history_index.map(|index| index + 1).unwrap_or(0),
                        edit.history_entries.len()
                    )
                } else {
                    String::new()
                }
            )),
            Line::from(edit_mode_summary(edit.field)),
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
        frame.render_widget(
            Paragraph::new(
                edit_shortcuts_lines(edit.field)
                    .into_iter()
                    .chain(if edit.field == FormField::Task {
                        let lines = recent_task_history_lines(edit);
                        if lines.is_empty() {
                            vec![Line::from("当前目标目录还没有历史提示词。")]
                        } else {
                            let mut result = vec![Line::from(""), Line::from("历史提示词：")];
                            result.extend(lines);
                            result
                        }
                    } else {
                        Vec::new()
                    })
                    .collect::<Vec<_>>(),
            )
            .block(Block::default().title("快捷键").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
            sections[2],
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

    fn start_action_line(&self, action: StartAction) -> (&'static str, String) {
        match action {
            StartAction::Doctor => ("检查环境", "先排掉明显问题。".to_string()),
            StartAction::Plan => (
                "先看方案",
                "推荐主路径：先拆清楚，再决定要不要跑。".to_string(),
            ),
            StartAction::Run => ("开始执行", run_source_user_hint(&self.form)),
            StartAction::ToggleSettings => (
                if self.advanced_settings_open {
                    "收起设置"
                } else {
                    "更多设置"
                },
                if self.advanced_settings_open {
                    "回到默认模式。".to_string()
                } else {
                    "只在需要时再看。".to_string()
                },
            ),
        }
    }

    fn history_action_line(&self, action: HistoryAction) -> (&'static str, String) {
        match action {
            HistoryAction::ExecutePlan => (
                "执行此方案",
                "显式绑定当前 plan session，再进入 run。".to_string(),
            ),
            HistoryAction::DeliverAccepted => (
                "交付已接收",
                "仅把 accepted_files 安全落地到目标目录。".to_string(),
            ),
            HistoryAction::ManualReview => (
                "人工审查",
                "逐文件审查 manual_review_files，并可发起单文件返修。".to_string(),
            ),
            HistoryAction::Continue => {
                if self
                    .selected_session
                    .as_ref()
                    .is_some_and(|session| session.is_plan_session())
                {
                    ("继续改方案", "基于这一轮继续打磨方案。".to_string())
                } else {
                    ("继续优化", "基于这一轮继续打磨。".to_string())
                }
            }
            HistoryAction::EditFeedback => (
                "补充反馈",
                if self.form.continue_feedback.trim().is_empty() {
                    "可写，也可以留空直接继续。".to_string()
                } else {
                    truncate(&self.form.continue_feedback, 56)
                },
            ),
            HistoryAction::ContinueMode => (
                "继续模式",
                format!(
                    "当前：{}",
                    continue_mode_user_label(self.form.continue_mode)
                ),
            ),
            HistoryAction::Replay => ("回放过程", "重新看一遍关键过程。".to_string()),
            HistoryAction::Detail => ("查看详情", "看完整计划、过程和产物。".to_string()),
            HistoryAction::ResetSelected => (
                "一键重置",
                "回退这一轮自动提交，并抹掉对应历史。".to_string(),
            ),
            HistoryAction::CleanSelected => ("删除当前", "删除这一轮及其后续。".to_string()),
            HistoryAction::CleanAll => ("清空全部", "清掉这个仓库的全部历史。".to_string()),
            HistoryAction::BackToStart => ("回开始页", "回去改任务或重新开始。".to_string()),
        }
    }

    fn run_action_line(&self, action: RunAction) -> (&'static str, String) {
        match action {
            RunAction::Back => (
                "返回上一级",
                if self.is_command_running() {
                    "只离开页面，不会停任务。".to_string()
                } else {
                    "回到上一个页面。".to_string()
                },
            ),
            RunAction::Stop => {
                let command = self.active_command.as_ref();
                (
                    if command.is_some_and(|item| item.state.is_running() && item.stop_requested) {
                        "停止请求已发出"
                    } else if command
                        .is_some_and(|item| item.state.is_running() && item.cancel_tx.is_some())
                    {
                        "停止当前动作"
                    } else {
                        "停止不可用"
                    },
                    if command.is_some_and(|item| item.state.is_running() && item.stop_requested) {
                        "系统正在等待安全收口完成。".to_string()
                    } else if command
                        .is_some_and(|item| item.state.is_running() && item.cancel_tx.is_some())
                    {
                        "安全停止，不是强杀。".to_string()
                    } else if command.is_some_and(|item| item.state.is_running()) {
                        "当前动作暂不支持安全停止。".to_string()
                    } else {
                        "现在没有可停的动作。".to_string()
                    },
                )
            }
            RunAction::ViewHistory => ("查看历史", "去看结果、回放或继续优化。".to_string()),
            RunAction::BackToStart => ("回开始页", "回去写任务或重新开始。".to_string()),
        }
    }

    fn current_start_action(&self) -> StartAction {
        StartAction::all()[self.start_action_index.min(StartAction::all().len() - 1)]
    }

    fn current_history_action(&self) -> HistoryAction {
        let actions = available_history_actions(self.selected_session.as_ref());
        actions[self.history_action_index.min(actions.len() - 1)]
    }

    fn current_run_action(&self) -> RunAction {
        RunAction::all()[self.run_action_index.min(RunAction::all().len() - 1)]
    }

    fn current_nav_route(&self) -> Route {
        Route::all()[self.nav_index.min(Route::all().len() - 1)]
    }

    fn restore_page_focus(&mut self) {
        match self.route {
            Route::Start => {
                if !self.advanced_settings_open && self.start_focus == StartFocus::AdvancedFields {
                    self.start_focus = StartFocus::TaskInput;
                }
            }
            Route::Run => {
                self.run_focus = RunFocus::Subviews;
            }
            Route::History => {
                self.history_focus = HistoryFocus::Sessions;
            }
        }
    }

    async fn activate_start_focus(&mut self) -> Result<()> {
        match self.start_focus {
            StartFocus::TaskInput => self.start_editing(FormField::Task),
            StartFocus::Actions => match self.current_start_action() {
                StartAction::Doctor => self.start_action(ShellAction::Doctor).await?,
                StartAction::Plan => self.start_action(ShellAction::Plan).await?,
                StartAction::Run => self.start_action(ShellAction::Run).await?,
                StartAction::ToggleSettings => self.toggle_advanced_settings(),
            },
            StartFocus::AdvancedFields => {
                let field = advanced_fields()[self.selected_field];
                if field_is_editable(field) {
                    self.start_editing(field);
                } else {
                    self.cycle_current_field(true);
                }
            }
        }
        Ok(())
    }

    async fn activate_history_focus(&mut self) -> Result<()> {
        match self.history_focus {
            HistoryFocus::Sessions => self.open_history_detail(),
            HistoryFocus::Actions => match self.current_history_action() {
                HistoryAction::ExecutePlan => {
                    if let Some(session) = self.selected_session.clone() {
                        self.use_plan_session_for_run(&session);
                        self.start_action(ShellAction::Run).await?;
                    }
                }
                HistoryAction::DeliverAccepted => self.deliver_selected_session_accepted().await?,
                HistoryAction::ManualReview => self.open_manual_review_popup()?,
                HistoryAction::Continue => self.start_action(ShellAction::ContinueSelected).await?,
                HistoryAction::EditFeedback => self.start_editing(FormField::ContinueFeedback),
                HistoryAction::ContinueMode => {
                    self.form.continue_mode = cycle_continue_mode(self.form.continue_mode, true);
                    self.push_notice(&format!(
                        "继续模式已切换为：{}。",
                        continue_mode_user_label(self.form.continue_mode)
                    ));
                }
                HistoryAction::Replay => self.start_action(ShellAction::ReplaySelected).await?,
                HistoryAction::Detail => self.open_history_detail(),
                HistoryAction::ResetSelected => self.open_reset_selected_confirm(),
                HistoryAction::CleanSelected => self.open_clean_selected_confirm(),
                HistoryAction::CleanAll => self.open_clean_all_confirm(),
                HistoryAction::BackToStart => {
                    self.navigate_to(Route::Start);
                    self.push_notice("已返回开始页；如需更多配置，请在右侧动作区打开“更多设置”。");
                }
            },
        }
        Ok(())
    }

    fn activate_run_focus(&mut self) {
        match self.run_focus {
            RunFocus::Subviews => {}
            RunFocus::Actions => match self.current_run_action() {
                RunAction::Back => {
                    let target = run_back_route(self.run_return_route);
                    self.route = target;
                    self.push_notice(if self.is_command_running() {
                        match target {
                            Route::Start => "已离开执行页；后台动作仍在运行，可随时再回来查看。",
                            Route::Run => "仍停留在执行页。",
                            Route::History => "已切到历史页；后台动作仍在运行。",
                        }
                    } else {
                        match target {
                            Route::Start => "已返回开始页。",
                            Route::Run => "已返回执行页。",
                            Route::History => "已返回历史页。",
                        }
                    });
                }
                RunAction::Stop => self.stop_active_command(),
                RunAction::ViewHistory => self.navigate_to(Route::History),
                RunAction::BackToStart => self.navigate_to(Route::Start),
            },
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }

        if matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return Ok(());
        }

        if !matches!(key.code, KeyCode::Esc) {
            self.exit_esc_armed_at = None;
        }

        if self.edit_state.is_some() {
            return self.handle_edit_key(key).await;
        }

        if self.manual_review.is_some() {
            return self.handle_manual_review_key(key).await;
        }

        if self.history_detail.is_some() {
            return self.handle_history_detail_key(key);
        }

        if self.confirm_dialog.is_some() {
            return self.handle_confirm_key(key);
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
            KeyCode::Esc => {
                if self.is_exit_armable_root() {
                    self.handle_exit_escape();
                } else {
                    self.handle_global_back();
                }
            }
            KeyCode::Char('a') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.route == Route::Start {
                    self.toggle_advanced_settings();
                } else {
                    self.navigate_to(Route::Start);
                    self.start_focus = StartFocus::Actions;
                    self.start_action_index = StartAction::all()
                        .iter()
                        .position(|item| *item == StartAction::ToggleSettings)
                        .unwrap_or(0);
                    self.push_notice("已回到开始页；按 Enter 可打开“更多设置”。");
                }
            }
            KeyCode::Char('g') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.refresh_project(true)?
            }
            KeyCode::Char('m') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                match self.route {
                    Route::Start => {
                        self.form.thinking_mode = cycle_thinking_mode(self.form.thinking_mode, true)
                    }
                    Route::History => {
                        self.form.continue_mode = cycle_continue_mode(self.form.continue_mode, true)
                    }
                    Route::Run => self.cycle_run_subview(true),
                }
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
            KeyCode::Char('y') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.deliver_selected_session_accepted().await?
            }
            KeyCode::Char('u') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_manual_review_popup()?
            }
            KeyCode::Char('c') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_action(ShellAction::ContinueSelected).await?
            }
            KeyCode::Char('l') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_action(ShellAction::ReplaySelected).await?
            }
            KeyCode::Char('z') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_reset_selected_confirm()
            }
            KeyCode::Char('x') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_clean_selected_confirm()
            }
            KeyCode::Char('X') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_clean_all_confirm()
            }
            KeyCode::Char('v') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_history_detail()
            }
            KeyCode::Char('s') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.stop_active_command()
            }
            KeyCode::Char('[') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.route == Route::Run && !self.nav_focus {
                    self.cycle_run_subview(false);
                }
            }
            KeyCode::Char(']') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.route == Route::Run && !self.nav_focus {
                    self.cycle_run_subview(true);
                }
            }
            KeyCode::Tab => self.handle_tab_navigation(true),
            KeyCode::BackTab => self.handle_tab_navigation(false),
            KeyCode::Up => self.handle_up_down(-1),
            KeyCode::Down => self.handle_up_down(1),
            KeyCode::Left => self.handle_left_right(false),
            KeyCode::Right => self.handle_left_right(true),
            KeyCode::Enter => self.handle_enter().await?,
            KeyCode::Char('e') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                match self.route {
                    Route::Start => self.activate_start_focus().await?,
                    Route::History => {
                        if self.selected_session.is_some() {
                            self.start_editing(FormField::ContinueFeedback);
                        } else {
                            self.open_history_selection()?;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_history_detail_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(detail) = &mut self.history_detail else {
            return Ok(());
        };
        let mut should_close = false;

        match key.code {
            KeyCode::Esc | KeyCode::Char('v') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                should_close = true;
            }
            KeyCode::Tab | KeyCode::Char(']') | KeyCode::Right
                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                detail.cycle_tab(true);
            }
            KeyCode::BackTab | KeyCode::Char('[') | KeyCode::Left
                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                detail.cycle_tab(false);
            }
            KeyCode::Up => detail.scroll_lines(-1),
            KeyCode::Down => detail.scroll_lines(1),
            KeyCode::PageUp => detail.previous_page(),
            KeyCode::PageDown => detail.next_page(),
            KeyCode::Home => detail.first_page(),
            KeyCode::End => detail.last_page(),
            KeyCode::Char('k') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                detail.scroll_lines(-1)
            }
            KeyCode::Char('j') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                detail.scroll_lines(1)
            }
            _ => {}
        }

        if should_close {
            self.history_detail = None;
            self.push_notice("已关闭历史详情。");
        }

        Ok(())
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.confirm_dialog = None;
                self.push_notice("已取消当前确认操作。");
            }
            KeyCode::Enter => {
                if let Err(error) = self.execute_confirm_action() {
                    self.confirm_dialog = None;
                    self.push_notice(&format!("操作失败：{error:#}"));
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_edit_key(&mut self, key: KeyEvent) -> Result<()> {
        let mut commit = false;
        let mut saved_field = None;
        let mut focus_after_commit = None;
        let mut cancel = false;

        {
            let Some(edit) = &mut self.edit_state else {
                return Ok(());
            };
            let multiline = matches!(
                edit.field,
                FormField::Task | FormField::ContinueFeedback | FormField::ReviewIssue
            );
            match key.code {
                KeyCode::Esc => {
                    cancel = true;
                }
                KeyCode::Enter if multiline && !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    insert_char_at_cursor(edit, '\n');
                }
                KeyCode::Enter => {
                    commit = true;
                    saved_field = Some(edit.field);
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
                    saved_field = Some(edit.field);
                }
                KeyCode::Char('p')
                    if multiline && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    commit = true;
                    saved_field = Some(edit.field);
                    focus_after_commit = Some(ShellAction::Plan);
                }
                KeyCode::Char('r')
                    if multiline && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    commit = true;
                    saved_field = Some(edit.field);
                    focus_after_commit = Some(match edit.field {
                        FormField::ContinueFeedback => ShellAction::ContinueSelected,
                        _ => ShellAction::Run,
                    });
                }
                KeyCode::Char('j')
                    if edit.field == FormField::Task
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    cycle_edit_history(edit, true);
                }
                KeyCode::Char('k')
                    if edit.field == FormField::Task
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    cycle_edit_history(edit, false);
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    insert_char_at_cursor(edit, ch);
                }
                _ => {}
            }
        }

        if cancel {
            self.edit_state = None;
            self.push_notice("已取消当前编辑，原值未变。");
            return Ok(());
        }

        if commit {
            self.commit_edit()?;
            if let Some(action) = focus_after_commit {
                let message = self.focus_action_after_save(action);
                self.push_notice(&message);
            } else if let Some(field) = saved_field {
                self.push_notice(&saved_field_notice(field));
            }
        }

        Ok(())
    }

    fn handle_tab_navigation(&mut self, forward: bool) {
        if self.nav_focus {
            self.nav_focus = false;
            self.restore_page_focus();
            return;
        }

        let _ = forward;
        self.nav_focus = true;
        self.nav_index = Route::all()
            .iter()
            .position(|item| *item == self.route)
            .unwrap_or(0);
    }

    fn is_exit_armable_root(&self) -> bool {
        self.route == Route::Start
            && !self.advanced_settings_open
            && !self.nav_focus
            && self.edit_state.is_none()
            && self.history_detail.is_none()
            && self.manual_review.is_none()
            && self.confirm_dialog.is_none()
    }

    fn handle_exit_escape(&mut self) {
        let now = Instant::now();
        if self
            .exit_esc_armed_at
            .is_some_and(|armed_at| now.duration_since(armed_at) <= EXIT_ESC_ARM_WINDOW)
        {
            self.exit_esc_armed_at = None;
            self.open_quit_confirm();
            return;
        }

        self.exit_esc_armed_at = Some(now);
        self.push_notice("再次按 `Esc` 将弹出退出确认；也可直接按 `Ctrl+C` 退出。");
    }

    fn handle_up_down(&mut self, delta: isize) {
        if self.nav_focus {
            if delta > 0 {
                self.nav_focus = false;
                self.restore_page_focus();
            }
            return;
        }

        match self.route {
            Route::Start => match self.start_focus {
                StartFocus::TaskInput => {
                    if delta < 0 {
                        self.nav_focus = true;
                    }
                }
                StartFocus::Actions => {
                    let len = StartAction::all().len() as isize;
                    self.start_action_index =
                        ((self.start_action_index as isize + delta).rem_euclid(len)) as usize;
                }
                StartFocus::AdvancedFields => {
                    let len = advanced_fields().len() as isize;
                    self.selected_field =
                        ((self.selected_field as isize + delta).rem_euclid(len)) as usize;
                }
            },
            Route::Run => match self.run_focus {
                RunFocus::Subviews => {
                    if delta < 0 {
                        self.nav_focus = true;
                    } else {
                        self.run_focus = RunFocus::Actions;
                    }
                }
                RunFocus::Actions => {
                    let len = RunAction::all().len() as isize;
                    self.run_action_index =
                        ((self.run_action_index as isize + delta).rem_euclid(len)) as usize;
                }
            },
            Route::History => match self.history_focus {
                HistoryFocus::Sessions => {
                    if delta < 0 && self.history_index == 0 {
                        self.nav_focus = true;
                    } else {
                        let len = self.project.sessions.len().max(1) as isize;
                        self.history_index =
                            ((self.history_index as isize + delta).rem_euclid(len)) as usize;
                        if let Some(item) = self.project.sessions.get(self.history_index) {
                            self.selected_session =
                                load_session(&self.project.target_dir, Some(&item.id)).ok();
                            self.refresh_history_detail_if_needed();
                        }
                    }
                }
                HistoryFocus::Actions => {
                    let len =
                        available_history_actions(self.selected_session.as_ref()).len() as isize;
                    self.history_action_index =
                        ((self.history_action_index as isize + delta).rem_euclid(len)) as usize;
                }
            },
        }
    }

    fn handle_left_right(&mut self, forward: bool) {
        if self.nav_focus {
            let current = self.nav_index.min(Route::all().len().saturating_sub(1));
            self.nav_index = cycle_index(current, Route::all().len(), forward);
            return;
        }

        match self.route {
            Route::Start => self.cycle_start_focus(forward),
            Route::Run => match self.run_focus {
                RunFocus::Subviews => self.cycle_run_subview(forward),
                RunFocus::Actions => {}
            },
            Route::History => self.cycle_history_focus(forward),
        }
    }

    async fn handle_enter(&mut self) -> Result<()> {
        if self.nav_focus {
            self.navigate_via_tab(self.current_nav_route());
            return Ok(());
        }

        match self.route {
            Route::Start => self.activate_start_focus().await?,
            Route::Run => self.activate_run_focus(),
            Route::History => self.activate_history_focus().await?,
        }
        Ok(())
    }

    fn cycle_start_focus(&mut self, forward: bool) {
        let order = if self.advanced_settings_open {
            vec![
                StartFocus::TaskInput,
                StartFocus::Actions,
                StartFocus::AdvancedFields,
            ]
        } else {
            vec![StartFocus::TaskInput, StartFocus::Actions]
        };
        let current = order
            .iter()
            .position(|item| *item == self.start_focus)
            .unwrap_or(0);
        self.start_focus = order[cycle_index(current, order.len(), forward)];
    }

    fn cycle_history_focus(&mut self, forward: bool) {
        let order = [HistoryFocus::Sessions, HistoryFocus::Actions];
        let current = order
            .iter()
            .position(|item| *item == self.history_focus)
            .unwrap_or(0);
        self.history_focus = order[cycle_index(current, order.len(), forward)];
    }

    fn cycle_current_field(&mut self, forward: bool) {
        if self.route != Route::Start || !self.advanced_settings_open {
            return;
        }

        match advanced_fields()[self.selected_field] {
            FormField::ContinueMode => {
                self.form.continue_mode = cycle_continue_mode(self.form.continue_mode, forward);
            }
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
            self.push_notice("后台动作仍在执行；你仍可回到开始页查看，但更多设置不会自动展开。");
        }

        if self.route != Route::Start {
            self.navigate_to(Route::Start);
            self.start_focus = StartFocus::Actions;
            self.start_action_index = StartAction::all()
                .iter()
                .position(|item| *item == StartAction::ToggleSettings)
                .unwrap_or(0);
            return;
        }

        let opening = !self.advanced_settings_open;
        self.advanced_settings_open = opening;
        self.selected_field = 0;
        self.start_focus = if opening {
            StartFocus::AdvancedFields
        } else {
            StartFocus::Actions
        };
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

        if !self.ensure_task_ready(action) {
            return Ok(());
        }
        if !self.ensure_continue_ready(action) {
            return Ok(());
        }

        let preview = self.command_preview_lines(action);
        let supports_stop =
            action_supports_stop(action, &self.form, self.selected_session.as_ref());
        let (tx, rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = if supports_stop {
            let (cancel_tx, cancel_rx) = watch::channel(false);
            (Some(cancel_tx), Some(cancel_rx))
        } else {
            (None, None)
        };
        if let Some(runtime_state) =
            prepare_runtime_state(action, &self.form, self.selected_session.as_ref())
        {
            self.runtime_state = Some(runtime_state);
        }
        self.run_subview = preferred_run_subview(action);
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
            state: CommandState::Running,
            started_at: Instant::now(),
            finished_at: None,
            stop_requested: false,
            output: initial_command_output(&preview, &self.project.display_target, supports_stop),
            cancel_tx,
            rx,
        });
        self.push_notice(&format!(
            "已开始：{}。{}",
            action.label(),
            truncate(&preview.summary, 56)
        ));
        self.navigate_to(Route::Run);
        Ok(())
    }

    fn ensure_task_ready(&mut self, action: ShellAction) -> bool {
        if !action.requires_task() || !self.form.task.trim().is_empty() {
            return true;
        }

        self.navigate_to(Route::Start);
        self.advanced_settings_open = false;
        self.start_editing(FormField::Task);
        self.push_notice("请先输入提示词，再生成方案或开始执行。");
        false
    }

    fn ensure_continue_ready(&mut self, action: ShellAction) -> bool {
        if action != ShellAction::ContinueSelected {
            return true;
        }

        if self.selected_session.is_none() {
            self.navigate_to(Route::History);
            self.push_notice("请先在历史页选中一个已完成 session，再继续优化。");
            return false;
        }
        if !self
            .selected_session
            .as_ref()
            .is_some_and(SessionManifest::continuable)
        {
            self.navigate_to(Route::History);
            self.push_notice("当前 session 还不能继续迭代；请先选一个已完成会话。");
            return false;
        }
        true
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
                        self.project.last_error = if report.ok {
                            None
                        } else {
                            Some(build_doctor_failure_summary(&report))
                        };
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
                        push_command_output(
                            &mut command.output,
                            format!(
                                "doctor 结论：{} / {}",
                                report.readiness.label(),
                                report.summary
                            ),
                        );
                    }
                    RunnerEvent::Finished { state, manifest } => {
                        command.state = state;
                        command.finished_at = Some(Instant::now());
                        push_command_output(
                            &mut command.output,
                            format!("动作结束：{} / {}", command.action.label(), state.label()),
                        );
                        finished = Some((command.action, state, *manifest));
                    }
                }
            }
        }

        if let Some((action, state, manifest)) = finished {
            if action == ShellAction::ReviewFixSelected {
                let pending = self.pending_review_fix.take();
                if let (Some(pending), Some(child_manifest)) = (pending, manifest.as_ref()) {
                    record_review_fix_completion(
                        &self.project.target_dir,
                        &pending.parent_session_id,
                        &pending.target_file,
                        child_manifest,
                    )?;
                    self.refresh_project(false)?;
                    self.open_manual_review_popup_for(
                        &pending.parent_session_id,
                        Some(&pending.target_file),
                    )?;
                    self.push_notice(&format!(
                        "单文件返修已结束：{} / {}",
                        pending.target_file,
                        state.label()
                    ));
                    self.navigate_to(Route::History);
                    return Ok(());
                }
            }
            if let Some(manifest) = manifest {
                if let Some(runtime_state) = &mut self.runtime_state {
                    runtime_state.set_identity(manifest.id.clone(), manifest.task.clone());
                }
                self.selected_session = Some(manifest);
                self.refresh_history_detail_if_needed();
            }
            self.push_notice(&format!("{} 已结束：{}", action.label(), state.label()));
            self.refresh_project(false)?;
            if !self.project.sessions.is_empty() {
                self.history_index = 0;
                self.open_history_selection()?;
                if matches!(state, CommandState::Succeeded)
                    && matches!(
                        action,
                        ShellAction::Run | ShellAction::Plan | ShellAction::ContinueSelected
                    )
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
        if command.stop_requested {
            self.push_notice("停止请求已发出，正在等待安全收口。");
            self.navigate_to(Route::Run);
            return;
        }
        let Some(cancel_tx) = command.cancel_tx.clone() else {
            self.push_notice("当前动作暂不支持安全停止；支持停止时，执行页会明确提示。");
            self.navigate_to(Route::Run);
            return;
        };
        // UI 层只负责发送停止信号，不直接粗暴 abort 后台任务；
        // 真正的停止、收敛与清理由 orchestrator / worker 协作完成。
        match cancel_tx.send(true) {
            Ok(_) => {
                command.stop_requested = true;
                push_command_output(
                    &mut command.output,
                    "停止请求已发送，等待在跑 worker / replay 安全退出…".to_string(),
                );
                self.push_notice("停止请求已发送，正在等待安全收口。");
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
        self.nav_focus = false;
        self.nav_index = Route::all()
            .iter()
            .position(|item| *item == route)
            .unwrap_or(0);
        self.restore_page_focus();
    }

    fn navigate_via_tab(&mut self, route: Route) {
        if self.is_command_running() && route != Route::Run {
            self.push_notice(
                "当前动作仍在后台执行；你可以继续切页查看信息，或回执行页发送停止信号。",
            );
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
        self.exit_esc_armed_at = None;
        if self.nav_focus {
            self.nav_focus = false;
            self.restore_page_focus();
            self.push_notice("已离开顶部导航。");
            return;
        }
        if self.manual_review.is_some() {
            self.manual_review = None;
            self.push_notice("已关闭人工审查。");
            return;
        }
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
            self.refresh_history_detail_if_needed();
        } else {
            self.selected_session = None;
            self.history_detail = None;
        }
        Ok(())
    }

    fn open_history_selection(&mut self) -> Result<()> {
        if let Some(item) = self.project.sessions.get(self.history_index) {
            self.selected_session = Some(load_session(&self.project.target_dir, Some(&item.id))?);
            self.refresh_history_detail_if_needed();
            self.navigate_to(Route::History);
        }
        Ok(())
    }

    fn open_history_detail(&mut self) {
        if self.route != Route::History {
            self.navigate_to(Route::History);
        }
        if let Some(session) = &self.selected_session {
            self.history_detail = Some(HistoryDetailState::from_session(session));
            self.push_notice("已打开历史详情，可切换查看计划、运行和产物原文。");
        } else {
            self.push_notice("请先在历史页选中一个会话。");
        }
    }

    fn open_clean_selected_confirm(&mut self) {
        if self.route != Route::History {
            return;
        }
        let Some(session) = &self.selected_session else {
            self.push_notice("请先在历史页选中一个会话。");
            return;
        };
        self.confirm_dialog = Some(ConfirmDialogState {
            action: ConfirmAction::CleanSelected {
                session_id: session.id.clone(),
            },
            title: "确认清理当前历史".to_string(),
            lines: vec![
                format!("将删除 session `{}`。", session.id),
                "如果有基于它继续生成的后续迭代，也会一起删除，避免 lineage 悬空。".to_string(),
                "对应 workers / integration / summary 等 `.codex-forge` 产物也会一并清掉。"
                    .to_string(),
                "按 Enter 确认，按 Esc 取消。".to_string(),
            ],
        });
    }

    fn open_reset_selected_confirm(&mut self) {
        if self.route != Route::History {
            return;
        }
        let Some(session) = &self.selected_session else {
            self.push_notice("请先在历史页选中一个会话。");
            return;
        };
        self.confirm_dialog = Some(ConfirmDialogState {
            action: ConfirmAction::ResetSelected {
                session_id: session.id.clone(),
            },
            title: "确认一键重置".to_string(),
            lines: vec![
                format!(
                    "将回退 session `{}` 及其后续迭代落地的自动提交。",
                    session.id
                ),
                "只有当这些提交仍位于当前 HEAD 尾部时，系统才会执行回滚，避免误删后续人工提交。"
                    .to_string(),
                "回滚成功后，对应 `.codex-forge` 历史、worker 产物和 summary 也会一并删除。"
                    .to_string(),
                "按 Enter 确认，按 Esc 取消。".to_string(),
            ],
        });
    }

    fn open_clean_all_confirm(&mut self) {
        if self.route != Route::History {
            return;
        }
        self.confirm_dialog = Some(ConfirmDialogState {
            action: ConfirmAction::CleanAll,
            title: "确认一键清空".to_string(),
            lines: vec![
                format!(
                    "将删除当前仓库下整个 `{}` 目录。",
                    self.project.target_dir.join(".codex-forge").display()
                ),
                "所有历史 session、回放文件、worker 产物和 summary 都会被清空。".to_string(),
                "按 Enter 确认，按 Esc 取消。".to_string(),
            ],
        });
    }

    fn open_quit_confirm(&mut self) {
        self.confirm_dialog = Some(ConfirmDialogState {
            action: ConfirmAction::Quit,
            title: "确认退出 TUI".to_string(),
            lines: vec![
                "将退出当前 codex-forge TUI 界面。".to_string(),
                "不会中断已经写入磁盘的历史会话；如果后台动作仍在执行，退出后你可以下次重新进入查看。".to_string(),
                "按 Enter 确认退出，按 Esc 取消。".to_string(),
            ],
        });
    }

    fn refresh_history_detail_if_needed(&mut self) {
        let current_session_id = self
            .history_detail
            .as_ref()
            .map(|detail| detail.session_id().to_string());
        let Some(current_session_id) = current_session_id else {
            return;
        };
        let Some(session) = &self.selected_session else {
            self.history_detail = None;
            return;
        };
        if current_session_id != session.id {
            self.history_detail = Some(HistoryDetailState::from_session(session));
        }
    }

    async fn deliver_selected_session_accepted(&mut self) -> Result<()> {
        let Some(session) = self.selected_session.clone() else {
            self.push_notice("请先在历史页选中一个会话。");
            return Ok(());
        };
        if !can_deliver_accepted_files(&session) {
            self.push_notice("当前会话没有可安全交付的 accepted_files。");
            return Ok(());
        }

        let plan = load_apply_plan_for_session(&session)?;
        let apply_result = session
            .apply_result
            .clone()
            .with_context(|| format!("session `{}` 缺少 apply_result", session.id))?;
        match deliver_accepted_files(&plan, &apply_result, session.repo_root()).await {
            Ok(delivered_files) => {
                let skipped_files = apply_result
                    .manual_review_files
                    .iter()
                    .filter(|file| !delivered_files.contains(*file))
                    .cloned()
                    .collect::<Vec<_>>();
                persist_manual_delivery_result(
                    &self.project.target_dir,
                    &session.id,
                    ManualDeliveryResult {
                        delivered_at: Utc::now(),
                        target_dir: session.repo_root().to_path_buf(),
                        delivered_files: delivered_files.clone(),
                        skipped_files,
                        success: true,
                        source_apply_status: Some(apply_result.status),
                        review_gate: apply_result.review_gate,
                        error: None,
                    },
                )?;
                self.refresh_project(false)?;
                self.push_notice(&format!(
                    "已将 {} 个 accepted_files 安全交付到目标目录。",
                    delivered_files.len()
                ));
            }
            Err(error) => {
                persist_manual_delivery_result(
                    &self.project.target_dir,
                    &session.id,
                    ManualDeliveryResult {
                        delivered_at: Utc::now(),
                        target_dir: session.repo_root().to_path_buf(),
                        delivered_files: Vec::new(),
                        skipped_files: apply_result.manual_review_files.clone(),
                        success: false,
                        source_apply_status: Some(apply_result.status),
                        review_gate: apply_result.review_gate,
                        error: Some(error.to_string()),
                    },
                )?;
                self.refresh_project(false)?;
                self.push_notice(&format!("安全交付失败：{error}"));
            }
        }
        Ok(())
    }

    fn approve_current_review_file(&mut self) -> Result<()> {
        let Some(review) = &mut self.manual_review else {
            return Ok(());
        };
        let Some(index) = review
            .state
            .files
            .get(review.file_index)
            .map(|_| review.file_index)
        else {
            return Ok(());
        };
        review.state.files[index].status = ManualReviewFileStatus::Approved;
        let file_path = review.state.files[index].path.clone();
        review.state.selected_file = Some(file_path.clone());
        let session_id = review.session_id.clone();
        let state = review.state.clone();
        persist_manual_review_state(&self.project.target_dir, &session_id, state)?;
        self.refresh_project(false)?;
        self.push_notice(&format!("已通过文件：{}", file_path));
        Ok(())
    }

    fn mark_current_review_file_needs_fix(&mut self) -> Result<()> {
        let Some(review) = &mut self.manual_review else {
            return Ok(());
        };
        let Some(index) = review
            .state
            .files
            .get(review.file_index)
            .map(|_| review.file_index)
        else {
            return Ok(());
        };
        review.state.files[index].status = ManualReviewFileStatus::NeedsFix;
        if review.state.files[index]
            .issue_summary
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            let file_path = review.state.files[index].path.clone();
            review.state.files[index].issue_summary = Some(format!(
                "请修复文件 `{}` 的人工审查问题，并保持改动范围只限该文件。",
                file_path
            ));
        }
        let file_path = review.state.files[index].path.clone();
        review.state.selected_file = Some(file_path.clone());
        let session_id = review.session_id.clone();
        let state = review.state.clone();
        persist_manual_review_state(&self.project.target_dir, &session_id, state)?;
        self.refresh_project(false)?;
        self.push_notice(&format!("已标记需修复：{}", file_path));
        Ok(())
    }

    async fn start_manual_review_fix(&mut self) -> Result<()> {
        if self
            .active_command
            .as_ref()
            .is_some_and(|command| command.state.is_running())
        {
            self.push_notice("已有动作在执行，请等待当前命令结束。");
            self.navigate_to(Route::Run);
            return Ok(());
        }
        let Some(review) = &self.manual_review else {
            return Ok(());
        };
        let Some(parent_session) = self.selected_session.clone() else {
            self.push_notice("请先在历史页选中一个会话。");
            return Ok(());
        };
        let Some(file) = review.selected_file() else {
            self.push_notice("当前没有可返修文件。");
            return Ok(());
        };
        let issue = file.issue_summary.clone().unwrap_or_else(|| {
            format!(
                "请只基于文件 `{}` 修复人工审查问题，并保留其余文件不动。",
                file.path
            )
        });

        let (tx, rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.pending_review_fix = Some(PendingReviewFix {
            parent_session_id: review.session_id.clone(),
            target_file: file.path.clone(),
        });
        self.runtime_state = Some(RuntimeViewState::new(
            &parent_session.id,
            &format!("人工审查返修 {}", file.path),
        ));
        self.run_subview = RunSubview::Timeline;
        spawn_review_fix_action(
            self.project.target_dir.clone(),
            parent_session,
            file.path.clone(),
            issue.clone(),
            tx,
            Some(cancel_rx),
        );
        self.active_command = Some(ActiveCommand {
            action: ShellAction::ReviewFixSelected,
            state: CommandState::Running,
            started_at: Instant::now(),
            finished_at: None,
            stop_requested: false,
            output: vec![
                format!("准备动作：修复当前文件 {}", file.path),
                format!("审查问题：{}", issue),
                "返修完成后会自动回到人工审查页，并展示修复前后 diff。".to_string(),
            ],
            cancel_tx: Some(cancel_tx),
            rx,
        });
        self.push_notice(&format!("已开始单文件返修：{}", file.path));
        self.navigate_to(Route::Run);
        Ok(())
    }

    async fn deliver_manual_review_approved(&mut self) -> Result<()> {
        let Some(review) = &self.manual_review else {
            return Ok(());
        };
        let Some(session) = self.selected_session.clone() else {
            self.push_notice("请先在历史页选中一个会话。");
            return Ok(());
        };
        let review_state = review.state.clone();
        let approved_files = review_state
            .files
            .iter()
            .filter(|item| item.status == ManualReviewFileStatus::Approved)
            .map(|item| item.path.clone())
            .collect::<Vec<_>>();
        if approved_files.is_empty() {
            self.push_notice("当前还没有人工通过的文件。");
            return Ok(());
        }

        match deliver_manual_review_approved_files(
            &self.project.target_dir,
            &session.id,
            &review_state,
            session.repo_root(),
        )
        .await
        {
            Ok(delivered_files) => {
                persist_manual_delivery_result(
                    &self.project.target_dir,
                    &session.id,
                    ManualDeliveryResult {
                        delivered_at: Utc::now(),
                        target_dir: session.repo_root().to_path_buf(),
                        delivered_files: delivered_files.clone(),
                        skipped_files: review_state
                            .files
                            .iter()
                            .filter(|item| item.status != ManualReviewFileStatus::Approved)
                            .map(|item| item.path.clone())
                            .collect(),
                        success: true,
                        source_apply_status: session.apply_result.as_ref().map(|item| item.status),
                        review_gate: session
                            .apply_result
                            .as_ref()
                            .and_then(|item| item.review_gate),
                        error: None,
                    },
                )?;
                self.refresh_project(false)?;
                self.push_notice(&format!(
                    "已将 {} 个人工通过文件交付到目标目录。",
                    delivered_files.len()
                ));
            }
            Err(error) => {
                self.push_notice(&format!("人工审查交付失败：{error}"));
            }
        }
        Ok(())
    }

    fn push_notice(&mut self, message: &str) {
        if self.notices.last().is_some_and(|last| last == message) {
            return;
        }
        self.notices.push(message.to_string());
        if self.notices.len() > MAX_NOTICE_LINES {
            let overflow = self.notices.len() - MAX_NOTICE_LINES;
            self.notices.drain(0..overflow);
        }
    }

    fn execute_confirm_action(&mut self) -> Result<()> {
        let Some(confirm) = self.confirm_dialog.take() else {
            return Ok(());
        };

        let message = match confirm.action {
            ConfirmAction::ResetSelected { session_id } => {
                let report = reset_session_lineage(&self.project.target_dir, &session_id)?;
                if let Some(reset_to) = report.reset_to {
                    format!(
                        "已回滚 {} 个 commit，重置到 {}，并清理 {} 个 session。",
                        report.reset_commits.len(),
                        truncate(&reset_to, 12),
                        report.removed_sessions.len()
                    )
                } else {
                    format!(
                        "目标 session 没有落地 commit，已直接清理 {} 个 session。",
                        report.removed_sessions.len()
                    )
                }
            }
            ConfirmAction::CleanSelected { session_id } => {
                let report = cleanup_session_lineage(&self.project.target_dir, &session_id)?;
                format!(
                    "已清理 {} 个 session：{}",
                    report.removed_sessions.len(),
                    report.removed_sessions.join("、")
                )
            }
            ConfirmAction::CleanAll => {
                let report = cleanup_all_forge_artifacts(&self.project.target_dir)?;
                if report.had_artifacts {
                    format!(
                        "已清空 `.codex-forge`，共删除 {} 个 session。",
                        report.removed_sessions.len()
                    )
                } else {
                    "当前仓库没有可清理的 `.codex-forge` 产物。".to_string()
                }
            }
            ConfirmAction::Quit => {
                self.should_quit = true;
                "正在退出 TUI。".to_string()
            }
        };

        self.history_detail = None;
        if !self.should_quit {
            self.refresh_project(false)?;
            self.navigate_to(Route::History);
        }
        self.push_notice(&message);
        Ok(())
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
            FormField::ContinueFeedback => {
                if self.form.continue_feedback.trim().is_empty() {
                    "—".to_string()
                } else {
                    truncate(&self.form.continue_feedback.replace('\n', " ⏎ "), 72)
                }
            }
            FormField::ReviewIssue => {
                if self.form.review_issue.trim().is_empty() {
                    "—".to_string()
                } else {
                    truncate(&self.form.review_issue.replace('\n', " ⏎ "), 72)
                }
            }
            FormField::ContinueMode => {
                continue_mode_user_label(self.form.continue_mode).to_string()
            }
            FormField::FromPlanSession => empty_to_dash(&self.form.from_plan_session_id),
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
            FormField::ContinueFeedback => self.form.continue_feedback.clone(),
            FormField::ReviewIssue => self
                .manual_review
                .as_ref()
                .and_then(|review| review.selected_file())
                .and_then(|item| item.issue_summary.clone())
                .unwrap_or_else(|| self.form.review_issue.clone()),
            FormField::FromPlanSession => self.form.from_plan_session_id.clone(),
            FormField::Workers => self.form.workers.clone(),
            FormField::MaxRetries => self.form.max_retries.clone(),
            FormField::Model => self.form.model.clone(),
            FormField::ResumeSession => self.form.resume_session_id.clone(),
            _ => return,
        };
        let history_entries = if field == FormField::Task {
            recent_task_history(&self.project)
        } else {
            Vec::new()
        };
        let history_index = if field == FormField::Task {
            history_entries.iter().position(|item| item == &current)
        } else {
            None
        };
        self.edit_state = Some(EditState {
            field,
            cursor: current.chars().count(),
            buffer: current,
            preferred_column: None,
            history_entries,
            history_index,
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
            FormField::ContinueFeedback => {
                self.form.continue_feedback = edit.buffer.trim().to_string()
            }
            FormField::ReviewIssue => {
                self.form.review_issue = edit.buffer.trim().to_string();
                if let Some(review) = &mut self.manual_review
                    && let Some(file) = review.selected_file_mut()
                {
                    file.issue_summary = if self.form.review_issue.trim().is_empty() {
                        None
                    } else {
                        Some(self.form.review_issue.clone())
                    };
                    review.state.selected_file = Some(file.path.clone());
                    persist_manual_review_state(
                        &self.project.target_dir,
                        &review.session_id,
                        review.state.clone(),
                    )?;
                }
            }
            FormField::FromPlanSession => {
                self.form.from_plan_session_id = edit.buffer.trim().to_string();
                if !self.form.from_plan_session_id.is_empty() {
                    self.form.resume_session_id.clear();
                }
            }
            FormField::Workers => self.form.workers = edit.buffer.trim().to_string(),
            FormField::MaxRetries => self.form.max_retries = edit.buffer.trim().to_string(),
            FormField::Model => self.form.model = edit.buffer.trim().to_string(),
            FormField::ResumeSession => {
                self.form.resume_session_id = edit.buffer.trim().to_string();
                if !self.form.resume_session_id.is_empty() {
                    self.form.from_plan_session_id.clear();
                }
            }
            _ => {}
        }
        if matches!(edit.field, FormField::TargetDir | FormField::ConfigPath) {
            self.refresh_project(true)?;
        }
        Ok(())
    }

    fn focus_action_after_save(&mut self, action: ShellAction) -> String {
        match action {
            ShellAction::Plan => {
                self.navigate_to(Route::Start);
                self.start_focus = StartFocus::Actions;
                self.start_action_index = StartAction::all()
                    .iter()
                    .position(|item| *item == StartAction::Plan)
                    .unwrap_or(0);
                "内容已保存。已准备好“先看方案”，按 Enter 手动开始。".to_string()
            }
            ShellAction::Run => {
                self.navigate_to(Route::Start);
                self.start_focus = StartFocus::Actions;
                self.start_action_index = StartAction::all()
                    .iter()
                    .position(|item| *item == StartAction::Run)
                    .unwrap_or(0);
                "内容已保存。已准备好“开始执行”，按 Enter 手动开始。".to_string()
            }
            ShellAction::ContinueSelected => {
                self.navigate_to(Route::History);
                self.history_focus = HistoryFocus::Actions;
                self.history_action_index =
                    available_history_actions(self.selected_session.as_ref())
                        .iter()
                        .position(|item| *item == HistoryAction::Continue)
                        .unwrap_or(0);
                "反馈已保存。已准备好“继续优化”，按 Enter 手动开始。".to_string()
            }
            ShellAction::ReviewFixSelected => {
                "内容已保存。现在可以在人工审查里发起单文件返修。".to_string()
            }
            ShellAction::Doctor | ShellAction::ReplaySelected => "内容已保存。".to_string(),
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

    fn use_plan_session_for_run(&mut self, session: &SessionManifest) {
        self.form.task = session.task.clone();
        self.form.from_plan_session_id = session.id.clone();
        self.form.resume_session_id.clear();
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
            if !form.from_plan_session_id.trim().is_empty() {
                args.push("--from-plan".to_string());
                args.push(form.from_plan_session_id.trim().to_string());
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
        ShellAction::ContinueSelected => {
            args.push("continue".to_string());
            if let Some(session) = selected_session {
                args.push("--session".to_string());
                args.push(session.id.clone());
            }
            args.push("--feedback".to_string());
            args.push(form.continue_feedback.clone());
            args.push("--mode".to_string());
            args.push(continue_mode_arg_value(form.continue_mode).to_string());
            args.push("--target-dir".to_string());
            args.push(target_dir.display().to_string());
            args.push("--ui".to_string());
            args.push("minimal".to_string());
        }
        ShellAction::ReviewFixSelected => {
            args.push("continue".to_string());
            args.push("--session".to_string());
            args.push(
                selected_session
                    .map(|session| session.id.clone())
                    .unwrap_or_else(|| "<review-session>".to_string()),
            );
            args.push("--feedback".to_string());
            args.push(form.review_issue.clone());
            args.push("--mode".to_string());
            args.push("run".to_string());
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
        ShellAction::ContinueSelected => {
            selected_session.map(|session| RuntimeViewState::new(&session.id, &session.task))
        }
        ShellAction::ReviewFixSelected => selected_session.map(|session| {
            RuntimeViewState::new(&session.id, &format!("人工审查返修 {}", session.task))
        }),
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
            ShellAction::ContinueSelected => {
                run_continue_embedded(&target_dir, &form, selected_session, tx.clone(), stop_rx)
                    .await
            }
            ShellAction::ReviewFixSelected => Ok((CommandState::Failed, None)),
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

fn spawn_review_fix_action(
    target_dir: PathBuf,
    parent_session: SessionManifest,
    target_file: String,
    issue_summary: String,
    tx: mpsc::UnboundedSender<RunnerEvent>,
    stop_rx: Option<watch::Receiver<bool>>,
) {
    tokio::spawn(async move {
        let outcome = run_review_fix_embedded(
            &target_dir,
            &parent_session,
            &target_file,
            &issue_summary,
            tx.clone(),
            stop_rx,
        )
        .await;

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

async fn run_continue_embedded(
    target_dir: &Path,
    form: &FormState,
    selected_session: Option<SessionManifest>,
    tx: mpsc::UnboundedSender<RunnerEvent>,
    stop_rx: Option<watch::Receiver<bool>>,
) -> Result<(CommandState, Option<SessionManifest>)> {
    let args = build_continue_args(target_dir, form, selected_session.as_ref())?;
    let (config, roles) = resolve_continue_config(args)?;
    if matches!(
        config.continuation.as_ref().map(|item| item.kind),
        Some(crate::model::ContinuationKind::PlanRefine)
    ) {
        let manifest = plan_session_embedded(config, roles, runtime_tx(&tx)).await?;
        Ok((CommandState::Succeeded, Some(manifest)))
    } else {
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
}

async fn run_review_fix_embedded(
    target_dir: &Path,
    parent_session: &SessionManifest,
    target_file: &str,
    issue_summary: &str,
    tx: mpsc::UnboundedSender<RunnerEvent>,
    stop_rx: Option<watch::Receiver<bool>>,
) -> Result<(CommandState, Option<SessionManifest>)> {
    let (config, roles) = build_review_fix_config(
        target_dir,
        parent_session,
        target_file,
        issue_summary,
        UiMode::Minimal,
    )?;
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
        from_plan: empty_as_none(&form.from_plan_session_id),
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

fn build_continue_args(
    target_dir: &Path,
    form: &FormState,
    selected_session: Option<&SessionManifest>,
) -> Result<ContinueArgs> {
    let session = selected_session.context("history continue 需要先选中一个 session")?;
    Ok(ContinueArgs {
        session: session.id.clone(),
        feedback: empty_as_none(&form.continue_feedback),
        mode: form.continue_mode,
        title: None,
        ui: UiModeArg::Minimal,
        target_dir: Some(target_dir.to_path_buf()),
    })
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
            let preview_manifest = serde_json::from_str::<SessionManifest>(&raw).ok()?;
            let manifest = load_session(target_dir, Some(&preview_manifest.id)).ok()?;
            let summary = manifest
                .final_summary
                .as_ref()
                .map(|item| item.overview.clone())
                .or_else(|| manifest.plan_todo.as_ref().map(|item| item.summary.clone()))
                .unwrap_or_else(|| "这次还没有摘要".to_string());
            Some(SessionSummary {
                id: manifest.id.clone(),
                created_at: format_beijing(manifest.created_at, "%m-%d %H:%M"),
                task: manifest.task.clone(),
                stage_label: manifest.status.label().to_string(),
                summary,
                mode_label: if manifest.is_plan_session() {
                    "方案".to_string()
                } else {
                    "执行".to_string()
                },
                continuable: manifest.continuable(),
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
        ShellAction::Plan => {
            if form.task.trim().is_empty() {
                "请先输入提示词，然后再生成方案和待办清单".to_string()
            } else {
                format!(
                    "先把“{}”拆成方案和待办清单，再决定怎么执行",
                    summarize_task(&form.task)
                )
            }
        }
        ShellAction::Run => {
            if form.task.trim().is_empty() {
                "请先输入提示词，然后再开始执行".to_string()
            } else if !form.resume_session_id.trim().is_empty() {
                format!(
                    "恢复运行 session `{}`，继续处理“{}”",
                    truncate(&form.resume_session_id, 18),
                    summarize_task(&form.task)
                )
            } else if !form.from_plan_session_id.trim().is_empty() {
                format!(
                    "基于 plan session `{}` 执行“{}”",
                    truncate(&form.from_plan_session_id, 18),
                    summarize_task(&form.task)
                )
            } else {
                format!(
                    "直接开始处理“{}”，默认使用 {}",
                    summarize_task(&form.task),
                    advanced_settings_summary(form)
                )
            }
        }
        ShellAction::ContinueSelected => {
            let action_title = if selected_session.is_some_and(|session| session.is_plan_session())
            {
                "继续改方案"
            } else {
                "继续优化"
            };
            if selected_session.is_none() {
                format!("请先在历史页选中一个已完成 session，再{action_title}")
            } else if form.continue_feedback.trim().is_empty() {
                format!("直接基于当前 session {action_title}；如果有额外反馈，也可以先补充。")
            } else {
                format!(
                    "基于 {} {}（{}）：{}",
                    selected_session
                        .map(|session| truncate(&session.id, 18))
                        .unwrap_or_else(|| "当前会话".to_string()),
                    action_title,
                    continue_mode_user_title(form.continue_mode),
                    truncate(&form.continue_feedback, 40)
                )
            }
        }
        ShellAction::ReviewFixSelected => selected_session
            .map(|session| format!("基于 `{}` 启动当前人工审查文件的返修子会话", session.id))
            .unwrap_or_else(|| "基于当前审查文件启动返修子会话".to_string()),
        ShellAction::ReplaySelected => format!(
            "回看 {} 的关键过程和结果",
            selected_session
                .map(|session| truncate(&session.id, 18))
                .unwrap_or_else(|| "最近一次会话".to_string())
        ),
    }
}

fn recent_task_history(project: &ProjectContext) -> Vec<String> {
    let mut seen = BTreeMap::<String, ()>::new();
    let mut items = Vec::new();
    for session in &project.sessions {
        let task = session.task.trim();
        if task.is_empty() || seen.contains_key(task) {
            continue;
        }
        seen.insert(task.to_string(), ());
        items.push(task.to_string());
    }
    items
}

fn recent_task_history_lines(edit: &EditState) -> Vec<Line<'static>> {
    edit.history_entries
        .iter()
        .take(5)
        .enumerate()
        .map(|(index, item)| {
            let prefix = if edit.history_index == Some(index) {
                ">"
            } else {
                "-"
            };
            Line::from(format!("{prefix} {}", truncate(item, 72)))
        })
        .collect()
}

fn available_history_actions(selected_session: Option<&SessionManifest>) -> Vec<HistoryAction> {
    let Some(session) = selected_session else {
        return vec![HistoryAction::CleanAll, HistoryAction::BackToStart];
    };

    let mut actions = Vec::new();
    if session.is_plan_session() {
        actions.push(HistoryAction::ExecutePlan);
    }
    if can_deliver_accepted_files(session) {
        actions.push(HistoryAction::DeliverAccepted);
    }
    if can_open_manual_review(session) {
        actions.push(HistoryAction::ManualReview);
    }
    actions.push(HistoryAction::Continue);
    actions.push(HistoryAction::EditFeedback);
    actions.push(HistoryAction::ContinueMode);
    actions.push(HistoryAction::Replay);
    actions.push(HistoryAction::Detail);
    if session.is_run_session() {
        actions.push(HistoryAction::ResetSelected);
    }
    actions.push(HistoryAction::CleanSelected);
    actions.push(HistoryAction::CleanAll);
    actions.push(HistoryAction::BackToStart);
    actions
}

fn run_source_user_hint(form: &FormState) -> String {
    if !form.resume_session_id.trim().is_empty() {
        format!(
            "当前会恢复运行 session `{}`，不会走新规划。",
            truncate(&form.resume_session_id, 18)
        )
    } else if !form.from_plan_session_id.trim().is_empty() {
        format!(
            "当前会基于 plan session `{}` 执行，不会静默改用其他方案。",
            truncate(&form.from_plan_session_id, 18)
        )
    } else {
        "当前会发起一次全新执行；如需承接方案，请显式指定 plan session。".to_string()
    }
}

fn action_supports_stop(
    action: ShellAction,
    form: &FormState,
    selected_session: Option<&SessionManifest>,
) -> bool {
    match action {
        ShellAction::Run | ShellAction::ReviewFixSelected | ShellAction::ReplaySelected => true,
        ShellAction::ContinueSelected => selected_session
            .map(|session| continue_mode_runs(form.continue_mode, session))
            .unwrap_or(false),
        ShellAction::Doctor | ShellAction::Plan => false,
    }
}

fn continue_mode_runs(mode: ContinueModeArg, session: &SessionManifest) -> bool {
    match mode {
        ContinueModeArg::Plan => false,
        ContinueModeArg::Run => true,
        ContinueModeArg::Auto => session.is_run_session(),
    }
}

fn initial_command_output(
    preview: &CommandPreview,
    display_target: &str,
    supports_stop: bool,
) -> Vec<String> {
    let mut lines = vec![
        format!("准备动作：{}", preview.summary),
        format!("目标仓库：{display_target}"),
        "系统工件会优先保留在 .codex-forge/；用户导出件会在完成收敛后写到目标仓库根目录。"
            .to_string(),
        format!("命令预览：{}", truncate(&preview.commandline, 120)),
        "内嵌执行已启动，等待实时事件…".to_string(),
    ];
    if supports_stop {
        lines.push("如需中止，请回执行页按 `s` 发起安全停止。".to_string());
    }
    lines
}

fn edit_mode_summary(field: FormField) -> &'static str {
    match field {
        FormField::Task => "支持多行输入，可直接保存或保存后定位到方案/执行动作。",
        FormField::ContinueFeedback => "支持多行输入，可直接保存或保存后定位到“继续优化”。",
        FormField::ReviewIssue => "支持多行输入，用来约束当前文件的返修目标。",
        _ => "单行编辑；改完可直接保存或退出。",
    }
}

fn edit_shortcuts_lines(field: FormField) -> Vec<Line<'static>> {
    match field {
        FormField::Task => vec![
            Line::from("保存：Ctrl+S"),
            Line::from("退出：Esc"),
            Line::from("换行：Enter"),
            Line::from("保存并定位：Ctrl+P 方案 / Ctrl+R 执行"),
            Line::from("历史提示词：Ctrl+J 下一条 / Ctrl+K 上一条"),
        ],
        FormField::ContinueFeedback => vec![
            Line::from("保存：Ctrl+S"),
            Line::from("退出：Esc"),
            Line::from("换行：Enter"),
            Line::from("保存并定位：Ctrl+R 继续优化"),
        ],
        FormField::ReviewIssue => vec![
            Line::from("保存：Ctrl+S"),
            Line::from("退出：Esc"),
            Line::from("换行：Enter"),
            Line::from("返修前建议先把问题写清楚"),
        ],
        _ => vec![
            Line::from("保存：Enter"),
            Line::from("退出：Esc"),
            Line::from("移动：方向键"),
            Line::from("删除：Backspace / Delete"),
        ],
    }
}

fn history_detail_shortcuts_lines() -> Vec<Line<'static>> {
    vec![
        Line::from("关闭：Esc / v"),
        Line::from("切页：Tab / ←→ / [ ]"),
        Line::from("滚动：↑↓ / j k"),
        Line::from("翻页：PgUp / PgDn / Home / End"),
        Line::from("左侧看用户摘要，右侧看技术细节"),
    ]
}

fn confirm_shortcuts_lines(action: &ConfirmAction) -> Vec<Line<'static>> {
    let action_line = match action {
        ConfirmAction::ResetSelected { .. } => "确认：Enter（回退自动提交并删除对应历史）",
        ConfirmAction::CleanSelected { .. } => "确认：Enter（删除当前会话及其后续迭代）",
        ConfirmAction::CleanAll => "确认：Enter（清空当前仓库下全部 .codex-forge 历史）",
        ConfirmAction::Quit => "确认：Enter（退出当前 TUI）",
    };
    vec![Line::from(action_line), Line::from("取消：Esc")]
}

fn command_elapsed_secs(command: &ActiveCommand) -> u64 {
    command
        .finished_at
        .unwrap_or_else(Instant::now)
        .duration_since(command.started_at)
        .as_secs()
}

fn build_doctor_failure_summary(report: &DoctorReport) -> String {
    let failed = report
        .checks
        .iter()
        .filter(|check| matches!(check.status, crate::model::CheckStatus::Failed))
        .map(|check| format!("{}：{}", check.name, truncate(&check.detail, 40)))
        .collect::<Vec<_>>();
    if failed.is_empty() {
        format!("doctor 未通过：{}", report.summary)
    } else {
        format!("doctor 未通过：{}", failed.join("；"))
    }
}

fn saved_field_notice(field: FormField) -> String {
    match field {
        FormField::Task => "内容已保存。现在请手动选择“先看方案”或“开始执行”。".to_string(),
        FormField::ContinueFeedback => "反馈已保存。现在请手动执行“继续优化”。".to_string(),
        FormField::ReviewIssue => "审查问题已保存。现在可以发起单文件返修。".to_string(),
        FormField::TargetDir | FormField::ConfigPath => {
            "内容已保存，项目上下文已刷新。".to_string()
        }
        _ => "内容已保存。".to_string(),
    }
}

fn preferred_run_subview(action: ShellAction) -> RunSubview {
    match action {
        ShellAction::Plan
        | ShellAction::Run
        | ShellAction::ContinueSelected
        | ShellAction::ReviewFixSelected
        | ShellAction::ReplaySelected => RunSubview::Timeline,
        ShellAction::Doctor => RunSubview::Dashboard,
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

fn existing_deliverables(session: &SessionManifest) -> Vec<PathBuf> {
    repo_export_candidates(session)
        .into_iter()
        .filter(|path| path.exists())
        .collect()
}

fn repo_export_candidates(session: &SessionManifest) -> Vec<PathBuf> {
    let files = if let Some(result) = session.manual_delivery_result.as_ref() {
        result.delivered_files.clone()
    } else {
        session
            .final_summary
            .as_ref()
            .map(|summary| summary.accepted_files.clone())
            .unwrap_or_default()
    };
    files
        .iter()
        .map(|path| repo_export_path(session, path))
        .collect::<Vec<_>>()
}

fn repo_export_path(session: &SessionManifest, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        session.repo_root().join(path)
    }
}

fn repo_export_label(session: &SessionManifest, path: &Path) -> String {
    path.strip_prefix(session.repo_root())
        .map(|item| item.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn append_repo_export_sections(sections: &mut Vec<String>, session: &SessionManifest) {
    let exports = repo_export_candidates(session);
    if exports.is_empty() {
        sections.push(format!(
            "===== Repo Exports =====\n\n当前会话尚未交付到目标目录。\n\n交付状态：{} / {}",
            delivery_status_label(session),
            delivery_status_detail(session)
        ));
        return;
    }

    for path in exports {
        append_repo_export_section(
            sections,
            &format!("Repo Export: {}", repo_export_label(session, &path)),
            path,
        );
    }
}

fn process_artifact_paths(session: &SessionManifest) -> Vec<PathBuf> {
    let mut paths = vec![
        session.deliverable_plan_path(),
        session.deliverable_summary_path(),
        session.deliverable_changes_path(),
        session.deliverable_verify_path(),
    ];
    paths.extend([
        session.summary_markdown_path.clone(),
        session.summary_json_path.clone(),
        session.apply_result_path.clone(),
        session.verification_report_path.clone(),
        session.change_trust_report_path.clone(),
    ]);
    if let Some(path) = &session.artifact_manifest.manual_delivery_result_path {
        paths.push(path.clone());
    }
    if let Some(path) = &session.artifact_manifest.manual_review_state_path {
        paths.push(path.clone());
    }
    paths.into_iter().filter(|path| path.exists()).collect()
}

fn existing_system_artifacts(session: &SessionManifest) -> Vec<PathBuf> {
    process_artifact_paths(session)
}

fn can_deliver_accepted_files(session: &SessionManifest) -> bool {
    session.is_run_session()
        && !session.delivered_to_target()
        && session
            .apply_result
            .as_ref()
            .is_some_and(|result| !result.accepted_files.is_empty())
}

fn can_open_manual_review(session: &SessionManifest) -> bool {
    session.is_run_session()
        && (session
            .apply_result
            .as_ref()
            .is_some_and(|result| !result.manual_review_files.is_empty())
            || session
                .manual_review_state
                .as_ref()
                .is_some_and(|state| !state.files.is_empty()))
}

fn load_apply_plan_for_session(session: &SessionManifest) -> Result<ApplyPlan> {
    let raw = fs::read_to_string(&session.apply_plan_path).with_context(|| {
        format!(
            "读取 apply plan 失败：{}",
            session.apply_plan_path.display()
        )
    })?;
    serde_json::from_str(&raw).with_context(|| {
        format!(
            "解析 apply plan 失败：{}",
            session.apply_plan_path.display()
        )
    })
}

fn persist_manual_delivery_result(
    target_dir: &Path,
    session_id: &str,
    result: ManualDeliveryResult,
) -> Result<()> {
    let mut session = load_session(target_dir, Some(session_id))?;
    set_manual_delivery_result_for_loaded_session(&mut session, result)
}

fn persist_manual_review_state(
    target_dir: &Path,
    session_id: &str,
    state: ManualReviewState,
) -> Result<()> {
    let mut session = load_session(target_dir, Some(session_id))?;
    set_manual_review_state_for_loaded_session(&mut session, state)
}

fn load_or_initialize_manual_review_state(
    target_dir: &Path,
    session: &SessionManifest,
) -> Result<ManualReviewState> {
    let mut state = session
        .manual_review_state
        .clone()
        .unwrap_or_else(|| build_initial_manual_review_state(session));

    let current_files = session
        .apply_result
        .as_ref()
        .map(|result| result.manual_review_files.clone())
        .unwrap_or_default();
    for file in current_files {
        if !state.files.iter().any(|item| item.path == file) {
            state
                .files
                .push(build_manual_review_file_record(session, &file));
        }
    }
    if state.selected_file.is_none() {
        state.selected_file = state.files.first().map(|item| item.path.clone());
    }
    persist_manual_review_state(target_dir, &session.id, state.clone())?;
    Ok(state)
}

fn build_initial_manual_review_state(session: &SessionManifest) -> ManualReviewState {
    let files = session
        .apply_result
        .as_ref()
        .map(|result| result.manual_review_files.clone())
        .unwrap_or_default()
        .into_iter()
        .map(|file| build_manual_review_file_record(session, &file))
        .collect::<Vec<_>>();
    ManualReviewState {
        source_session_id: session.id.clone(),
        selected_file: files.first().map(|item| item.path.clone()),
        files,
    }
}

fn build_manual_review_file_record(
    session: &SessionManifest,
    file: &str,
) -> ManualReviewFileRecord {
    let source_workers = session
        .worker_results
        .iter()
        .filter(|result| result.changed_files.iter().any(|item| item == file))
        .map(|result| result.agent_id.clone())
        .collect::<Vec<_>>();
    ManualReviewFileRecord {
        path: file.to_string(),
        status: ManualReviewFileStatus::Pending,
        source_workers,
        issue_summary: None,
        fix_session_ids: Vec::new(),
        latest_fix_session_id: None,
    }
}

fn delivery_status_label(session: &SessionManifest) -> &'static str {
    if session.delivered_to_target() {
        "已交付"
    } else {
        "未交付"
    }
}

fn delivery_status_detail(session: &SessionManifest) -> String {
    if let Some(result) = &session.manual_delivery_result {
        if result.success {
            return format!(
                "已手动交付 {} 个 accepted_files 到目标目录。",
                result.delivered_files.len()
            );
        }
        return format!(
            "上次手动交付失败：{}",
            result.error.as_deref().unwrap_or("未知错误")
        );
    }

    if let Some(apply_result) = &session.apply_result {
        if apply_result.synced_to_target && matches!(apply_result.status, ApplyStatus::Applied) {
            return "auto-safe 已自动同步到目标目录。".to_string();
        }
        let mut reasons = Vec::new();
        if let Some(gate) = apply_result.review_gate {
            reasons.push(format!("review gate：{}", gate.label()));
        }
        reasons.push(format!("apply：{}", apply_result.status.label()));
        if let Some(bundle_dir) = &apply_result.bundle_dir {
            reasons.push(format!("bundle：{}", bundle_dir.display()));
        }
        return reasons.join(" / ");
    }

    "当前没有可用交付记录。".to_string()
}

fn build_manual_review_detail_text(
    target_dir: &Path,
    state: &ManualReviewState,
    file_index: usize,
    diff_view: ManualReviewDiffView,
) -> String {
    let Some(file) = state.files.get(file_index) else {
        return "当前没有可审查文件。".to_string();
    };
    let source_session = load_session(target_dir, Some(&state.source_session_id)).ok();
    let latest_fix_session = file
        .latest_fix_session_id
        .as_deref()
        .and_then(|session_id| load_session(target_dir, Some(session_id)).ok());
    let issue_summary = file
        .issue_summary
        .clone()
        .unwrap_or_else(|| "尚未记录审查问题。".to_string());

    let mut sections = vec![format!(
        "文件：{}\n状态：{}\n来源 worker：{}\n审查问题：{}\n返修 session：{}",
        file.path,
        file.status.label(),
        if file.source_workers.is_empty() {
            "无".to_string()
        } else {
            file.source_workers.join("、")
        },
        issue_summary,
        file.latest_fix_session_id
            .clone()
            .unwrap_or_else(|| "无".to_string())
    )];

    match diff_view {
        ManualReviewDiffView::Source => {
            sections.push("===== 原始候选 Diff =====".to_string());
            sections.push(
                source_session
                    .as_ref()
                    .map(|session| render_file_diff_for_session(session, &file.path))
                    .unwrap_or_else(|| "无法加载来源 session。".to_string()),
            );
        }
        ManualReviewDiffView::LatestFix => {
            sections.push("===== 返修后 Diff =====".to_string());
            sections.push(
                latest_fix_session
                    .as_ref()
                    .map(|session| render_file_diff_for_session(session, &file.path))
                    .unwrap_or_else(|| "当前还没有返修结果。".to_string()),
            );
            if let Some(session) = latest_fix_session.as_ref() {
                sections.push("===== 返修会话摘要 =====".to_string());
                sections.push(
                    session
                        .final_summary
                        .as_ref()
                        .map(|summary| {
                            format!(
                                "{}\n结果：{}\nApply：{}",
                                summary.overview,
                                summary.result_status.label(),
                                summary.apply_status.label()
                            )
                        })
                        .unwrap_or_else(|| "返修会话还没有最终摘要。".to_string()),
                );
            }
        }
        ManualReviewDiffView::Compare => {
            sections.push("===== 修复前 =====".to_string());
            sections.push(
                source_session
                    .as_ref()
                    .map(|session| render_file_diff_for_session(session, &file.path))
                    .unwrap_or_else(|| "无法加载来源 session。".to_string()),
            );
            sections.push("===== 修复后 =====".to_string());
            sections.push(
                latest_fix_session
                    .as_ref()
                    .map(|session| render_file_diff_for_session(session, &file.path))
                    .unwrap_or_else(|| "当前还没有返修结果。".to_string()),
            );
        }
    }

    sections.join("\n\n")
}

fn render_file_diff_for_session(session: &SessionManifest, file: &str) -> String {
    let sections = session
        .worker_results
        .iter()
        .filter(|result| result.changed_files.iter().any(|item| item == file))
        .filter_map(|result| {
            let diff_path = result.diff_path.as_ref()?;
            let raw = fs::read_to_string(diff_path).ok()?;
            let diff = extract_file_diff_from_patch(&raw, file)?;
            Some(format!(
                "### {} / {}\n{}",
                result.agent_id, result.role, diff
            ))
        })
        .collect::<Vec<_>>();

    if sections.is_empty() {
        "当前会话没有该文件的可展示 diff。".to_string()
    } else {
        sections.join("\n\n")
    }
}

fn extract_file_diff_from_patch(patch: &str, file: &str) -> Option<String> {
    let markers = [
        format!("diff --git a/{file} b/{file}"),
        format!("diff --git \"a/{file}\" \"b/{file}\""),
    ];
    let start = patch
        .lines()
        .enumerate()
        .find(|(_, line)| markers.iter().any(|marker| line == marker))
        .map(|(index, _)| index)?;
    let lines = patch.lines().collect::<Vec<_>>();
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| line.starts_with("diff --git "))
        .map(|(index, _)| index)
        .unwrap_or(lines.len());
    Some(lines[start..end].join("\n"))
}

fn collect_changed_files(manifest: &SessionManifest) -> Vec<String> {
    let mut files = manifest
        .worker_results
        .iter()
        .flat_map(|result| result.changed_files.iter().cloned())
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    files
}

fn record_review_fix_completion(
    target_dir: &Path,
    parent_session_id: &str,
    target_file: &str,
    child_manifest: &SessionManifest,
) -> Result<()> {
    let mut session = load_session(target_dir, Some(parent_session_id))?;
    let mut state = load_or_initialize_manual_review_state(target_dir, &session)?;
    let changed_files = collect_changed_files(child_manifest);
    let out_of_scope = changed_files
        .iter()
        .filter(|file| file.as_str() != target_file)
        .cloned()
        .collect::<Vec<_>>();
    let touched_target = changed_files.iter().any(|file| file == target_file);

    let record = state
        .files
        .iter_mut()
        .find(|item| item.path == target_file)
        .with_context(|| format!("人工审查状态里缺少文件 `{target_file}`"))?;
    if !record
        .fix_session_ids
        .iter()
        .any(|item| item == &child_manifest.id)
    {
        record.fix_session_ids.push(child_manifest.id.clone());
    }
    record.latest_fix_session_id = Some(child_manifest.id.clone());
    record.status = if !out_of_scope.is_empty() {
        record.issue_summary = Some(format!("返修越界，额外修改了：{}", out_of_scope.join("、")));
        ManualReviewFileStatus::NeedsFix
    } else if touched_target {
        ManualReviewFileStatus::FixedPendingReview
    } else {
        record.issue_summary = Some("返修会话没有生成当前文件的新 diff。".to_string());
        ManualReviewFileStatus::NeedsFix
    };
    state.selected_file = Some(target_file.to_string());
    set_manual_review_state_for_loaded_session(&mut session, state)
}

async fn deliver_manual_review_approved_files(
    target_dir: &Path,
    review_session_id: &str,
    state: &ManualReviewState,
    destination: &Path,
) -> Result<Vec<String>> {
    let review_session = load_session(target_dir, Some(review_session_id))?;
    let mut by_session = BTreeMap::<String, Vec<String>>::new();
    for record in &state.files {
        if record.status != ManualReviewFileStatus::Approved {
            continue;
        }
        let source_session_id = record
            .latest_fix_session_id
            .clone()
            .unwrap_or_else(|| review_session.id.clone());
        by_session
            .entry(source_session_id)
            .or_default()
            .push(record.path.clone());
    }
    if by_session.is_empty() {
        anyhow::bail!("当前没有已人工通过的文件");
    }

    let clean = crate::worktree::git_is_clean(destination).await?;
    if !clean {
        anyhow::bail!("目标工作区存在未提交改动，拒绝执行人工审查交付");
    }

    let mut delivered = Vec::new();
    for (session_id, files) in by_session {
        let source_session = load_session(target_dir, Some(&session_id))?;
        let plan = load_apply_plan_for_session(&source_session)?;
        let applied = deliver_selected_files_from_plan(&plan, destination, &files).await?;
        delivered.extend(applied);
    }
    delivered.sort();
    delivered.dedup();
    Ok(delivered)
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

fn continue_mode_user_title(mode: ContinueModeArg) -> &'static str {
    match mode {
        ContinueModeArg::Auto => "自动判断",
        ContinueModeArg::Plan => "只重做方案",
        ContinueModeArg::Run => "直接执行",
    }
}

fn continue_mode_user_label(mode: ContinueModeArg) -> &'static str {
    match mode {
        ContinueModeArg::Auto => "自动判断（auto）",
        ContinueModeArg::Plan => "只重做方案（plan）",
        ContinueModeArg::Run => "直接执行（run）",
    }
}

fn continue_mode_arg_value(mode: ContinueModeArg) -> &'static str {
    match mode {
        ContinueModeArg::Auto => "auto",
        ContinueModeArg::Plan => "plan",
        ContinueModeArg::Run => "run",
    }
}

fn advanced_fields() -> &'static [FormField] {
    &[
        FormField::TargetDir,
        FormField::ConfigPath,
        FormField::ContinueMode,
        FormField::FromPlanSession,
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
    if !form.from_plan_session_id.trim().is_empty() {
        items.push(format!(
            "执行方案 {}",
            truncate(&form.from_plan_session_id, 18)
        ));
    }
    if !form.resume_session_id.trim().is_empty() {
        items.push(format!("恢复 {}", truncate(&form.resume_session_id, 18)));
    }
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
            | FormField::ContinueFeedback
            | FormField::FromPlanSession
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

fn cycle_continue_mode(current: ContinueModeArg, forward: bool) -> ContinueModeArg {
    let modes = [
        ContinueModeArg::Auto,
        ContinueModeArg::Plan,
        ContinueModeArg::Run,
    ];
    let index = modes.iter().position(|item| *item == current).unwrap_or(0);
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
                "编辑中：Ctrl+S 保存，Esc 取消。"
            } else {
                "编辑中：Enter 换行，Ctrl+S 保存，Ctrl+P / Ctrl+R 保存并定位动作，Esc 取消。"
            }
        } else if edit.field == FormField::ContinueFeedback && width >= 72 {
            "编辑中：Enter 换行，Ctrl+S 保存，Ctrl+R 保存并定位“继续优化”，Esc 取消。"
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
            "开始：Enter 写任务/开字段，←→ 切区，Tab 进导航。"
        } else {
            "开始：Enter 写任务或打开字段编辑，←→ 切区，↑↓ 选动作，Enter 执行，Tab 进导航，Esc 收起低频设置。"
        })],
        Route::Run => vec![Line::from(format!(
            "{}：{}",
            if compact {
                "执行：←→ 切视图，↑↓ 看下一步，Tab 进导航"
            } else {
                "执行：←→ 切视图，↑↓ 看下一步，Enter 执行，Esc 返回，s 停止；历史详情请去历史页打开，Tab 进导航"
            },
            run_subview.label()
        ))],
        Route::History => vec![Line::from(if compact {
            "历史：←→ 切列表/下一步，↑↓ 选择，Enter 查看/执行。"
        } else {
            "历史：←→ 切列表/下一步，↑↓ 选择，Enter 查看详情/执行动作，u 人工审查，y 交付已接收，e 补反馈，v 详情，z 重置，x 删除，Esc 返回，Tab 进导航。"
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
    let _ = width;
    Route::all().iter().map(|route| route.label()).collect()
}

fn run_subview_titles(width: u16) -> Vec<&'static str> {
    if width < 68 {
        vec!["总览", "执行", "交付"]
    } else {
        RunSubview::all().iter().map(|item| item.label()).collect()
    }
}

fn history_detail_tab_titles(width: u16) -> Vec<&'static str> {
    let _ = width;
    HistoryDetailTab::all()
        .iter()
        .map(|item| item.label())
        .collect()
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

fn cycle_edit_history(edit: &mut EditState, forward: bool) {
    if edit.history_entries.is_empty() {
        return;
    }
    let current = edit.history_index.unwrap_or_else(|| {
        if forward {
            edit.history_entries.len().saturating_sub(1)
        } else {
            0
        }
    });
    let next = cycle_index(current, edit.history_entries.len(), forward);
    edit.history_index = Some(next);
    edit.buffer = edit.history_entries[next].clone();
    edit.cursor = edit.buffer.chars().count();
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

fn build_history_detail_summary(session: &SessionManifest, tab: HistoryDetailTab) -> String {
    match tab {
        HistoryDetailTab::Overview => {
            let summary = session
                .final_summary
                .as_ref()
                .map(|item| item.overview.clone())
                .or_else(|| session.plan_todo.as_ref().map(|item| item.summary.clone()))
                .unwrap_or_else(|| "这次还没有可直接展示的摘要。".to_string());
            format!(
                "任务：{}\n\n状态：{}\n类型：{}\n会话：{}\n创建时间：{}\n目标仓库：{}\n目标目录交付：{} / {}\n\n摘要：{}\n\n交付物目录：{}\n系统记录目录：{}\n",
                session.task,
                session.status.label(),
                session.session_kind.label(),
                session.id,
                format_beijing(session.created_at, "%Y-%m-%d %H:%M:%S"),
                session.repo_root().display(),
                delivery_status_label(session),
                delivery_status_detail(session),
                summary,
                session.repo_root().display(),
                session.session_dir.display(),
            )
        }
        HistoryDetailTab::Plan => {
            if let Some(plan) = &session.plan_todo {
                let todo_titles = plan
                    .todos
                    .iter()
                    .map(|item| format!("- {} {}", item.id, item.title))
                    .collect::<Vec<_>>()
                    .join("\n");
                let risks = if plan.risks.is_empty() {
                    "- 无".to_string()
                } else {
                    plan.risks
                        .iter()
                        .map(|item| format!("- {item}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                format!(
                    "方案摘要：{}\n\n推进策略：{}\n\n待办数量：{}\n{}\n\n主要风险：\n{}\n\n计划过程文件：{}\n",
                    plan.summary,
                    plan.approach,
                    plan.todos.len(),
                    todo_titles,
                    risks,
                    session.deliverable_plan_path().display(),
                )
            } else {
                "当前会话没有生成方案。".to_string()
            }
        }
        HistoryDetailTab::Runtime => {
            let timeline = if session.timeline_events.is_empty() {
                "还没有过程记录。".to_string()
            } else {
                session
                    .timeline_events
                    .iter()
                    .rev()
                    .take(8)
                    .rev()
                    .map(|item| {
                        format!(
                            "- {} / {} / {}",
                            format_beijing(item.ts, "%H:%M:%S"),
                            item.title,
                            item.detail
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            format!(
                "过程概览\n\n最近关键节点：\n{}\n\n当前状态：{}\nWorker 数量：{}\nTodo 数量：{}\n",
                timeline,
                session.status.label(),
                session.worker_results.len(),
                session.todo_states.len(),
            )
        }
        HistoryDetailTab::Artifacts => {
            let system_artifacts = existing_system_artifacts(session)
                .into_iter()
                .map(|path| format!("- 已生成 / {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n");
            let repo_exports = session
                .final_summary
                .as_ref()
                .map(|_| repo_export_candidates(session))
                .unwrap_or_default()
                .into_iter()
                .map(|path| {
                    let status = if path.exists() {
                        "已导出"
                    } else {
                        "未导出"
                    };
                    format!("- {} / {}", status, repo_export_label(session, &path))
                })
                .collect::<Vec<_>>()
                .join("\n");
            let final_summary = session.final_summary.as_ref();
            format!(
                "交付概览\n\n目标目录状态：{} / {}\n\n系统工件：\n{}\n\n用户导出件：\n{}\n\n已接收文件：{}\n人工复核：{}\n已验证能力：{}\n开放风险：{}\n",
                delivery_status_label(session),
                delivery_status_detail(session),
                if system_artifacts.is_empty() {
                    "- 尚未生成系统工件".to_string()
                } else {
                    system_artifacts
                },
                if repo_exports.is_empty() {
                    "- 当前会话尚未交付到目标目录".to_string()
                } else {
                    repo_exports
                },
                final_summary
                    .map(|item| item.accepted_files.len())
                    .unwrap_or(0),
                final_summary
                    .map(|item| item.manual_review_files.len())
                    .unwrap_or(0),
                final_summary
                    .map(|item| item.verified_capabilities.len())
                    .unwrap_or(0),
                final_summary.map(|item| item.open_risks.len()).unwrap_or(0),
            )
        }
        HistoryDetailTab::Technical => format!(
            "这里保留原始技术资料，方便排障和复盘。\n\nsession 目录：{}\ntimeline：{}\nartifact manifest：{}\nworker 数量：{}\n",
            session.session_dir.display(),
            session.timeline_path.display(),
            session.artifact_manifest_path.display(),
            session.worker_results.len(),
        ),
    }
}

fn build_history_detail_body(session: &SessionManifest, tab: HistoryDetailTab) -> String {
    let mut sections = Vec::<String>::new();
    match tab {
        HistoryDetailTab::Overview => {
            sections.push(format!(
                "会话：{}\n任务：{}\n状态：{}\n版本：V{}\n根会话：{}\n来源会话：{}\n创建时间：{}\n目标仓库：{}\n协作模板：{}\n任务强度：{}\n结果落地：{}\n目标目录交付：{} / {}\n可继续反馈：{}\nSession 目录：{}",
                session.id,
                session.task,
                session.status.label(),
                session.iteration_index_value(),
                session.root_session_id_ref(),
                session.parent_session_id.as_deref().unwrap_or("无"),
                format_beijing(session.created_at, "%Y-%m-%d %H:%M:%S"),
                session.repo_root().display(),
                session.role_set,
                thinking_mode_user_title(session.thinking_mode),
                apply_mode_user_label(session.apply_mode),
                delivery_status_label(session),
                delivery_status_detail(session),
                if session.continuable() { "是" } else { "否" },
                session.session_dir.display()
            ));
            append_serialized_section(&mut sections, "Repo Snapshot", &session.repo_snapshot);
            append_serialized_section(&mut sections, "Lineage", &session.lineage);
            append_serialized_section(&mut sections, "Feedback History", &session.feedback_history);
            append_serialized_section(&mut sections, "Final Summary", &session.final_summary);
            append_serialized_section(&mut sections, "Doctor Report", &session.doctor_report);
            append_serialized_section(&mut sections, "Artifact Index", &session.artifact_index);
        }
        HistoryDetailTab::Plan => {
            append_file_section_if_exists(
                &mut sections,
                "Plan Todo Markdown",
                session.session_dir.join("commander").join("plan-todo.md"),
            );
            append_file_section_if_exists(
                &mut sections,
                "Process Plan Markdown",
                session.deliverable_plan_path(),
            );
            append_optional_file_section(
                &mut sections,
                "Plan Todo JSON",
                session.artifact_manifest.plan_todo_path.clone(),
            );
            append_serialized_section(&mut sections, "Plan Todo (Manifest)", &session.plan_todo);
            append_file_section_if_exists(
                &mut sections,
                "Execution Graph JSON",
                &session.graph_path,
            );
            append_serialized_section(
                &mut sections,
                "Execution Graph (Manifest)",
                &session.execution_graph,
            );
            append_file_section_if_exists(
                &mut sections,
                "Execution Contract JSON",
                &session.execution_contract_path,
            );
            append_serialized_section(
                &mut sections,
                "Execution Contract (Manifest)",
                &session.execution_contract,
            );
            append_optional_file_section(
                &mut sections,
                "Todo State JSON",
                session.artifact_manifest.todo_state_path.clone(),
            );
            append_serialized_section(
                &mut sections,
                "Todo States (Manifest)",
                &session.todo_states,
            );
        }
        HistoryDetailTab::Runtime => {
            append_file_section_if_exists(&mut sections, "Timeline JSONL", &session.timeline_path);
            append_serialized_section(
                &mut sections,
                "Timeline Summary (Manifest)",
                &session.timeline_events,
            );
            append_serialized_section(&mut sections, "Demo Summary", &session.demo_summary);
            append_serialized_section(&mut sections, "Doctor Report", &session.doctor_report);
        }
        HistoryDetailTab::Artifacts => {
            append_file_section_if_exists(
                &mut sections,
                "Summary Markdown",
                &session.summary_markdown_path,
            );
            append_file_section_if_exists(
                &mut sections,
                "Summary JSON",
                &session.summary_json_path,
            );
            append_optional_file_section(
                &mut sections,
                "Apply Plan JSON",
                session.artifact_manifest.apply_plan_path.clone(),
            );
            append_file_section_if_exists(
                &mut sections,
                "Apply Result JSON",
                &session.apply_result_path,
            );
            append_serialized_section(
                &mut sections,
                "Apply Result (Manifest)",
                &session.apply_result,
            );
            append_file_section_if_exists(
                &mut sections,
                "Verification Report JSON",
                &session.verification_report_path,
            );
            append_serialized_section(
                &mut sections,
                "Verification Report (Manifest)",
                &session.verification_report,
            );
            append_file_section_if_exists(
                &mut sections,
                "Change Trust Report JSON",
                &session.change_trust_report_path,
            );
            append_serialized_section(
                &mut sections,
                "Change Trust Report (Manifest)",
                &session.change_trust_report,
            );
            append_optional_file_section(
                &mut sections,
                "Manual Delivery Result JSON",
                session
                    .artifact_manifest
                    .manual_delivery_result_path
                    .clone(),
            );
            append_serialized_section(
                &mut sections,
                "Manual Delivery Result (Manifest)",
                &session.manual_delivery_result,
            );
            append_optional_file_section(
                &mut sections,
                "Manual Review State JSON",
                session.artifact_manifest.manual_review_state_path.clone(),
            );
            append_serialized_section(
                &mut sections,
                "Manual Review State (Manifest)",
                &session.manual_review_state,
            );
            append_file_section_if_exists(
                &mut sections,
                "Process Summary Markdown",
                session.deliverable_summary_path(),
            );
            append_file_section_if_exists(
                &mut sections,
                "Process Changes Markdown",
                session.deliverable_changes_path(),
            );
            append_file_section_if_exists(
                &mut sections,
                "Process Verify Markdown",
                session.deliverable_verify_path(),
            );
            append_repo_export_sections(&mut sections, session);
        }
        HistoryDetailTab::Technical => {
            if session.worker_results.is_empty() {
                sections.push("当前 session 没有 worker 运行输出。".to_string());
            }
            for worker in &session.worker_results {
                sections.push(format!(
                    "Worker：{}\n角色：{}\n标题：{}\n状态：{}\n尝试次数：{}\n退出码：{}\n改动文件：{}\n错误：{}",
                    worker.agent_id,
                    worker.role,
                    worker.task_title,
                    worker.status.label(),
                    worker.attempts,
                    worker
                        .exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "无".to_string()),
                    if worker.changed_files.is_empty() {
                        "无".to_string()
                    } else {
                        worker.changed_files.join("；")
                    },
                    worker.error.clone().unwrap_or_else(|| "无".to_string())
                ));
                append_serialized_section(
                    &mut sections,
                    &format!("Worker {} Result JSON", worker.agent_id),
                    worker,
                );
                append_optional_file_section(
                    &mut sections,
                    &format!("Worker {} Diff", worker.agent_id),
                    worker.diff_path.clone(),
                );
                append_optional_file_section(
                    &mut sections,
                    &format!("Worker {} Git Status", worker.agent_id),
                    worker.git_status_path.clone(),
                );
                append_optional_file_section(
                    &mut sections,
                    &format!("Worker {} Handoff", worker.agent_id),
                    worker.handoff_path.clone(),
                );
                append_file_section_if_exists(
                    &mut sections,
                    &format!("Worker {} Final Output", worker.agent_id),
                    &worker.final_output_path,
                );
                append_file_section_if_exists(
                    &mut sections,
                    &format!("Worker {} Stdout", worker.agent_id),
                    &worker.stdout_path,
                );
                append_file_section_if_exists(
                    &mut sections,
                    &format!("Worker {} Stderr", worker.agent_id),
                    &worker.stderr_path,
                );
            }
            append_serialized_section(
                &mut sections,
                "Artifact Manifest",
                &session.artifact_manifest,
            );
            append_file_section_if_exists(
                &mut sections,
                "Artifact Manifest JSON",
                &session.artifact_manifest_path,
            );
            append_optional_file_section(
                &mut sections,
                "Feedback Markdown",
                session.artifact_manifest.feedback_markdown_path.clone(),
            );
            append_optional_file_section(
                &mut sections,
                "Feedback JSON",
                session.artifact_manifest.feedback_json_path.clone(),
            );
            append_optional_file_section(
                &mut sections,
                "Iteration Summary Markdown",
                session.artifact_manifest.iteration_summary_path.clone(),
            );
            append_optional_file_section(
                &mut sections,
                "Lineage JSON",
                session.artifact_manifest.lineage_path.clone(),
            );
            append_optional_file_section(
                &mut sections,
                "Latest Pointer Markdown",
                session.artifact_manifest.latest_pointer_path.clone(),
            );
        }
    }

    sections.join("\n\n")
}

fn append_serialized_section<T>(sections: &mut Vec<String>, title: &str, value: &T)
where
    T: Serialize,
{
    let body = serde_json::to_string_pretty(value)
        .unwrap_or_else(|error| format!("序列化失败：{error:#}"));
    sections.push(format!("===== {title} =====\n\n{body}"));
}

fn append_optional_file_section(sections: &mut Vec<String>, title: &str, path: Option<PathBuf>) {
    match path {
        Some(path) => append_file_section_if_exists(sections, title, path),
        None => sections.push(format!("===== {title} =====\n\n未生成该文件。")),
    }
}

fn append_repo_export_section<P>(sections: &mut Vec<String>, title: &str, path: P)
where
    P: AsRef<Path>,
{
    append_file_section_with_missing_message(sections, title, path, "当前会话未导出该用户文件。");
}

fn append_file_section_if_exists<P>(sections: &mut Vec<String>, title: &str, path: P)
where
    P: AsRef<Path>,
{
    append_file_section_with_missing_message(sections, title, path, "文件不存在。");
}

fn append_file_section_with_missing_message<P>(
    sections: &mut Vec<String>,
    title: &str,
    path: P,
    missing_message: &str,
) where
    P: AsRef<Path>,
{
    let path = path.as_ref();
    let body = if path.exists() {
        read_text_lossy(path)
    } else {
        missing_message.to_string()
    };
    sections.push(format!(
        "===== {title} =====\n路径：{}\n\n{}",
        path.display(),
        body
    ));
}

fn read_text_lossy(path: &Path) -> String {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        Err(error) => format!("读取失败：{error:#}"),
    }
}

fn page_count_for_text(text: &str, lines_per_page: usize) -> usize {
    let total_lines = text.lines().count().max(1);
    total_lines.div_ceil(lines_per_page.max(1))
}

fn page_text(text: &str, page: usize, lines_per_page: usize) -> String {
    let lines_per_page = lines_per_page.max(1);
    let start = page.saturating_mul(lines_per_page);
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return String::new();
    }
    let start = start.min(lines.len().saturating_sub(1));
    let end = (start + lines_per_page).min(lines.len());
    lines[start..end].join("\n")
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveCommand, AppShell, CommandState, FormField, FormState, HistoryAction, ProjectContext,
        Route, RunSubview, SessionSummary, ShellAction, action_supports_stop,
        build_command_preview, build_continue_args, contextual_help_lines, history_back_route,
        next_history_return_route, next_run_return_route, page_count_for_text, page_text,
        preferred_run_subview, prepare_runtime_state, push_command_output, route_titles,
        run_back_route, run_subview_titles, split_main_sections, wrapped_cursor_row_col,
    };
    use crate::cli::ContinueModeArg;
    use crate::model::{
        ApplyDecision, ApplyMode, ApplyResult, ApplyStatus, ArtifactManifest, BaselineArtifacts,
        DoctorCheck, DoctorReadiness, DoctorReport, FinalSummary, RepoSnapshot, ResultStatus,
        RuntimeEvent, ScopeDrift, SessionKind, SessionManifest, SessionPreset, SessionStatus,
        ThinkingMode, TimelineEventSummary, TrustLevel, UiMode,
    };
    use chrono::Utc;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Rect};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Instant;
    use tempfile::TempDir;
    use tokio::sync::{mpsc, watch};

    fn sample_final_summary(overview: &str) -> FinalSummary {
        FinalSummary {
            overview: overview.to_string(),
            result_status: ResultStatus::Completed,
            review_gate: Some(ApplyDecision::AllowFull),
            apply_status: ApplyStatus::Applied,
            trust_level: TrustLevel::High,
            accepted_files: vec!["src/app_shell.rs".to_string()],
            manual_review_files: vec!["tests/tui.rs".to_string()],
            rejected_files: Vec::new(),
            verified_capabilities: vec!["cargo test".to_string()],
            blocked_verifications: Vec::new(),
            open_risks: vec!["补一条真实终端冒烟".to_string()],
            recommended_next_action: vec!["继续压测".to_string()],
            todo_states: Vec::new(),
            used_fallback: false,
            review_report: None,
            evidence_summary: vec!["单测已通过".to_string()],
            iteration_index: 1,
            based_on_session_id: None,
            feedback_summary: Vec::new(),
            delta_summary: Vec::new(),
            completed_this_iteration: vec!["补齐 TUI 回归".to_string()],
            unaccepted_feedback: Vec::new(),
        }
    }

    fn test_shell() -> AppShell {
        AppShell {
            route: Route::Start,
            history_return_route: Route::Start,
            run_return_route: Route::Start,
            nav_focus: false,
            nav_index: 0,
            start_focus: super::StartFocus::TaskInput,
            history_focus: super::HistoryFocus::Sessions,
            run_focus: super::RunFocus::Subviews,
            start_action_index: 0,
            history_action_index: 0,
            run_action_index: 0,
            selected_field: 0,
            advanced_settings_open: false,
            edit_state: None,
            history_index: 0,
            notices: Vec::new(),
            form: FormState::default(),
            project: ProjectContext {
                target_dir: PathBuf::from("/tmp/demo"),
                display_target: "/tmp/demo".to_string(),
                verification_commands: Vec::new(),
                role_sets: vec!["default".to_string()],
                sessions: Vec::new(),
                last_error: None,
            },
            selected_session: None,
            history_detail: None,
            manual_review: None,
            confirm_dialog: None,
            runtime_state: None,
            run_subview: RunSubview::Dashboard,
            last_doctor_report: None,
            active_command: None,
            pending_review_fix: None,
            exit_esc_armed_at: None,
            should_quit: false,
        }
    }

    fn sample_session_in(root: &Path, id: &str) -> SessionManifest {
        let session_dir = root.join(".codex-forge").join("sessions").join(id);
        SessionManifest {
            id: id.to_string(),
            task: "继续优化博客".to_string(),
            repo_snapshot: RepoSnapshot {
                repo_root: root.to_path_buf(),
                display_name: "demo".to_string(),
                top_level_entries: vec!["src".to_string()],
                detected_stacks: vec!["Rust".to_string()],
                readme_excerpt: None,
            },
            created_at: Utc::now(),
            status: SessionStatus::Completed,
            session_kind: SessionKind::Run,
            ui_mode: UiMode::Minimal,
            workers_requested: 3,
            role_set: "default".to_string(),
            model: None,
            thinking_mode: ThinkingMode::Balanced,
            cleanup_success: false,
            apply_mode: ApplyMode::AutoSafe,
            max_retries: 2,
            fail_fast: false,
            verification_commands: Vec::new(),
            config_path: None,
            preset: None,
            iteration_index: 1,
            shared_context_version: 1,
            root_session_id: id.to_string(),
            parent_session_id: None,
            continuation_kind: None,
            feedback_history: Vec::new(),
            supersedes_session_id: None,
            baseline_artifacts: BaselineArtifacts::default(),
            plan_todo: None,
            todo_states: Vec::new(),
            execution_graph: None,
            execution_contract: None,
            worker_results: Vec::new(),
            artifact_manifest: ArtifactManifest::default(),
            apply_result: None,
            verification_report: None,
            change_trust_report: None,
            doctor_report: None,
            final_summary: None,
            manual_delivery_result: None,
            manual_review_state: None,
            review_fix: None,
            memory_manifest: None,
            source_plan_session_id: None,
            reused_plan_session_id: None,
            resumed_from_session_id: None,
            artifact_index: Vec::new(),
            timeline_events: Vec::new(),
            demo_summary: Vec::new(),
            lineage: Vec::new(),
            session_dir: session_dir.clone(),
            timeline_path: session_dir.join("timeline.jsonl"),
            graph_path: session_dir.join("commander").join("execution-graph.json"),
            execution_contract_path: session_dir
                .join("commander")
                .join("execution-contract.json"),
            summary_json_path: session_dir.join("summary.json"),
            summary_markdown_path: session_dir.join("summary.md"),
            artifact_manifest_path: session_dir.join("artifact-manifest.json"),
            apply_plan_path: session_dir.join("integration").join("apply-plan.json"),
            apply_result_path: session_dir.join("integration").join("apply-result.json"),
            verification_report_path: session_dir
                .join("integration")
                .join("verification-report.json"),
            change_trust_report_path: session_dir
                .join("integration")
                .join("change-trust-report.json"),
        }
    }

    fn test_project_dir() -> TempDir {
        let dir = TempDir::new().expect("temp project");
        fs::write(
            dir.path().join("codex-forge.toml"),
            "[defaults]\nverification_commands = [\"cargo test\"]\n",
        )
        .expect("write config");
        dir
    }

    fn persist_session(_root: &Path, session: &SessionManifest) {
        fs::create_dir_all(&session.session_dir).expect("create session dir");
        fs::write(
            session.session_dir.join("manifest.json"),
            serde_json::to_string_pretty(session).expect("serialize session"),
        )
        .expect("write manifest");
    }

    fn render_shell_to_text(shell: &AppShell, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| shell.render(frame))
            .expect("draw shell");
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

    fn finished_command(action: ShellAction) -> ActiveCommand {
        let (_tx, rx) = mpsc::unbounded_channel();
        ActiveCommand {
            action,
            state: CommandState::Succeeded,
            started_at: Instant::now(),
            finished_at: Some(Instant::now()),
            stop_requested: false,
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
            state: CommandState::Running,
            started_at: Instant::now(),
            finished_at: None,
            stop_requested: false,
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

    fn sample_session(id: &str) -> SessionManifest {
        sample_session_in(Path::new("/tmp/demo"), id)
    }

    #[test]
    fn builds_run_command_from_form() {
        let form = FormState {
            target_dir: "/tmp/demo".to_string(),
            config_path: String::new(),
            task: "实现 v5".to_string(),
            continue_feedback: String::new(),
            review_issue: String::new(),
            continue_mode: ContinueModeArg::Auto,
            from_plan_session_id: String::new(),
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
    fn empty_task_preview_requires_prompt_first() {
        let form = FormState::default();

        let plan_preview =
            build_command_preview(Path::new("/tmp/demo"), &form, ShellAction::Plan, None);
        let run_preview =
            build_command_preview(Path::new("/tmp/demo"), &form, ShellAction::Run, None);

        assert!(plan_preview.summary.contains("先输入提示词"));
        assert!(run_preview.summary.contains("先输入提示词"));
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
    fn builds_continue_command_from_selected_session() {
        let form = FormState {
            continue_feedback: "把验证说明写得更清楚".to_string(),
            continue_mode: ContinueModeArg::Plan,
            ..FormState::default()
        };
        let session = sample_session("session-1");
        let preview = build_command_preview(
            Path::new("/tmp/demo"),
            &form,
            ShellAction::ContinueSelected,
            Some(&session),
        );
        assert!(preview.args.contains(&"continue".to_string()));
        assert!(preview.args.contains(&"--session".to_string()));
        assert!(preview.args.contains(&"session-1".to_string()));
        assert!(preview.args.contains(&"--feedback".to_string()));
        assert!(preview.args.contains(&"把验证说明写得更清楚".to_string()));
        assert!(preview.args.contains(&"--mode".to_string()));
        assert!(preview.args.contains(&"plan".to_string()));
        assert!(preview.summary.contains("继续优化"));
    }

    #[test]
    fn builds_run_command_from_selected_plan_session() {
        let form = FormState {
            task: "实现 v5".to_string(),
            from_plan_session_id: "plan-123".to_string(),
            ..FormState::default()
        };

        let preview = build_command_preview(Path::new("/tmp/demo"), &form, ShellAction::Run, None);

        assert!(preview.args.contains(&"--from-plan".to_string()));
        assert!(preview.args.contains(&"plan-123".to_string()));
        assert!(preview.summary.contains("基于 plan session"));
    }

    #[test]
    fn build_continue_args_uses_selected_continue_mode() {
        let form = FormState {
            continue_feedback: "补上执行验证".to_string(),
            continue_mode: ContinueModeArg::Run,
            ..FormState::default()
        };
        let session = sample_session("session-2");

        let args = build_continue_args(Path::new("/tmp/demo"), &form, Some(&session)).unwrap();

        assert_eq!(args.mode, ContinueModeArg::Run);
        assert_eq!(args.session, "session-2");
        assert_eq!(args.feedback, Some("补上执行验证".to_string()));
    }

    #[test]
    fn available_history_actions_show_execute_plan_for_plan_session_only() {
        let mut plan_session = sample_session("session-plan");
        plan_session.session_kind = SessionKind::Plan;
        plan_session.apply_mode = ApplyMode::None;
        let run_session = sample_session("session-run");

        let plan_actions = super::available_history_actions(Some(&plan_session));
        let run_actions = super::available_history_actions(Some(&run_session));

        assert!(plan_actions.contains(&HistoryAction::ExecutePlan));
        assert!(!plan_actions.contains(&HistoryAction::ResetSelected));
        assert!(!run_actions.contains(&HistoryAction::ExecutePlan));
        assert!(run_actions.contains(&HistoryAction::ResetSelected));
    }

    #[test]
    fn available_history_actions_show_deliver_for_undelivered_run_session() {
        let mut run_session = sample_session("session-run");
        run_session.apply_result = Some(ApplyResult {
            mode: ApplyMode::AutoSafe,
            status: ApplyStatus::Bundled,
            integration_worktree: None,
            applied_workers: Vec::new(),
            rejected_workers: Vec::new(),
            conflicts: vec!["reviewer 明确阻止自动应用".to_string()],
            synced_to_target: false,
            bundle_dir: Some(run_session.session_dir.join("integration").join("bundle")),
            final_patch_path: None,
            log_path: run_session
                .session_dir
                .join("integration")
                .join("apply.log"),
            review_gate: Some(ApplyDecision::Block),
            trust_level: TrustLevel::Low,
            scope_drift: ScopeDrift::Minor,
            accepted_files: vec!["src/app_shell.rs".to_string()],
            manual_review_files: vec!["README.md".to_string()],
            rejected_files: Vec::new(),
            out_of_scope_files: Vec::new(),
            todo_commits: Vec::new(),
            review_report: None,
        });

        let actions = super::available_history_actions(Some(&run_session));

        assert!(actions.contains(&HistoryAction::DeliverAccepted));
    }

    #[test]
    fn available_history_actions_show_manual_review_for_manual_review_files() {
        let mut run_session = sample_session("session-run");
        run_session.apply_result = Some(ApplyResult {
            mode: ApplyMode::AutoSafe,
            status: ApplyStatus::Bundled,
            integration_worktree: None,
            applied_workers: Vec::new(),
            rejected_workers: Vec::new(),
            conflicts: vec!["需要人工复核".to_string()],
            synced_to_target: false,
            bundle_dir: Some(run_session.session_dir.join("integration").join("bundle")),
            final_patch_path: None,
            log_path: run_session
                .session_dir
                .join("integration")
                .join("apply.log"),
            review_gate: Some(ApplyDecision::AllowPartial),
            trust_level: TrustLevel::Medium,
            scope_drift: ScopeDrift::Minor,
            accepted_files: Vec::new(),
            manual_review_files: vec!["README.md".to_string()],
            rejected_files: Vec::new(),
            out_of_scope_files: Vec::new(),
            todo_commits: Vec::new(),
            review_report: None,
        });

        let actions = super::available_history_actions(Some(&run_session));

        assert!(actions.contains(&HistoryAction::ManualReview));
    }

    #[test]
    fn builds_initial_manual_review_state_from_apply_result() {
        let mut run_session = sample_session("session-run");
        run_session.worker_results = vec![crate::model::WorkerResult {
            agent_id: "implementer-1".to_string(),
            role: "implementer".to_string(),
            task_title: "实现主干".to_string(),
            status: crate::model::WorkerStatus::Succeeded,
            exit_code: Some(0),
            attempts: 1,
            diagnostic_summary: Some(String::new()),
            summary: Some(String::new()),
            final_message: String::new(),
            changed_files: vec!["README.md".to_string()],
            worktree_path: PathBuf::from("/tmp"),
            prompt_path: PathBuf::from("/tmp/prompt"),
            stdout_path: PathBuf::from("/tmp/stdout"),
            stderr_path: PathBuf::from("/tmp/stderr"),
            events_path: PathBuf::from("/tmp/events"),
            final_output_path: PathBuf::from("/tmp/final"),
            diff_path: None,
            git_status_path: None,
            handoff_path: None,
            handoff: None,
            error: None,
        }];
        run_session.apply_result = Some(ApplyResult {
            mode: ApplyMode::AutoSafe,
            status: ApplyStatus::Bundled,
            integration_worktree: None,
            applied_workers: Vec::new(),
            rejected_workers: Vec::new(),
            conflicts: vec!["需要人工复核".to_string()],
            synced_to_target: false,
            bundle_dir: None,
            final_patch_path: None,
            log_path: run_session
                .session_dir
                .join("integration")
                .join("apply.log"),
            review_gate: Some(ApplyDecision::AllowPartial),
            trust_level: TrustLevel::Medium,
            scope_drift: ScopeDrift::Minor,
            accepted_files: Vec::new(),
            manual_review_files: vec!["README.md".to_string()],
            rejected_files: Vec::new(),
            out_of_scope_files: Vec::new(),
            todo_commits: Vec::new(),
            review_report: None,
        });

        let state = super::build_initial_manual_review_state(&run_session);

        assert_eq!(state.source_session_id, "session-run");
        assert_eq!(state.selected_file.as_deref(), Some("README.md"));
        assert_eq!(state.files.len(), 1);
        assert_eq!(
            state.files[0].source_workers,
            vec!["implementer-1".to_string()]
        );
    }

    #[test]
    fn recent_task_history_deduplicates_current_project_sessions() {
        let mut shell = test_shell();
        shell.project.sessions = vec![
            SessionSummary {
                id: "s1".to_string(),
                created_at: "03-01 10:00".to_string(),
                task: "修复登录".to_string(),
                stage_label: "已完成".to_string(),
                summary: "done".to_string(),
                mode_label: "运行".to_string(),
                continuable: true,
            },
            SessionSummary {
                id: "s2".to_string(),
                created_at: "03-01 11:00".to_string(),
                task: "修复登录".to_string(),
                stage_label: "已完成".to_string(),
                summary: "done".to_string(),
                mode_label: "运行".to_string(),
                continuable: true,
            },
            SessionSummary {
                id: "s3".to_string(),
                created_at: "03-01 12:00".to_string(),
                task: "生成脚手架".to_string(),
                stage_label: "已完成".to_string(),
                summary: "done".to_string(),
                mode_label: "运行".to_string(),
                continuable: true,
            },
        ];

        let history = super::recent_task_history(&shell.project);

        assert_eq!(
            history,
            vec!["修复登录".to_string(), "生成脚手架".to_string()]
        );
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
        assert_eq!(route_titles(50), vec!["开始", "执行中", "历史结果"]);
        assert_eq!(run_subview_titles(60), vec!["总览", "执行", "交付"]);
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
        assert!(!shell.advanced_settings_open);
        assert_eq!(shell.start_focus, super::StartFocus::Actions);
        assert_eq!(
            shell.current_start_action(),
            super::StartAction::ToggleSettings
        );
    }

    #[test]
    fn toggle_advanced_from_history_navigates_back_to_start() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.history_return_route = Route::Run;

        shell.toggle_advanced_settings();

        assert_eq!(shell.route, Route::Start);
        assert!(!shell.advanced_settings_open);
        assert_eq!(shell.start_focus, super::StartFocus::Actions);
        assert_eq!(
            shell.current_start_action(),
            super::StartAction::ToggleSettings
        );
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

        assert_eq!(shell.route, Route::Start);
        assert!(!shell.advanced_settings_open);
        assert!(
            shell
                .notices
                .last()
                .is_some_and(|message| message.contains("后台动作仍在执行"))
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
    fn tab_navigation_allows_leaving_run_while_command_is_running() {
        let mut shell = test_shell();
        shell.route = Route::Run;
        shell.active_command = Some(running_command(ShellAction::Run));

        shell.navigate_via_tab(Route::History);

        assert_eq!(shell.route, Route::History);
        assert!(
            shell
                .notices
                .last()
                .is_some_and(|message| message.contains("继续切页查看信息"))
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

        assert!(history.contains("切列表/下一步"));
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

        assert!(text.contains("这里会显示你刚刚做了什么"));
        assert!(text.contains("回开始页开始"));
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

        assert!(text.contains("这次已经结束"));
        assert!(text.contains("去历史查看结果"));
    }

    #[test]
    fn shell_action_stop_support_matches_expected_paths() {
        let default_form = FormState::default();
        let mut plan_session = sample_session("session-plan");
        plan_session.session_kind = SessionKind::Plan;
        plan_session.apply_mode = ApplyMode::None;
        let mut run_session = sample_session("session-run");
        run_session.final_summary = Some(sample_final_summary("已有结果"));

        assert!(!action_supports_stop(
            ShellAction::Doctor,
            &default_form,
            Some(&run_session)
        ));
        assert!(!action_supports_stop(
            ShellAction::Plan,
            &default_form,
            Some(&run_session)
        ));
        assert!(action_supports_stop(ShellAction::Run, &default_form, None));
        assert!(action_supports_stop(
            ShellAction::ReplaySelected,
            &default_form,
            None
        ));
        assert!(!action_supports_stop(
            ShellAction::ContinueSelected,
            &default_form,
            Some(&plan_session)
        ));
        assert!(action_supports_stop(
            ShellAction::ContinueSelected,
            &default_form,
            Some(&run_session)
        ));
    }

    #[test]
    fn preferred_run_subview_defaults_to_timeline_for_process_actions() {
        assert_eq!(
            preferred_run_subview(ShellAction::Doctor),
            RunSubview::Dashboard
        );
        assert_eq!(
            preferred_run_subview(ShellAction::Plan),
            RunSubview::Timeline
        );
        assert_eq!(
            preferred_run_subview(ShellAction::Run),
            RunSubview::Timeline
        );
        assert_eq!(
            preferred_run_subview(ShellAction::ContinueSelected),
            RunSubview::Timeline
        );
        assert_eq!(
            preferred_run_subview(ShellAction::ReplaySelected),
            RunSubview::Timeline
        );
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
        assert!(
            prepare_runtime_state(
                ShellAction::ContinueSelected,
                &form,
                Some(&sample_session("session-1"))
            )
            .is_some()
        );
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
            state: CommandState::Running,
            started_at: Instant::now(),
            finished_at: None,
            stop_requested: false,
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
    async fn key_sequence_edits_task_and_saves_with_ctrl_s() {
        let mut shell = test_shell();

        shell.handle_key(key(KeyCode::Char('e'))).await.unwrap();
        shell.handle_key(key(KeyCode::Char('修'))).await.unwrap();
        shell.handle_key(key(KeyCode::Char('复'))).await.unwrap();
        shell.handle_key(ctrl_key('s')).await.unwrap();

        assert_eq!(shell.form.task, "修复");
        assert!(shell.edit_state.is_none());
        assert!(
            shell
                .notices
                .last()
                .is_some_and(|message| message.contains("内容已保存"))
        );
    }

    #[tokio::test]
    async fn ctrl_r_in_task_editor_only_focuses_run_action() {
        let mut shell = test_shell();

        shell.handle_key(key(KeyCode::Char('e'))).await.unwrap();
        shell.handle_key(key(KeyCode::Char('修'))).await.unwrap();
        shell.handle_key(key(KeyCode::Char('复'))).await.unwrap();
        shell.handle_key(ctrl_key('r')).await.unwrap();

        assert_eq!(shell.form.task, "修复");
        assert!(shell.edit_state.is_none());
        assert!(shell.active_command.is_none());
        assert_eq!(shell.route, Route::Start);
        assert_eq!(shell.start_focus, super::StartFocus::Actions);
        assert_eq!(shell.current_start_action(), super::StartAction::Run);
        assert!(shell.notices.last().is_some_and(|message| {
            message.contains("已准备好“开始执行”") && message.contains("手动开始")
        }));
    }

    #[tokio::test]
    async fn ctrl_r_in_feedback_editor_only_focuses_continue_action() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.selected_session = Some(sample_session("session-1"));
        shell.start_editing(FormField::ContinueFeedback);

        shell.handle_key(key(KeyCode::Char('补'))).await.unwrap();
        shell.handle_key(key(KeyCode::Char('充'))).await.unwrap();
        shell.handle_key(ctrl_key('r')).await.unwrap();

        assert_eq!(shell.form.continue_feedback, "补充");
        assert!(shell.edit_state.is_none());
        assert!(shell.active_command.is_none());
        assert_eq!(shell.route, Route::History);
        assert_eq!(shell.history_focus, super::HistoryFocus::Actions);
        assert_eq!(
            shell.current_history_action(),
            super::HistoryAction::Continue
        );
    }

    #[tokio::test]
    async fn key_sequence_history_to_advanced_then_escape_returns_cleanly() {
        let mut shell = test_shell();

        shell.handle_key(key(KeyCode::Char('3'))).await.unwrap();
        assert_eq!(shell.route, Route::History);

        shell.handle_key(key(KeyCode::Char('a'))).await.unwrap();
        assert_eq!(shell.route, Route::Start);
        assert!(!shell.advanced_settings_open);

        shell.handle_key(key(KeyCode::Enter)).await.unwrap();
        assert!(shell.advanced_settings_open);
        shell.handle_key(key(KeyCode::Esc)).await.unwrap();
        assert_eq!(shell.route, Route::Start);
        assert!(!shell.advanced_settings_open);
    }

    #[tokio::test]
    async fn key_sequence_tab_enters_nav_and_brackets_cycle_run_subviews() {
        let mut shell = test_shell();
        shell.route = Route::Run;

        shell.handle_key(key(KeyCode::Tab)).await.unwrap();
        assert!(shell.nav_focus);
        assert_eq!(shell.run_subview, RunSubview::Dashboard);

        shell.handle_key(key(KeyCode::Char(']'))).await.unwrap();
        assert_eq!(shell.run_subview, RunSubview::Dashboard);

        shell.handle_key(key(KeyCode::Tab)).await.unwrap();
        assert!(!shell.nav_focus);

        shell.handle_key(key(KeyCode::Char(']'))).await.unwrap();
        assert_eq!(shell.run_subview, RunSubview::Timeline);

        shell.handle_key(key(KeyCode::Char(']'))).await.unwrap();
        assert_eq!(shell.run_subview, RunSubview::Summary);

        shell.handle_key(key(KeyCode::Char('['))).await.unwrap();
        assert_eq!(shell.run_subview, RunSubview::Timeline);
    }

    #[tokio::test]
    async fn key_sequence_nav_focus_can_switch_route_directly() {
        let mut shell = test_shell();

        shell.handle_key(key(KeyCode::Up)).await.unwrap();
        assert!(shell.nav_focus);

        shell.handle_key(key(KeyCode::Right)).await.unwrap();
        shell.handle_key(key(KeyCode::Right)).await.unwrap();
        assert_eq!(shell.current_nav_route(), Route::History);

        shell.handle_key(key(KeyCode::Enter)).await.unwrap();
        assert_eq!(shell.route, Route::History);
        assert!(!shell.nav_focus);
    }

    #[tokio::test]
    async fn tab_enters_and_exits_nav_focus() {
        let mut shell = test_shell();

        shell.handle_key(key(KeyCode::Tab)).await.unwrap();
        assert!(shell.nav_focus);
        assert_eq!(shell.current_nav_route(), Route::Start);

        shell.handle_key(key(KeyCode::Tab)).await.unwrap();
        assert!(!shell.nav_focus);
        assert_eq!(shell.start_focus, super::StartFocus::TaskInput);
    }

    #[tokio::test]
    async fn ctrl_c_quits_immediately() {
        let mut shell = test_shell();

        shell.handle_key(ctrl_key('c')).await.unwrap();

        assert!(shell.should_quit);
    }

    #[tokio::test]
    async fn double_escape_opens_quit_confirm_on_start_root() {
        let mut shell = test_shell();

        shell.handle_key(key(KeyCode::Esc)).await.unwrap();
        assert!(shell.confirm_dialog.is_none());
        assert!(
            shell
                .notices
                .last()
                .is_some_and(|message| message.contains("再次按 `Esc`"))
        );

        shell.handle_key(key(KeyCode::Esc)).await.unwrap();
        assert!(matches!(
            shell.confirm_dialog.as_ref().map(|dialog| &dialog.action),
            Some(super::ConfirmAction::Quit)
        ));
        assert!(!shell.should_quit);
    }

    #[tokio::test]
    async fn quit_confirm_enter_exits_tui() {
        let mut shell = test_shell();
        shell.open_quit_confirm();

        shell.handle_key(key(KeyCode::Enter)).await.unwrap();

        assert!(shell.should_quit);
    }

    #[tokio::test]
    async fn key_sequence_running_command_keeps_background_state_when_switching_pages() {
        let mut shell = test_shell();
        shell.route = Route::Run;
        shell.active_command = Some(running_command(ShellAction::Run));

        shell.handle_key(key(KeyCode::Char('3'))).await.unwrap();
        assert_eq!(shell.route, Route::History);

        shell.handle_key(key(KeyCode::Char('a'))).await.unwrap();
        assert_eq!(shell.route, Route::Start);
        assert!(!shell.advanced_settings_open);
        assert!(
            shell
                .notices
                .last()
                .is_some_and(|message| message.contains("Enter 可打开“更多设置”"))
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

    #[tokio::test]
    async fn plan_without_task_reopens_task_editor_instead_of_starting() {
        let mut shell = test_shell();

        shell.handle_key(key(KeyCode::Char('p'))).await.unwrap();

        assert_eq!(shell.route, Route::Start);
        assert!(shell.active_command.is_none());
        assert!(matches!(
            shell.edit_state.as_ref().map(|edit| edit.field),
            Some(FormField::Task)
        ));
        assert!(
            shell
                .notices
                .last()
                .is_some_and(|message| message.contains("请先输入提示词"))
        );
    }

    #[tokio::test]
    async fn continue_without_feedback_starts_run_directly() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.selected_session = Some(sample_session("session-1"));

        shell.handle_key(key(KeyCode::Char('c'))).await.unwrap();

        assert_eq!(shell.route, Route::Run);
        assert!(shell.active_command.is_some());
        assert!(shell.edit_state.is_none());
    }

    #[tokio::test]
    async fn history_left_right_switches_focus_between_list_and_actions() {
        let mut shell = test_shell();
        shell.route = Route::History;
        assert_eq!(shell.history_focus, super::HistoryFocus::Sessions);

        shell.handle_key(key(KeyCode::Right)).await.unwrap();
        assert_eq!(shell.history_focus, super::HistoryFocus::Actions);

        shell.handle_key(key(KeyCode::Left)).await.unwrap();
        assert_eq!(shell.history_focus, super::HistoryFocus::Sessions);
    }

    #[tokio::test]
    async fn history_v_opens_and_esc_closes_detail_popup() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.selected_session = Some(sample_session("session-1"));

        shell.handle_key(key(KeyCode::Char('v'))).await.unwrap();
        assert!(shell.history_detail.is_some());

        shell.handle_key(key(KeyCode::Esc)).await.unwrap();
        assert!(shell.history_detail.is_none());
    }

    #[tokio::test]
    async fn history_x_opens_and_esc_closes_clean_confirm() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.selected_session = Some(sample_session("session-1"));

        shell.handle_key(key(KeyCode::Char('x'))).await.unwrap();
        assert!(shell.confirm_dialog.is_some());

        shell.handle_key(key(KeyCode::Esc)).await.unwrap();
        assert!(shell.confirm_dialog.is_none());
    }

    #[tokio::test]
    async fn history_z_opens_reset_confirm() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.selected_session = Some(sample_session("session-1"));

        shell.handle_key(key(KeyCode::Char('z'))).await.unwrap();

        assert!(matches!(
            shell.confirm_dialog.as_ref().map(|state| &state.action),
            Some(super::ConfirmAction::ResetSelected { .. })
        ));
    }

    #[tokio::test]
    async fn history_shift_x_opens_clean_all_confirm() {
        let mut shell = test_shell();
        shell.route = Route::History;

        shell.handle_key(key(KeyCode::Char('X'))).await.unwrap();

        assert!(shell.confirm_dialog.is_some());
    }

    #[tokio::test]
    async fn history_detail_left_right_switch_tabs() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.selected_session = Some(sample_session("session-1"));

        shell.handle_key(key(KeyCode::Char('v'))).await.unwrap();
        assert_eq!(
            shell
                .history_detail
                .as_ref()
                .map(|detail| detail.active_tab),
            Some(super::HistoryDetailTab::Overview)
        );

        shell.handle_key(key(KeyCode::Right)).await.unwrap();
        assert_eq!(
            shell
                .history_detail
                .as_ref()
                .map(|detail| detail.active_tab),
            Some(super::HistoryDetailTab::Plan)
        );

        shell.handle_key(key(KeyCode::Left)).await.unwrap();
        assert_eq!(
            shell
                .history_detail
                .as_ref()
                .map(|detail| detail.active_tab),
            Some(super::HistoryDetailTab::Overview)
        );
    }

    #[tokio::test]
    async fn history_detail_lazy_loads_current_tab_only() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.selected_session = Some(sample_session("session-1"));

        shell.handle_key(key(KeyCode::Char('v'))).await.unwrap();
        let initial = shell
            .history_detail
            .as_ref()
            .map(|detail| detail.detail.clone())
            .unwrap_or_default();
        assert!(initial.contains("会话：session-1"));

        shell.handle_key(key(KeyCode::Right)).await.unwrap();
        let switched = shell
            .history_detail
            .as_ref()
            .map(|detail| detail.detail.clone())
            .unwrap_or_default();
        assert_ne!(initial, switched);
        assert!(switched.contains("Execution Graph"));
    }

    #[test]
    fn page_text_slices_lines_by_page() {
        let text = (0..6)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(page_count_for_text(&text, 2), 3);
        assert_eq!(page_text(&text, 0, 2), "line-0\nline-1");
        assert_eq!(page_text(&text, 1, 2), "line-2\nline-3");
        assert_eq!(page_text(&text, 2, 2), "line-4\nline-5");
    }

    #[tokio::test]
    async fn history_detail_page_down_switches_page_slice() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.selected_session = Some(sample_session("session-1"));
        shell.open_history_detail();

        if let Some(detail) = shell.history_detail.as_mut() {
            detail.detail = (0..(super::HISTORY_DETAIL_PAGE_LINES + 5))
                .map(|i| format!("line-{i}"))
                .collect::<Vec<_>>()
                .join("\n");
        }

        let before = shell
            .history_detail
            .as_ref()
            .map(|detail| detail.current_page_text())
            .unwrap_or_default();
        assert!(before.contains("line-0"));
        assert!(!before.contains(&format!("line-{}", super::HISTORY_DETAIL_PAGE_LINES)));

        shell.handle_key(key(KeyCode::PageDown)).await.unwrap();

        let after = shell
            .history_detail
            .as_ref()
            .map(|detail| detail.current_page_text())
            .unwrap_or_default();
        assert!(after.contains(&format!("line-{}", super::HISTORY_DETAIL_PAGE_LINES)));
        assert!(!after.contains("line-0"));
    }

    #[test]
    fn push_command_output_trims_old_lines() {
        let mut output = Vec::new();
        for index in 0..260 {
            push_command_output(&mut output, format!("line-{index}"));
        }

        assert_eq!(output.len(), super::MAX_LOG_LINES);
        assert_eq!(output.first().map(String::as_str), Some("line-20"));
        assert_eq!(output.last().map(String::as_str), Some("line-259"));
    }

    #[test]
    fn push_notice_deduplicates_and_limits_notice_count() {
        let mut shell = test_shell();
        shell.notices.clear();
        shell.push_notice("重复提示");
        shell.push_notice("重复提示");
        for index in 0..12 {
            shell.push_notice(&format!("notice-{index}"));
        }

        assert_eq!(shell.notices.len(), super::MAX_NOTICE_LINES);
        assert!(!shell.notices.iter().any(|item| item == "重复提示"));
        assert_eq!(shell.notices.first().map(String::as_str), Some("notice-4"));
        assert_eq!(shell.notices.last().map(String::as_str), Some("notice-11"));
    }

    #[test]
    fn run_summary_widget_prefers_runtime_summary_over_session_summary() {
        let mut shell = test_shell();
        let mut session = sample_session("session-1");
        session.final_summary = Some(sample_final_summary("历史摘要"));
        shell.selected_session = Some(session);
        shell.runtime_state = Some(crate::ui::RuntimeViewState::new("session-1", "当前任务"));
        shell.route = Route::Run;
        shell.run_subview = RunSubview::Summary;
        shell.runtime_state.as_mut().expect("runtime").summary =
            Some(sample_final_summary("实时摘要"));

        let rendered = render_shell_to_text(&shell, 120, 36);
        let normalized = normalize_rendered_text(&rendered);
        assert!(normalized.contains("实时摘要"), "{rendered}");
        assert!(!normalized.contains("历史摘要"), "{rendered}");
        assert!(normalized.contains("结果：完成"), "{rendered}");
    }

    #[test]
    fn run_summary_widget_lists_real_repo_exports() {
        let temp = TempDir::new().expect("temp dir");
        let mut shell = test_shell();
        let mut session = sample_session_in(temp.path(), "session-export");
        fs::create_dir_all(temp.path().join("src")).expect("create src dir");
        fs::write(temp.path().join("src").join("app_shell.rs"), "// exported")
            .expect("write export");
        session.final_summary = Some(sample_final_summary("导出摘要"));
        shell.selected_session = Some(session);
        shell.route = Route::Run;
        shell.run_subview = RunSubview::Summary;

        let rendered = render_shell_to_text(&shell, 120, 36);
        let normalized = normalize_rendered_text(&rendered);
        assert!(normalized.contains("用户导出件："), "{rendered}");
        assert!(normalized.contains("src/app_shell.rs"), "{rendered}");
        assert!(!normalized.contains("codex-forge-summary.md"), "{rendered}");
    }

    #[test]
    fn run_timeline_widget_shows_session_events_when_idle() {
        let mut shell = test_shell();
        let mut session = sample_session("session-2");
        session.timeline_events = vec![TimelineEventSummary {
            ts: Utc::now(),
            title: "阶段切换".to_string(),
            detail: "进入回放".to_string(),
        }];
        shell.selected_session = Some(session);
        shell.run_subview = RunSubview::Timeline;
        shell.route = Route::Run;

        let rendered = render_shell_to_text(&shell, 120, 36);
        let normalized = normalize_rendered_text(&rendered);
        assert!(normalized.contains("历史会话：session-2"), "{rendered}");
        assert!(normalized.contains("阶段切换"), "{rendered}");
        assert!(normalized.contains("进入回放"), "{rendered}");
    }

    #[test]
    fn run_timeline_widget_shows_live_execution_stream() {
        let mut shell = test_shell();
        shell.runtime_state = Some(crate::ui::RuntimeViewState::new("session-live", "实时执行"));
        shell.run_subview = RunSubview::Timeline;
        shell.route = Route::Run;
        if let Some(state) = shell.runtime_state.as_mut() {
            state.apply(&RuntimeEvent::PhaseChanged {
                phase: "规划中".to_string(),
            });
            state.apply(&RuntimeEvent::WorkerDispatched {
                agent_id: "implementer-1".to_string(),
                role: "implementer".to_string(),
                title: "补齐执行流".to_string(),
                worktree_path: "/tmp/demo".into(),
            });
            state.apply(&RuntimeEvent::WorkerOutput {
                agent_id: "implementer-1".to_string(),
                stream: "stdout".to_string(),
                message: "cargo test -q".to_string(),
            });
        }

        let rendered = render_shell_to_text(&shell, 120, 36);
        let normalized = normalize_rendered_text(&rendered);
        assert!(normalized.contains("执行流"), "{rendered}");
        assert!(normalized.contains("当前焦点：implementer-1"), "{rendered}");
        assert!(normalized.contains("cargotest-q"), "{rendered}");
    }

    #[test]
    fn history_artifacts_prefers_system_artifacts_and_downgrades_missing_repo_exports() {
        let temp = TempDir::new().expect("temp dir");
        let session = sample_session_in(temp.path(), "session-artifacts");
        fs::create_dir_all(&session.session_dir).expect("session dir");
        fs::create_dir_all(session.apply_result_path.parent().expect("integration dir"))
            .expect("integration dir");
        fs::write(&session.summary_markdown_path, "# Summary\n\n系统工件").expect("summary md");
        fs::write(&session.summary_json_path, "{\"overview\":\"系统工件\"}").expect("summary json");
        fs::write(&session.apply_result_path, "{\"status\":\"applied\"}").expect("apply result");
        fs::write(&session.verification_report_path, "{\"status\":\"passed\"}")
            .expect("verification report");

        let body = super::build_history_detail_body(&session, super::HistoryDetailTab::Artifacts);
        assert!(body.contains("===== Summary Markdown ====="), "{body}");
        assert!(
            body.contains("===== Process Summary Markdown ====="),
            "{body}"
        );
        assert!(body.contains("===== Repo Exports ====="), "{body}");
        assert!(body.contains("当前会话尚未交付到目标目录"), "{body}");

        let summary =
            super::build_history_detail_summary(&session, super::HistoryDetailTab::Artifacts);
        assert!(summary.contains("系统工件"), "{summary}");
        assert!(summary.contains("用户导出件"), "{summary}");
    }

    #[test]
    fn render_edit_and_confirm_popups_expose_key_tui_copy() {
        let mut shell = test_shell();
        shell.start_editing(FormField::Task);
        let edit_rendered = render_shell_to_text(&shell, 120, 36);
        let edit_normalized = normalize_rendered_text(&edit_rendered);
        assert!(
            edit_normalized.contains("编辑字段：任务描述"),
            "{edit_rendered}"
        );
        assert!(
            edit_normalized.contains("字符数：0光标：0"),
            "{edit_rendered}"
        );
        assert!(edit_normalized.contains("快捷键"), "{edit_rendered}");
        assert!(edit_normalized.contains("保存：Ctrl+S"), "{edit_rendered}");
        assert!(edit_normalized.contains("退出：Esc"), "{edit_rendered}");

        shell.edit_state = None;
        shell.route = Route::History;
        shell.open_clean_all_confirm();
        let confirm_rendered = render_shell_to_text(&shell, 120, 36);
        let confirm_normalized = normalize_rendered_text(&confirm_rendered);
        assert!(
            confirm_normalized.contains("确认一键清空"),
            "{confirm_rendered}"
        );
        assert!(
            confirm_normalized.contains(".codex-forge"),
            "{confirm_rendered}"
        );
        assert!(confirm_normalized.contains("快捷键"), "{confirm_rendered}");
        assert!(
            confirm_normalized.contains("确认：Enter"),
            "{confirm_rendered}"
        );
        assert!(
            confirm_normalized.contains("取消：Esc"),
            "{confirm_rendered}"
        );
    }

    #[test]
    fn history_detail_popup_exposes_navigation_shortcuts() {
        let mut shell = test_shell();
        shell.route = Route::History;
        shell.selected_session = Some(sample_session("session-1"));
        shell.open_history_detail();
        if let Some(detail) = shell.history_detail.as_mut() {
            detail.detail = "short body".to_string();
        }

        let rendered = render_shell_to_text(&shell, 120, 40);
        let normalized = normalize_rendered_text(&rendered);
        assert!(normalized.contains("快捷键"), "{rendered}");
        assert!(normalized.contains("关闭：Esc/v"), "{rendered}");
        assert!(normalized.contains("切页：Tab/←→/[]"), "{rendered}");
        assert!(
            normalized.contains("翻页：PgUp/PgDn/Home/End"),
            "{rendered}"
        );
    }

    #[test]
    fn poll_command_output_updates_runtime_state_and_refreshes_history_after_run_finish() {
        let project_dir = test_project_dir();
        let session = sample_session_in(project_dir.path(), "session-finished");
        persist_session(project_dir.path(), &session);

        let mut shell = test_shell();
        shell.project.target_dir = project_dir.path().to_path_buf();
        shell.project.display_target = project_dir.path().display().to_string();
        shell.form.target_dir = project_dir.path().display().to_string();
        shell.runtime_state = Some(crate::ui::RuntimeViewState::new("准备中", "旧任务"));

        let (tx, rx) = mpsc::unbounded_channel();
        shell.active_command = Some(ActiveCommand {
            action: ShellAction::Run,
            state: CommandState::Running,
            started_at: Instant::now(),
            finished_at: None,
            stop_requested: false,
            output: Vec::new(),
            cancel_tx: None,
            rx,
        });

        let _ = tx.send(super::RunnerEvent::Runtime(RuntimeEvent::PhaseChanged {
            phase: "执行中".to_string(),
        }));
        let _ = tx.send(super::RunnerEvent::Runtime(RuntimeEvent::SummaryReady {
            summary: Box::new(sample_final_summary("运行态摘要")),
        }));
        let _ = tx.send(super::RunnerEvent::Finished {
            state: CommandState::Succeeded,
            manifest: Box::new(Some(session.clone())),
        });

        shell.poll_command_output().unwrap();

        assert_eq!(
            shell.active_command.as_ref().map(|command| command.state),
            Some(CommandState::Succeeded)
        );
        assert!(
            shell
                .active_command
                .as_ref()
                .and_then(|command| command.finished_at)
                .is_some()
        );
        assert_eq!(shell.route, Route::History);
        assert_eq!(
            shell
                .runtime_state
                .as_ref()
                .map(|state| state.session_id.as_str()),
            Some("session-finished")
        );
        assert_eq!(
            shell
                .runtime_state
                .as_ref()
                .and_then(|state| state.summary.as_ref())
                .map(|summary| summary.overview.as_str()),
            Some("运行态摘要")
        );
        assert_eq!(
            shell
                .selected_session
                .as_ref()
                .map(|session| session.id.as_str()),
            Some("session-finished")
        );
        assert!(shell.active_command.as_ref().is_some_and(|command| {
            command
                .output
                .iter()
                .any(|line| line.contains("进入执行中阶段"))
        }));
        assert!(
            shell
                .run_log_lines()
                .iter()
                .any(|line| line.to_string().contains("总耗时")),
        );
    }

    #[test]
    fn poll_command_output_records_doctor_checks_into_run_log() {
        let mut shell = test_shell();
        let (tx, rx) = mpsc::unbounded_channel();
        shell.active_command = Some(ActiveCommand {
            action: ShellAction::Doctor,
            state: CommandState::Running,
            started_at: Instant::now(),
            finished_at: None,
            stop_requested: false,
            output: Vec::new(),
            cancel_tx: None,
            rx,
        });

        let _ = tx.send(super::RunnerEvent::Doctor(DoctorReport {
            checks: vec![DoctorCheck {
                name: "git".to_string(),
                status: crate::model::CheckStatus::Passed,
                detail: "仓库可用".to_string(),
            }],
            ok: true,
            readiness: DoctorReadiness::Green,
            summary: "环境正常".to_string(),
            demo_mode: false,
            recommended_role_set: "default".to_string(),
            recommended_apply_mode: ApplyMode::AutoSafe,
        }));

        shell.poll_command_output().unwrap();

        assert_eq!(
            shell
                .last_doctor_report
                .as_ref()
                .map(|report| report.summary.as_str()),
            Some("环境正常")
        );
        assert!(shell.active_command.as_ref().is_some_and(|command| {
            command
                .output
                .iter()
                .any(|line| line.contains("[通过] git - 仓库可用"))
        }));
    }

    #[test]
    fn doctor_dashboard_surfaces_failed_checks_and_summary() {
        let mut shell = test_shell();
        shell.route = Route::Run;
        shell.run_subview = RunSubview::Dashboard;
        shell.active_command = Some(finished_command(ShellAction::Doctor));
        shell.last_doctor_report = Some(DoctorReport {
            checks: vec![DoctorCheck {
                name: "worktree".to_string(),
                status: crate::model::CheckStatus::Failed,
                detail: "git worktree add 失败".to_string(),
            }],
            ok: false,
            readiness: DoctorReadiness::Red,
            summary: "存在阻塞问题，请先处理失败项。".to_string(),
            demo_mode: false,
            recommended_role_set: "default".to_string(),
            recommended_apply_mode: ApplyMode::AutoSafe,
        });

        let rendered = render_shell_to_text(&shell, 120, 36);
        let normalized = normalize_rendered_text(&rendered);
        assert!(normalized.contains("检查结论：红色"), "{rendered}");
        assert!(
            normalized.contains("[失败]worktree-gitworktreeadd失败"),
            "{rendered}"
        );
    }
}
