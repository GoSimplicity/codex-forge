use super::*;

pub(super) fn bool_label(value: bool) -> String {
    if value {
        "开启".to_string()
    } else {
        "关闭".to_string()
    }
}

pub(super) fn empty_to_dash(value: &str) -> String {
    if value.trim().is_empty() {
        "—".to_string()
    } else {
        value.to_string()
    }
}

pub(super) fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.replace('\n', " ");
    if trimmed.chars().count() <= max {
        trimmed
    } else {
        format!("{}…", trimmed.chars().take(max).collect::<String>())
    }
}

pub(super) fn summarize_task(task: &str) -> String {
    if task.trim().is_empty() {
        "当前任务".to_string()
    } else {
        truncate(task, 24)
    }
}

pub(super) fn thinking_mode_user_title(mode: ThinkingMode) -> &'static str {
    match mode {
        ThinkingMode::Quick => "快速推进",
        ThinkingMode::Balanced => "平衡模式",
        ThinkingMode::HardThink => "复杂任务",
    }
}

pub(super) fn thinking_mode_user_hint(mode: ThinkingMode) -> &'static str {
    match mode {
        ThinkingMode::Quick => "更快出结果",
        ThinkingMode::Balanced => "默认推荐",
        ThinkingMode::HardThink => "更重视边界和风险",
    }
}

pub(super) fn apply_mode_user_label(mode: ApplyMode) -> &'static str {
    match mode {
        ApplyMode::InPlace => "直接写入目标目录",
        ApplyMode::AutoSafe => "自动安全落地",
        ApplyMode::Bundle => "只输出变更包",
        ApplyMode::None => "只做方案和审阅",
    }
}

pub(super) fn continue_mode_user_title(mode: ContinueModeArg) -> &'static str {
    match mode {
        ContinueModeArg::Auto => "自动续跑",
        ContinueModeArg::Run => "直接执行",
    }
}

pub(super) fn continue_mode_user_label(mode: ContinueModeArg) -> &'static str {
    match mode {
        ContinueModeArg::Auto => "自动续跑（auto）",
        ContinueModeArg::Run => "直接执行（run）",
    }
}

pub(super) fn continue_mode_arg_value(mode: ContinueModeArg) -> &'static str {
    match mode {
        ContinueModeArg::Auto => "auto",
        ContinueModeArg::Run => "run",
    }
}

pub(super) fn advanced_fields() -> &'static [FormField] {
    &[
        FormField::TargetDir,
        FormField::ConfigPath,
        FormField::ContinueMode,
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

pub(super) fn advanced_settings_summary(form: &FormState) -> String {
    let mut items = vec![
        format!("{} 个 worker", form.workers),
        form.role_set.clone(),
        apply_mode_user_label(form.apply_mode).to_string(),
    ];
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

pub(super) fn verification_summary(project: &ProjectContext) -> String {
    if project.verification_commands.is_empty() {
        "未配置".to_string()
    } else {
        truncate(&project.verification_commands.join("  |  "), 100)
    }
}

pub(super) fn field_is_editable(field: FormField) -> bool {
    matches!(
        field,
        FormField::TargetDir
            | FormField::ConfigPath
            | FormField::Task
            | FormField::ContinueFeedback
            | FormField::Workers
            | FormField::MaxRetries
            | FormField::Model
            | FormField::ResumeSession
    )
}

pub(super) fn cycle_thinking_mode(current: ThinkingMode, forward: bool) -> ThinkingMode {
    let modes = [
        ThinkingMode::Quick,
        ThinkingMode::Balanced,
        ThinkingMode::HardThink,
    ];
    let index = modes.iter().position(|item| *item == current).unwrap_or(1);
    modes[cycle_index(index, modes.len(), forward)]
}

pub(super) fn cycle_continue_mode(current: ContinueModeArg, forward: bool) -> ContinueModeArg {
    let modes = [ContinueModeArg::Auto, ContinueModeArg::Run];
    let index = modes.iter().position(|item| *item == current).unwrap_or(0);
    modes[cycle_index(index, modes.len(), forward)]
}

pub(super) fn thinking_mode_color(mode: ThinkingMode) -> Color {
    match mode {
        ThinkingMode::Quick => Color::LightBlue,
        ThinkingMode::Balanced => Color::LightGreen,
        ThinkingMode::HardThink => Color::LightMagenta,
    }
}

pub(super) fn contextual_help_lines(
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
            "开始：Enter 写任务或打开字段编辑，↑↓ 选动作，p 方案，r 运行，k 校验配置，i 查看角色，Tab 进导航，Esc 收起高级设置。"
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
            "历史：←→ 切列表/下一步，↑↓ 选择，Enter 查看详情/执行动作，e 补反馈，v 详情，l 回放，z 重置，x 删除，Esc 返回，Tab 进导航。"
        })],
    }
}

pub(super) fn shell_layout_constraints(area: Rect) -> Vec<Constraint> {
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

pub(super) fn route_titles(width: u16) -> Vec<&'static str> {
    let _ = width;
    Route::all().iter().map(|route| route.label()).collect()
}

pub(super) fn run_subview_titles(width: u16) -> Vec<&'static str> {
    if width < 68 {
        vec!["总览", "执行", "交付"]
    } else {
        RunSubview::all().iter().map(|item| item.label()).collect()
    }
}

pub(super) fn history_detail_tab_titles(width: u16) -> Vec<&'static str> {
    let _ = width;
    HistoryDetailTab::all()
        .iter()
        .map(|item| item.label())
        .collect()
}

pub(super) fn split_main_sections(area: Rect) -> Vec<Rect> {
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

pub(super) fn run_route_constraints(area: Rect) -> Vec<Constraint> {
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

pub(super) fn popup_percent(size: u16, desired: u16, max: u16) -> u16 {
    if size < 60 {
        max
    } else if size < 90 {
        desired.max(80)
    } else {
        desired
    }
}

pub(super) fn shell_escape(value: &str) -> String {
    if value.contains(' ') {
        format!("{value:?}")
    } else {
        value.to_string()
    }
}

pub(super) fn next_history_return_route(current: Route, stored: Route, next: Route) -> Route {
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

pub(super) fn next_run_return_route(current: Route, stored: Route, next: Route) -> Route {
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

pub(super) fn history_back_route(history_return_route: Route) -> Route {
    if history_return_route == Route::History {
        Route::Start
    } else {
        history_return_route
    }
}

pub(super) fn run_back_route(run_return_route: Route) -> Route {
    if run_return_route == Route::Run {
        Route::Start
    } else {
        run_return_route
    }
}

pub(super) fn cycle_index(current: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if forward {
        (current + 1) % len
    } else {
        (current + len - 1) % len
    }
}

pub(super) fn centered_rect(horizontal: u16, vertical: u16, area: Rect) -> Rect {
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
