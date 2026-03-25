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
use crate::apply::deliver_selected_files_from_plan;
use crate::cli::{
    ApplyModeArg, ContinueArgs, ContinueModeArg, PlanArgs, RunArgs, SharedTaskArgs,
    ThinkingModeArg, UiModeArg,
};
use crate::config::{LoadedProjectConfig, load_project_config, validate_project_config};
use crate::doctor::run_doctor;
use crate::model::{
    ApplyMode, ApplyPlan, ApplyStatus, DoctorReport, ManualDeliveryResult, ManualReviewFileRecord,
    ManualReviewFileStatus, ManualReviewState, RuntimeEvent, SessionManifest, SessionPreset,
    ThinkingMode, UiMode,
};
use crate::orchestrator::{EmbeddedRunOutcome, run_session_embedded};
use crate::replay::replay_session_embedded;
use crate::resources::{ResourceCatalog, load_resource_catalog, resolve_role_set};
use crate::session::{
    cleanup_all_forge_artifacts, cleanup_session_lineage, load_session, reset_session_lineage,
    set_manual_delivery_result_for_loaded_session, set_manual_review_state_for_loaded_session,
};
use crate::time::format_beijing;
use crate::ui::{RuntimeViewState, describe_runtime_event, render_runtime_dashboard};
use crate::workspace::{remember_target_dir, resolve_target_dir};

mod commands;
mod detail;
mod edit;
mod flow;
mod input;
mod project;
mod render;
mod text;

use self::commands::*;
use self::detail::*;
use self::edit::*;
use self::project::*;
use self::text::*;

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
            Self::ThinkingMode => "Codex 思考强度",
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
    ExecutePlanSelected,
    ContinueSelected,
    ReviewFixSelected,
    ReplaySelected,
    ConfigValidate,
    AgentsList,
}

