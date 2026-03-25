use super::*;

pub(super) fn parse_usize(value: &str) -> Option<usize> {
    value.trim().parse::<usize>().ok()
}

pub(super) fn empty_as_none(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(super) fn load_project_context(
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

pub(super) fn hydrate_form_from_loaded(
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

pub(super) fn load_session_summaries(target_dir: &Path) -> Result<Vec<SessionSummary>> {
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

pub(super) fn session_root(target_dir: &Path) -> PathBuf {
    discover_repo_root(target_dir)
        .unwrap_or_else(|| target_dir.to_path_buf())
        .join(".codex-forge")
        .join("sessions")
}

pub(super) fn discover_repo_root(target_dir: &Path) -> Option<PathBuf> {
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

pub(super) fn sorted_role_sets(resources: &ResourceCatalog) -> Vec<String> {
    let mut items = resources.role_sets.keys().cloned().collect::<Vec<_>>();
    items.sort();
    items
}

pub(super) fn append_shared_args(args: &mut Vec<String>, target_dir: &Path, form: &FormState) {
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

pub(super) fn build_action_summary(
    _target_dir: &Path,
    form: &FormState,
    action: ShellAction,
    selected_session: Option<&SessionManifest>,
) -> String {
    match action {
        ShellAction::Doctor => "检查当前仓库的环境、配置和验证条件".to_string(),
        ShellAction::Plan => {
            if form.task.trim().is_empty() {
                "请先输入提示词，然后先生成方案".to_string()
            } else {
                format!(
                    "先为“{}”生成方案；只输出计划与执行图，不会直接写目标目录",
                    summarize_task(&form.task)
                )
            }
        }
        ShellAction::Run => {
            if form.task.trim().is_empty() {
                "请先输入提示词，然后再开始运行".to_string()
            } else if !form.resume_session_id.trim().is_empty() {
                format!(
                    "恢复运行 session `{}`，继续处理“{}”",
                    truncate(&form.resume_session_id, 18),
                    summarize_task(&form.task)
                )
            } else {
                format!(
                    "直接开始处理“{}”；系统会先规划，再自动执行并落地，默认使用 {}",
                    summarize_task(&form.task),
                    advanced_settings_summary(form)
                )
            }
        }
        ShellAction::ExecutePlanSelected => selected_session
            .map(|session| {
                format!(
                    "直接执行方案会话 `{}`；复用既有计划并开始落地“{}”",
                    truncate(&session.id, 18),
                    summarize_task(&session.task)
                )
            })
            .unwrap_or_else(|| "请先在历史页选中一个已完成方案，再执行此方案".to_string()),
        ShellAction::ContinueSelected => {
            let action_title = "继续优化";
            if selected_session.is_none() {
                format!("请先在历史页选中一个已完成 session，再{action_title}")
            } else if form.continue_feedback.trim().is_empty() {
                format!("直接基于当前 session {action_title}；系统会先重新规划，再继续执行。")
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
        ShellAction::ConfigValidate => "检查当前仓库配置、规则和角色集合是否有效".to_string(),
        ShellAction::AgentsList => "查看当前仓库可用角色、角色集合与来源信息".to_string(),
    }
}

pub(super) fn recent_task_history(project: &ProjectContext) -> Vec<String> {
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

pub(super) fn recent_task_history_lines(edit: &EditState) -> Vec<Line<'static>> {
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

pub(super) fn available_history_actions(
    selected_session: Option<&SessionManifest>,
) -> Vec<HistoryAction> {
    let Some(session) = selected_session else {
        return vec![HistoryAction::CleanAll, HistoryAction::BackToStart];
    };

    let mut actions = if session.is_plan_session() {
        vec![
            HistoryAction::ExecutePlan,
            HistoryAction::Replay,
            HistoryAction::Detail,
        ]
    } else {
        vec![
            HistoryAction::Continue,
            HistoryAction::EditFeedback,
            HistoryAction::ContinueMode,
            HistoryAction::ManualWrite,
            HistoryAction::Replay,
            HistoryAction::Detail,
        ]
    };
    if session.is_run_session() && !session_can_manual_write(session) {
        actions.retain(|action| *action != HistoryAction::ManualWrite);
    }
    if session.is_run_session() {
        actions.push(HistoryAction::ResetSelected);
    }
    actions.push(HistoryAction::CleanSelected);
    actions.push(HistoryAction::CleanAll);
    actions.push(HistoryAction::BackToStart);
    actions
}

pub(super) fn session_can_manual_write(session: &SessionManifest) -> bool {
    !session.wrote_to_target()
        && session
            .apply_result
            .as_ref()
            .is_some_and(|result| !result.accepted_files.is_empty())
}

pub(super) fn run_source_user_hint(form: &FormState) -> String {
    if !form.resume_session_id.trim().is_empty() {
        format!(
            "当前会恢复运行 session `{}`，不会走新规划。",
            truncate(&form.resume_session_id, 18)
        )
    } else {
        "当前会发起一次全新运行；系统会先规划，再自动执行并直接写入目标目录。".to_string()
    }
}

pub(super) fn action_supports_stop(
    action: ShellAction,
    _form: &FormState,
    selected_session: Option<&SessionManifest>,
) -> bool {
    match action {
        ShellAction::Plan
        | ShellAction::Run
        | ShellAction::ExecutePlanSelected
        | ShellAction::ReviewFixSelected
        | ShellAction::ReplaySelected => true,
        ShellAction::ContinueSelected => selected_session.is_some(),
        ShellAction::Doctor | ShellAction::ConfigValidate | ShellAction::AgentsList => false,
    }
}

pub(super) fn initial_command_output(
    action: ShellAction,
    preview: &CommandPreview,
    display_target: &str,
    supports_stop: bool,
) -> Vec<String> {
    let mut lines = vec![
        format!("准备动作：{}", preview.summary),
        format!("目标仓库：{display_target}"),
        match action {
            ShellAction::Plan => "本次只生成方案与执行图；不会写入目标目录。".to_string(),
            ShellAction::Doctor | ShellAction::ConfigValidate | ShellAction::AgentsList => {
                "这是只读检查动作；不会生成代码改动。".to_string()
            }
            _ => "系统工件会优先保留在 .codex-forge/；代码结果默认直接写入目标目录，自动失败时可改为手动写入。"
                .to_string(),
        },
        format!("命令预览：{}", truncate(&preview.commandline, 120)),
        "内嵌执行已启动，等待实时事件…".to_string(),
    ];
    if supports_stop {
        lines.push("如需中止，请回执行页按 `s` 发起安全停止。".to_string());
    }
    lines
}

pub(super) fn edit_mode_summary(field: FormField) -> &'static str {
    match field {
        FormField::Task => "支持多行输入，可直接保存或保存后定位到方案/执行动作。",
        FormField::ContinueFeedback => "支持多行输入，可直接保存或保存后定位到“继续优化”。",
        FormField::ReviewIssue => "支持多行输入，用来约束当前文件的返修目标。",
        _ => "单行编辑；改完可直接保存或退出。",
    }
}

pub(super) fn edit_shortcuts_lines(field: FormField) -> Vec<Line<'static>> {
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

pub(super) fn history_detail_shortcuts_lines() -> Vec<Line<'static>> {
    vec![
        Line::from("关闭：Esc / v"),
        Line::from("切页：Tab / ←→ / [ ]"),
        Line::from("滚动：↑↓ / j k"),
        Line::from("翻页：PgUp / PgDn / Home / End"),
        Line::from("左侧看用户摘要，右侧看技术细节"),
    ]
}

pub(super) fn confirm_shortcuts_lines(action: &ConfirmAction) -> Vec<Line<'static>> {
    let action_line = match action {
        ConfirmAction::ResetSelected { .. } => "确认：Enter（回退自动提交并删除对应历史）",
        ConfirmAction::CleanSelected { .. } => "确认：Enter（删除当前会话及其后续迭代）",
        ConfirmAction::CleanAll => "确认：Enter（清空当前仓库下全部 .codex-forge 历史）",
        ConfirmAction::Quit => "确认：Enter（退出当前 TUI）",
    };
    vec![Line::from(action_line), Line::from("取消：Esc")]
}

pub(super) fn command_elapsed_secs(command: &ActiveCommand) -> u64 {
    command
        .finished_at
        .unwrap_or_else(Instant::now)
        .duration_since(command.started_at)
        .as_secs()
}

pub(super) fn build_doctor_failure_summary(report: &DoctorReport) -> String {
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

pub(super) fn saved_field_notice(field: FormField) -> String {
    match field {
        FormField::Task => "内容已保存。现在请手动选择“先看方案”或“开始运行”。".to_string(),
        FormField::ContinueFeedback => "反馈已保存。现在请手动执行“继续优化”。".to_string(),
        FormField::ReviewIssue => "审查问题已保存。现在可以发起单文件返修。".to_string(),
        FormField::TargetDir | FormField::ConfigPath => {
            "内容已保存，项目上下文已刷新。".to_string()
        }
        _ => "内容已保存。".to_string(),
    }
}

pub(super) fn preferred_run_subview(action: ShellAction) -> RunSubview {
    match action {
        ShellAction::Plan
        | ShellAction::Run
        | ShellAction::ExecutePlanSelected
        | ShellAction::ContinueSelected
        | ShellAction::ReviewFixSelected
        | ShellAction::ReplaySelected => RunSubview::Timeline,
        ShellAction::Doctor | ShellAction::ConfigValidate | ShellAction::AgentsList => {
            RunSubview::Dashboard
        }
    }
}

pub(super) fn optional_path(value: &str) -> Option<&Path> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(Path::new(trimmed))
    }
}

pub(super) fn existing_deliverables(session: &SessionManifest) -> Vec<PathBuf> {
    repo_export_candidates(session)
        .into_iter()
        .filter(|path| path.exists())
        .collect()
}

pub(super) fn repo_export_candidates(session: &SessionManifest) -> Vec<PathBuf> {
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

pub(super) fn repo_export_path(session: &SessionManifest, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        session.repo_root().join(path)
    }
}

pub(super) fn repo_export_label(session: &SessionManifest, path: &Path) -> String {
    path.strip_prefix(session.repo_root())
        .map(|item| item.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

pub(super) fn append_repo_export_sections(sections: &mut Vec<String>, session: &SessionManifest) {
    let exports = repo_export_candidates(session);
    if exports.is_empty() {
        sections.push(format!(
            "===== Repo Exports =====\n\n当前会话尚未写入目标目录。\n\n目标目录状态：{} / {}",
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

pub(super) async fn deliver_session_accepted_files(
    target_dir: &Path,
    session_id: &str,
    destination: &Path,
) -> Result<Vec<String>> {
    let source_session = load_session(target_dir, Some(session_id))?;
    let accepted_files = source_session
        .apply_result
        .as_ref()
        .map(|result| result.accepted_files.clone())
        .unwrap_or_default();
    if accepted_files.is_empty() {
        anyhow::bail!("当前会话没有可手动写入的 accepted_files");
    }

    let clean = crate::worktree::git_is_clean(destination).await?;
    if !clean {
        anyhow::bail!("目标工作区存在未提交改动，拒绝执行手动写入");
    }

    let plan = load_apply_plan_for_session(&source_session)?;
    deliver_selected_files_from_plan(&plan, destination, &accepted_files).await
}

pub(super) fn process_artifact_paths(session: &SessionManifest) -> Vec<PathBuf> {
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

pub(super) fn existing_system_artifacts(session: &SessionManifest) -> Vec<PathBuf> {
    process_artifact_paths(session)
}

pub(super) fn can_open_manual_review(session: &SessionManifest) -> bool {
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

pub(super) fn load_apply_plan_for_session(session: &SessionManifest) -> Result<ApplyPlan> {
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

pub(super) fn persist_manual_delivery_result(
    target_dir: &Path,
    session_id: &str,
    result: ManualDeliveryResult,
) -> Result<()> {
    let mut session = load_session(target_dir, Some(session_id))?;
    set_manual_delivery_result_for_loaded_session(&mut session, result)
}

pub(super) fn persist_manual_review_state(
    target_dir: &Path,
    session_id: &str,
    state: ManualReviewState,
) -> Result<()> {
    let mut session = load_session(target_dir, Some(session_id))?;
    set_manual_review_state_for_loaded_session(&mut session, state)
}

pub(super) fn load_or_initialize_manual_review_state(
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

pub(super) fn build_initial_manual_review_state(session: &SessionManifest) -> ManualReviewState {
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

pub(super) fn build_manual_review_file_record(
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

pub(super) fn delivery_status_label(session: &SessionManifest) -> &'static str {
    if session.is_plan_session() {
        return "未执行";
    }
    if session.wrote_to_target() {
        "已写入"
    } else {
        "未写入"
    }
}

pub(super) fn delivery_status_detail(session: &SessionManifest) -> String {
    if session.is_plan_session() {
        return "这是方案会话；尚未进入代码落地阶段，可在历史页执行此方案。".to_string();
    }
    if let Some(result) = &session.manual_delivery_result {
        if result.success {
            return format!(
                "已手动将 {} 个 accepted_files 写入目标目录。",
                result.delivered_files.len()
            );
        }
        return format!(
            "上次手动写入失败：{}",
            result.error.as_deref().unwrap_or("未知错误")
        );
    }

    if let Some(apply_result) = &session.apply_result {
        if apply_result.synced_to_target && matches!(apply_result.status, ApplyStatus::Applied) {
            return "auto-safe 已自动同步到目标目录。".to_string();
        }
        if apply_result.wrote_to_target {
            return format!(
                "代码已写入目标目录 / apply：{}",
                apply_result.status.label()
            );
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

    "当前没有可用写入记录。".to_string()
}

pub(super) fn build_manual_review_detail_text(
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

pub(super) fn render_file_diff_for_session(session: &SessionManifest, file: &str) -> String {
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

pub(super) fn extract_file_diff_from_patch(patch: &str, file: &str) -> Option<String> {
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

pub(super) fn collect_changed_files(manifest: &SessionManifest) -> Vec<String> {
    let mut files = manifest
        .worker_results
        .iter()
        .flat_map(|result| result.changed_files.iter().cloned())
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    files
}

pub(super) fn record_review_fix_completion(
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

pub(super) async fn deliver_manual_review_approved_files(
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
