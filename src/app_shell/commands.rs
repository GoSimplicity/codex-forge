use super::*;

#[derive(Debug)]
pub(super) struct CommandPreview {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) args: Vec<String>,
    pub(super) commandline: String,
    pub(super) summary: String,
}

pub(super) fn build_command_preview(
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
        ShellAction::ExecutePlanSelected => {
            let task = selected_session
                .map(|session| session.task.clone())
                .unwrap_or_else(|| form.task.clone());
            args.push("run".to_string());
            args.push(task);
            append_shared_args(&mut args, target_dir, form);
            args.push("--ui".to_string());
            args.push("minimal".to_string());
            args.push("--apply-mode".to_string());
            args.push(form.apply_mode.label().to_string());
            args.push("--max-retries".to_string());
            args.push(form.max_retries.clone());
            if let Some(session) = selected_session {
                args.push("--from-plan".to_string());
                args.push(session.id.clone());
            }
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
        ShellAction::ConfigValidate => {
            args.push("config".to_string());
            args.push("validate".to_string());
            args.push("--target-dir".to_string());
            args.push(target_dir.display().to_string());
            if let Some(path) = optional_path(&form.config_path) {
                args.push("--config".to_string());
                args.push(path.display().to_string());
            }
        }
        ShellAction::AgentsList => {
            args.push("agents".to_string());
            args.push("list".to_string());
            args.push("--target-dir".to_string());
            args.push(target_dir.display().to_string());
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

pub(super) fn prepare_runtime_state(
    action: ShellAction,
    form: &FormState,
    selected_session: Option<&SessionManifest>,
) -> Option<RuntimeViewState> {
    match action {
        ShellAction::Doctor => None,
        ShellAction::Plan => Some(RuntimeViewState::new("准备中", &form.task)),
        ShellAction::Run => Some(RuntimeViewState::new("准备中", &form.task)),
        ShellAction::ExecutePlanSelected => {
            selected_session.map(|session| RuntimeViewState::new(&session.id, &session.task))
        }
        ShellAction::ContinueSelected => {
            selected_session.map(|session| RuntimeViewState::new(&session.id, &session.task))
        }
        ShellAction::ReviewFixSelected => selected_session.map(|session| {
            RuntimeViewState::new(&session.id, &format!("人工审查返修 {}", session.task))
        }),
        ShellAction::ReplaySelected => {
            selected_session.map(|session| RuntimeViewState::new(&session.id, &session.task))
        }
        ShellAction::ConfigValidate | ShellAction::AgentsList => None,
    }
}

/// 统一裁剪日志缓存，避免执行页无限增长导致 TUI 卡顿。
pub(super) fn push_command_output(output: &mut Vec<String>, line: String) {
    output.push(line);
    if output.len() > MAX_LOG_LINES {
        let overflow = output.len() - MAX_LOG_LINES;
        output.drain(0..overflow);
    }
}

pub(super) fn spawn_embedded_action(
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
            ShellAction::Plan => run_plan_embedded(&target_dir, &form, tx.clone(), stop_rx).await,
            ShellAction::Run => run_run_embedded(&target_dir, &form, tx.clone(), stop_rx).await,
            ShellAction::ExecutePlanSelected => {
                run_execute_plan_embedded(&target_dir, &form, selected_session, tx.clone(), stop_rx)
                    .await
            }
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
            ShellAction::ConfigValidate => {
                run_config_validate_embedded(&target_dir, &form, tx.clone()).await
            }
            ShellAction::AgentsList => run_agents_list_embedded(&target_dir, tx.clone()).await,
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

pub(super) fn spawn_review_fix_action(
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

pub(super) fn runtime_tx(
    tx: &mpsc::UnboundedSender<RunnerEvent>,
) -> mpsc::UnboundedSender<RuntimeEvent> {
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

pub(super) async fn run_doctor_embedded(
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

pub(super) async fn run_run_embedded(
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

pub(super) async fn run_plan_embedded(
    target_dir: &Path,
    form: &FormState,
    tx: mpsc::UnboundedSender<RunnerEvent>,
    stop_rx: Option<watch::Receiver<bool>>,
) -> Result<(CommandState, Option<SessionManifest>)> {
    let args = build_plan_args(target_dir, form);
    let (config, roles) = resolve_plan_config(args)?;
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

pub(super) async fn run_continue_embedded(
    target_dir: &Path,
    form: &FormState,
    selected_session: Option<SessionManifest>,
    tx: mpsc::UnboundedSender<RunnerEvent>,
    stop_rx: Option<watch::Receiver<bool>>,
) -> Result<(CommandState, Option<SessionManifest>)> {
    let args = build_continue_args(target_dir, form, selected_session.as_ref())?;
    let (config, roles) = resolve_continue_config(args)?;
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

pub(super) async fn run_execute_plan_embedded(
    target_dir: &Path,
    form: &FormState,
    selected_session: Option<SessionManifest>,
    tx: mpsc::UnboundedSender<RunnerEvent>,
    stop_rx: Option<watch::Receiver<bool>>,
) -> Result<(CommandState, Option<SessionManifest>)> {
    let args = build_execute_plan_args(target_dir, form, selected_session.as_ref())?;
    let (config, roles) = resolve_run_config(args)?;
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

pub(super) async fn run_review_fix_embedded(
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

pub(super) async fn run_config_validate_embedded(
    target_dir: &Path,
    form: &FormState,
    tx: mpsc::UnboundedSender<RunnerEvent>,
) -> Result<(CommandState, Option<SessionManifest>)> {
    let loaded = validate_project_config(target_dir, optional_path(&form.config_path))
        .with_context(|| "配置校验失败")?;
    let resources = load_resource_catalog(target_dir).with_context(|| "资源校验失败")?;
    let roles = resolve_role_set(&resources, &loaded.settings.role_set)
        .with_context(|| "角色集合校验失败")?;
    for line in [
        format!(
            "配置有效：{}",
            loaded
                .path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "使用内置默认值".to_string())
        ),
        format!("默认 workers：{}", loaded.settings.workers),
        format!("默认 thinking_mode：{}", loaded.settings.thinking_mode),
        format!("默认 apply_mode：{}", loaded.settings.apply_mode),
        format!("默认 max_retries：{}", loaded.settings.max_retries),
        format!("默认 role_set：{}", loaded.settings.role_set),
        format!("命中全局规则：{}", resources.rules.global_origin.describe()),
    ] {
        let _ = tx.send(RunnerEvent::Line(line));
    }
    if let Some(origin) = &resources.rules.reviewer_origin {
        let _ = tx.send(RunnerEvent::Line(format!(
            "命中 reviewer 规则：{}",
            origin.describe()
        )));
    }
    let _ = tx.send(RunnerEvent::Line(format!(
        "角色集合节点数：{}",
        roles.len()
    )));
    Ok((CommandState::Succeeded, None))
}

pub(super) async fn run_agents_list_embedded(
    target_dir: &Path,
    tx: mpsc::UnboundedSender<RunnerEvent>,
) -> Result<(CommandState, Option<SessionManifest>)> {
    let resources = load_resource_catalog(target_dir)?;
    let _ = tx.send(RunnerEvent::Line("codex-forge v6 可用角色：".to_string()));
    let mut role_items = resources.roles.values().collect::<Vec<_>>();
    role_items.sort_by(|left, right| left.role.key.cmp(&right.role.key));
    for role in role_items {
        let _ = tx.send(RunnerEvent::Line(format!(
            "- {} (`{}`)：{} | skills: {} | can_edit: {} | source: {}",
            role.role.title,
            role.role.key,
            role.role.mission,
            role.role.skills.join("、"),
            role.role.can_edit,
            role.origin.describe()
        )));
    }
    let _ = tx.send(RunnerEvent::Line("角色集合：".to_string()));
    let mut role_sets = resources.role_sets.iter().collect::<Vec<_>>();
    role_sets.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (name, role_set) in role_sets {
        let _ = tx.send(RunnerEvent::Line(format!(
            "- {}：{} | source: {}",
            name,
            role_set.roles.join("、"),
            role_set.origin.describe()
        )));
    }
    Ok((CommandState::Succeeded, None))
}

pub(super) fn build_plan_args(target_dir: &Path, form: &FormState) -> PlanArgs {
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
    }
}

pub(super) fn build_run_args(target_dir: &Path, form: &FormState) -> RunArgs {
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
        from_plan: None,
        apply_mode: Some(match form.apply_mode {
            ApplyMode::InPlace => ApplyModeArg::InPlace,
            ApplyMode::AutoSafe => ApplyModeArg::AutoSafe,
            ApplyMode::Bundle => ApplyModeArg::Bundle,
            ApplyMode::None => ApplyModeArg::None,
        }),
        max_retries: parse_usize(&form.max_retries),
        fail_fast: form.fail_fast,
        cleanup_success: form.cleanup_success,
    }
}

pub(super) fn build_execute_plan_args(
    target_dir: &Path,
    form: &FormState,
    selected_session: Option<&SessionManifest>,
) -> Result<RunArgs> {
    let session = selected_session.context("执行方案需要先选中一个 plan session")?;
    Ok(RunArgs {
        shared: SharedTaskArgs {
            task: session.task.clone(),
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
        resume: None,
        from_plan: Some(session.id.clone()),
        apply_mode: Some(match form.apply_mode {
            ApplyMode::InPlace => ApplyModeArg::InPlace,
            ApplyMode::AutoSafe => ApplyModeArg::AutoSafe,
            ApplyMode::Bundle => ApplyModeArg::Bundle,
            ApplyMode::None => ApplyModeArg::None,
        }),
        max_retries: parse_usize(&form.max_retries),
        fail_fast: form.fail_fast,
        cleanup_success: form.cleanup_success,
    })
}

pub(super) fn build_continue_args(
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