impl ShellAction {
    fn label(self) -> &'static str {
        match self {
            Self::Doctor => "检查环境",
            Self::Plan => "先看方案",
            Self::Run => "开始运行",
            Self::ExecutePlanSelected => "执行此方案",
            Self::ContinueSelected => "继续优化",
            Self::ReviewFixSelected => "修复当前文件",
            Self::ReplaySelected => "回放过程",
            Self::ConfigValidate => "校验配置",
            Self::AgentsList => "查看角色",
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
/// - Summary：收敛与结果摘要
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
            Self::Summary => "最终结果",
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
    ConfigValidate,
    AgentsList,
    ToggleSettings,
}

impl StartAction {
    fn all() -> [Self; 6] {
        [
            Self::Doctor,
            Self::Plan,
            Self::Run,
            Self::ConfigValidate,
            Self::AgentsList,
            Self::ToggleSettings,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryAction {
    ExecutePlan,
    Continue,
    EditFeedback,
    ContinueMode,
    ManualWrite,
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
            thinking_mode: ThinkingMode::Balanced,
            role_set: "default".to_string(),
            workers: "4".to_string(),
            max_retries: "2".to_string(),
            model: String::new(),
            apply_mode: ApplyMode::InPlace,
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
                "默认主路径：先写任务，再直接开始运行；系统会先规划，再自动执行并落地到目标目录。"
                    .to_string(),
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
    fn builds_plan_command_from_form() {
        let form = FormState {
            target_dir: "/tmp/demo".to_string(),
            task: "先出方案".to_string(),
            thinking_mode: ThinkingMode::Balanced,
            role_set: "default".to_string(),
            workers: "3".to_string(),
            ..FormState::default()
        };

        let preview = build_command_preview(Path::new("/tmp/demo"), &form, ShellAction::Plan, None);
        assert!(preview.args.contains(&"plan".to_string()));
        assert!(preview.args.contains(&"先出方案".to_string()));
        assert!(preview.summary.contains("生成方案"));
    }

    #[test]
    fn empty_task_preview_requires_prompt_first() {
        let form = FormState::default();
        let run_preview =
            build_command_preview(Path::new("/tmp/demo"), &form, ShellAction::Run, None);

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
            continue_mode: ContinueModeArg::Run,
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
        assert!(preview.args.contains(&"run".to_string()));
        assert!(preview.summary.contains("继续优化"));
    }

    #[test]
    fn builds_execute_plan_command_from_selected_session() {
        let form = FormState {
            task: "会被方案覆盖".to_string(),
            apply_mode: ApplyMode::AutoSafe,
            max_retries: "2".to_string(),
            ..FormState::default()
        };
        let mut session = sample_session("session-plan");
        session.session_kind = crate::model::SessionKind::Plan;
        session.task = "按方案执行".to_string();
        let preview = build_command_preview(
            Path::new("/tmp/demo"),
            &form,
            ShellAction::ExecutePlanSelected,
            Some(&session),
        );
        assert!(preview.args.contains(&"run".to_string()));
        assert!(preview.args.contains(&"--from-plan".to_string()));
        assert!(preview.args.contains(&"session-plan".to_string()));
        assert!(preview.args.contains(&"按方案执行".to_string()));
    }

    #[test]
    fn builds_run_command_without_plan_selector() {
        let form = FormState {
            task: "实现 v5".to_string(),
            ..FormState::default()
        };

        let preview = build_command_preview(Path::new("/tmp/demo"), &form, ShellAction::Run, None);

        assert!(!preview.args.contains(&"--from-plan".to_string()));
        assert!(preview.summary.contains("系统会先规划，再自动执行并落地"));
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
    fn available_history_actions_hide_plan_and_manual_delivery_actions() {
        let run_session = sample_session("session-run");

        let run_actions = super::available_history_actions(Some(&run_session));

        assert!(run_actions.contains(&HistoryAction::ResetSelected));
        assert!(run_actions.iter().all(|action| {
            matches!(
                action,
                HistoryAction::Continue
                    | HistoryAction::EditFeedback
                    | HistoryAction::ContinueMode
                    | HistoryAction::Replay
                    | HistoryAction::Detail
                    | HistoryAction::ResetSelected
                    | HistoryAction::CleanSelected
                    | HistoryAction::CleanAll
                    | HistoryAction::BackToStart
            )
        }));
    }

    #[test]
    fn available_history_actions_for_plan_session_show_execute_plan() {
        let mut plan_session = sample_session("session-plan");
        plan_session.session_kind = crate::model::SessionKind::Plan;

        let actions = super::available_history_actions(Some(&plan_session));

        assert!(actions.contains(&HistoryAction::ExecutePlan));
        assert!(!actions.contains(&HistoryAction::Continue));
        assert!(!actions.contains(&HistoryAction::ResetSelected));
    }

    #[test]
    fn available_history_actions_show_manual_write_when_auto_write_missing() {
        let mut run_session = sample_session("session-run");
        run_session.apply_result = Some(ApplyResult {
            mode: ApplyMode::Bundle,
            status: ApplyStatus::Bundled,
            integration_worktree: None,
            applied_workers: Vec::new(),
            rejected_workers: Vec::new(),
            conflicts: Vec::new(),
            wrote_to_target: false,
            synced_to_target: false,
            bundle_dir: None,
            final_patch_path: None,
            log_path: run_session
                .session_dir
                .join("integration")
                .join("apply.log"),
            review_gate: Some(ApplyDecision::AllowFull),
            trust_level: TrustLevel::Medium,
            scope_drift: ScopeDrift::None,
            accepted_files: vec!["src/main.rs".to_string()],
            manual_review_files: Vec::new(),
            rejected_files: Vec::new(),
            out_of_scope_files: Vec::new(),
            todo_commits: Vec::new(),
            review_report: None,
        });

        let run_actions = super::available_history_actions(Some(&run_session));

        assert!(run_actions.contains(&HistoryAction::ManualWrite));
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
            wrote_to_target: false,
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
    fn start_route_surfaces_advanced_settings_and_codex_effort() {
        let shell = test_shell();
        let rendered = render_shell_to_text(&shell, 120, 36);
        let normalized = normalize_rendered_text(&rendered);

        assert!(normalized.contains("高级设置"), "{rendered}");
        assert!(normalized.contains("Codex思考强度"), "{rendered}");
        assert!(normalized.contains("切换Codex思考强度"), "{rendered}");
    }

    #[test]
    fn shell_action_stop_support_matches_expected_paths() {
        let default_form = FormState::default();
        let mut run_session = sample_session("session-run");
        run_session.final_summary = Some(sample_final_summary("已有结果"));

        assert!(!action_supports_stop(
            ShellAction::Doctor,
            &default_form,
            Some(&run_session)
        ));
        assert!(action_supports_stop(ShellAction::Plan, &default_form, None));
        assert!(action_supports_stop(ShellAction::Run, &default_form, None));
        assert!(action_supports_stop(
            ShellAction::ExecutePlanSelected,
            &default_form,
            Some(&run_session)
        ));
        assert!(action_supports_stop(
            ShellAction::ReplaySelected,
            &default_form,
            None
        ));
        assert!(action_supports_stop(
            ShellAction::ContinueSelected,
            &default_form,
            Some(&run_session)
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
            message.contains("已准备好“开始运行”") && message.contains("手动开始")
        }));
    }

    #[tokio::test]
    async fn ctrl_p_in_task_editor_only_focuses_plan_action() {
        let mut shell = test_shell();

        shell.handle_key(key(KeyCode::Char('e'))).await.unwrap();
        shell.handle_key(key(KeyCode::Char('修'))).await.unwrap();
        shell.handle_key(key(KeyCode::Char('复'))).await.unwrap();
        shell.handle_key(ctrl_key('p')).await.unwrap();

        assert_eq!(shell.form.task, "修复");
        assert!(shell.edit_state.is_none());
        assert!(shell.active_command.is_none());
        assert_eq!(shell.route, Route::Start);
        assert_eq!(shell.start_focus, super::StartFocus::Actions);
        assert_eq!(shell.current_start_action(), super::StartAction::Plan);
        assert!(shell.notices.last().is_some_and(|message| {
            message.contains("已准备好“先看方案”") && message.contains("手动开始")
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
                .is_some_and(|message| message.contains("Enter 可打开“高级设置”"))
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
    async fn run_without_task_reopens_task_editor_instead_of_starting() {
        let mut shell = test_shell();

        shell.handle_key(key(KeyCode::Char('r'))).await.unwrap();

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
        assert!(normalized.contains("用户结果件："), "{rendered}");
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
        assert!(body.contains("当前会话尚未写入目标目录"), "{body}");

        let summary =
            super::build_history_detail_summary(&session, super::HistoryDetailTab::Artifacts);
        assert!(summary.contains("系统工件"), "{summary}");
        assert!(summary.contains("用户结果件"), "{summary}");
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
