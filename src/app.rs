use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Parser;

use crate::app_shell::run_app_shell;
use crate::cli::{
    AgentCommands, AgentListArgs, ApplyModeArg, CleanArgs, Cli, Commands, ConfigCommands,
    ConfigValidateArgs, ContinueArgs, ContinueModeArg, DoctorArgs, PlanArgs, PresetArg, ReplayArgs,
    ResetArgs, RunArgs, ThinkingModeArg, TuiArgs, UiModeArg,
};
use crate::config::{load_project_config, validate_project_config};
use crate::doctor::run_doctor;
use crate::model::{
    ApplyMode, BaselineArtifacts, ContinuationConfig, ContinuationKind, FeedbackRecord,
    ReviewFixRequest, SessionConfig, SessionLineageEntry, SessionManifest, SessionPreset,
    ThinkingMode, UiMode,
};
use crate::orchestrator::run_session;
use crate::replay::replay_session;
use crate::resources::{load_resource_catalog, resolve_role_set};
use crate::session::{
    CleanupScope, cleanup_all_forge_artifacts, cleanup_session_lineage, load_session,
    reset_session_lineage,
};
use crate::workspace::{describe_git_readiness, resolve_target_dir};

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => run_tui(TuiArgs { target_dir: None }).await?,
        Some(Commands::Tui(args)) => run_tui(args).await?,
        Some(Commands::Plan(args)) => {
            let (config, roles) = resolve_plan_config(args)?;
            run_session(config, roles).await?;
        }
        Some(Commands::Run(args)) => {
            let (config, roles) = resolve_run_config(args)?;
            let manifest = run_session(config, roles).await?;
            ensure_run_delivered(&manifest)?;
        }
        Some(Commands::Continue(args)) => {
            let (config, roles) = resolve_continue_config(args)?;
            let manifest = run_session(config, roles).await?;
            ensure_run_delivered(&manifest)?;
        }
        Some(Commands::Replay(args)) => run_replay(args).await?,
        Some(Commands::Reset(args)) => run_reset_command(args)?,
        Some(Commands::Clean(args)) => run_clean_command(args)?,
        Some(Commands::Agents(args)) => match args.command {
            AgentCommands::List(list_args) => print_agents(list_args)?,
        },
        Some(Commands::Doctor(args)) => run_doctor_command(args).await?,
        Some(Commands::Config(args)) => match args.command {
            ConfigCommands::Validate(validate_args) => run_config_validate(validate_args)?,
        },
    }
    Ok(())
}

async fn run_tui(args: TuiArgs) -> Result<()> {
    let initial_target_dir = match args.target_dir {
        Some(path) => Some(resolve_target_dir(Some(path.as_path()))?.path),
        None => None,
    };
    run_app_shell(initial_target_dir).await
}

pub(crate) fn resolve_plan_config(
    args: PlanArgs,
) -> Result<(SessionConfig, Vec<crate::model::RoleConfig>)> {
    let task = validated_task_input(&args.shared.task)?;
    let target_dir = resolve_target_dir(args.shared.target_dir.as_deref())?.path;
    let loaded = load_project_config(&target_dir, args.shared.config.as_deref())?;
    let resources = load_resource_catalog(&target_dir)?;
    let role_set = args
        .shared
        .role_set
        .clone()
        .unwrap_or_else(|| loaded.settings.role_set.clone());
    let roles = resolve_role_set(&resources, &role_set)?;
    if roles.is_empty() {
        bail!("角色集合为空，无法规划");
    }

    let config = SessionConfig {
        task,
        workers: args
            .shared
            .workers
            .unwrap_or(loaded.settings.workers)
            .max(1),
        role_set,
        model: args.shared.model.or(loaded.settings.model.clone()),
        thinking_mode: args
            .shared
            .thinking_mode
            .map(into_thinking_mode)
            .unwrap_or(loaded.settings.thinking_mode),
        ui_mode: into_ui_mode(args.shared.ui),
        target_dir,
        cleanup_success: false,
        apply_mode: ApplyMode::None,
        max_retries: loaded.settings.max_retries,
        fail_fast: false,
        verification_commands: loaded.settings.verification_commands.clone(),
        config_path: loaded.path,
        global_rule_prompt: resources.rules.global.clone(),
        reviewer_rule_prompt: resources.rules.reviewer.clone(),
        plan_only: true,
        preset: None,
        source_plan_session_id: None,
        resume_session_id: None,
        continuation: None,
    };
    Ok((config, roles))
}

pub(crate) fn resolve_run_config(
    args: RunArgs,
) -> Result<(SessionConfig, Vec<crate::model::RoleConfig>)> {
    let task = validated_task_input(&args.shared.task)?;
    let target_dir = resolve_target_dir(args.shared.target_dir.as_deref())?.path;
    let loaded = load_project_config(&target_dir, args.shared.config.as_deref())?;
    let resources = load_resource_catalog(&target_dir)?;
    let role_set = args
        .shared
        .role_set
        .clone()
        .unwrap_or_else(|| loaded.settings.role_set.clone());
    let roles = resolve_role_set(&resources, &role_set)?;
    if roles.is_empty() {
        bail!("角色集合为空，无法运行");
    }

    let config = SessionConfig {
        task,
        workers: args
            .shared
            .workers
            .unwrap_or(loaded.settings.workers)
            .max(1),
        role_set,
        model: args.shared.model.or(loaded.settings.model.clone()),
        thinking_mode: args
            .shared
            .thinking_mode
            .map(into_thinking_mode)
            .unwrap_or(loaded.settings.thinking_mode),
        ui_mode: into_ui_mode(args.shared.ui),
        target_dir,
        cleanup_success: args.cleanup_success || loaded.settings.cleanup_success,
        apply_mode: args
            .apply_mode
            .map(into_apply_mode)
            .unwrap_or(loaded.settings.apply_mode),
        max_retries: args.max_retries.unwrap_or(loaded.settings.max_retries),
        fail_fast: args.fail_fast || loaded.settings.fail_fast,
        verification_commands: loaded.settings.verification_commands.clone(),
        config_path: loaded.path,
        global_rule_prompt: resources.rules.global.clone(),
        reviewer_rule_prompt: resources.rules.reviewer.clone(),
        plan_only: false,
        preset: args.preset.map(into_preset),
        source_plan_session_id: args.from_plan,
        resume_session_id: args.resume,
        continuation: None,
    };
    Ok((apply_run_preset(config), roles))
}

pub(crate) fn resolve_continue_config(
    args: ContinueArgs,
) -> Result<(SessionConfig, Vec<crate::model::RoleConfig>)> {
    let target_dir = resolve_target_dir(args.target_dir.as_deref())?.path;
    let parent_manifest = load_session(&target_dir, Some(&args.session))?;
    if !parent_manifest.continuable() {
        bail!("continue 只支持已完成 session；未完成任务请改用 run --resume");
    }

    let loaded = load_project_config(&target_dir, parent_manifest.config_path.as_deref())?;
    let resources = load_resource_catalog(&target_dir)?;
    let role_set = parent_manifest.role_set.clone();
    let roles = resolve_role_set(&resources, &role_set)?;
    if roles.is_empty() {
        bail!("角色集合为空，无法继续迭代");
    }

    let continuation_kind = resolve_continuation_kind(args.mode, &parent_manifest);
    let feedback = args.feedback.unwrap_or_default();
    let mut feedback_history = parent_manifest.feedback_history.clone();
    if !feedback.trim().is_empty() {
        feedback_history.push(build_feedback_record(&feedback, args.title.clone()));
    }

    let continuation = ContinuationConfig {
        parent_session_id: parent_manifest.id.clone(),
        root_session_id: parent_manifest.root_session_id_ref().to_string(),
        iteration_index: parent_manifest.iteration_index_value() + 1,
        kind: continuation_kind,
        title: args.title,
        feedback,
        feedback_history,
        baseline_artifacts: baseline_artifacts_from_manifest(&parent_manifest),
        parent_lineage: if parent_manifest.lineage.is_empty() {
            vec![SessionLineageEntry {
                session_id: parent_manifest.id.clone(),
                iteration_index: parent_manifest.iteration_index_value(),
                continuation_kind: parent_manifest.continuation_kind,
                status: parent_manifest.status,
                created_at: parent_manifest.created_at,
            }]
        } else {
            parent_manifest.lineage.clone()
        },
        parent_task: parent_manifest.task.clone(),
        parent_plan_summary: parent_manifest
            .plan_todo
            .as_ref()
            .map(|item| item.summary.clone()),
        parent_summary_overview: parent_manifest
            .final_summary
            .as_ref()
            .map(|item| item.overview.clone()),
        parent_recommended_next_action: parent_manifest
            .final_summary
            .as_ref()
            .map(|item| item.recommended_next_action.clone())
            .unwrap_or_default(),
        review_fix: None,
    };

    let config = SessionConfig {
        task: parent_manifest.task.clone(),
        workers: parent_manifest.workers_requested.max(1),
        role_set,
        model: parent_manifest
            .model
            .clone()
            .or(loaded.settings.model.clone()),
        thinking_mode: parent_manifest.thinking_mode,
        ui_mode: into_ui_mode(args.ui),
        target_dir,
        cleanup_success: parent_manifest.cleanup_success,
        apply_mode: if matches!(parent_manifest.apply_mode, ApplyMode::None) {
            loaded.settings.apply_mode
        } else {
            parent_manifest.apply_mode
        },
        max_retries: parent_manifest.max_retries.max(1),
        fail_fast: parent_manifest.fail_fast,
        verification_commands: if parent_manifest.verification_commands.is_empty() {
            loaded.settings.verification_commands.clone()
        } else {
            parent_manifest.verification_commands.clone()
        },
        config_path: loaded.path.or(parent_manifest.config_path.clone()),
        global_rule_prompt: resources.rules.global.clone(),
        reviewer_rule_prompt: resources.rules.reviewer.clone(),
        plan_only: false,
        preset: parent_manifest.preset,
        source_plan_session_id: None,
        resume_session_id: None,
        continuation: Some(continuation),
    };
    Ok((config, roles))
}

pub(crate) fn build_review_fix_config(
    target_dir: &std::path::Path,
    parent_manifest: &SessionManifest,
    target_file: &str,
    issue_summary: &str,
    ui_mode: UiMode,
) -> Result<(SessionConfig, Vec<crate::model::RoleConfig>)> {
    let loaded = load_project_config(target_dir, parent_manifest.config_path.as_deref())?;
    let resources = load_resource_catalog(target_dir)?;
    let role_set = parent_manifest.role_set.clone();
    let roles = resolve_role_set(&resources, &role_set)?;
    if roles.is_empty() {
        bail!("角色集合为空，无法继续迭代");
    }

    let mut feedback_history = parent_manifest.feedback_history.clone();
    feedback_history.push(build_feedback_record(
        issue_summary,
        Some(format!("人工审查返修 {}", target_file)),
    ));

    let continuation = ContinuationConfig {
        parent_session_id: parent_manifest.id.clone(),
        root_session_id: parent_manifest.root_session_id_ref().to_string(),
        iteration_index: parent_manifest.iteration_index_value() + 1,
        kind: ContinuationKind::RunRefine,
        title: Some(format!("人工审查返修 {}", target_file)),
        feedback: issue_summary.to_string(),
        feedback_history,
        baseline_artifacts: baseline_artifacts_from_manifest(parent_manifest),
        parent_lineage: if parent_manifest.lineage.is_empty() {
            vec![SessionLineageEntry {
                session_id: parent_manifest.id.clone(),
                iteration_index: parent_manifest.iteration_index_value(),
                continuation_kind: parent_manifest.continuation_kind,
                status: parent_manifest.status,
                created_at: parent_manifest.created_at,
            }]
        } else {
            parent_manifest.lineage.clone()
        },
        parent_task: parent_manifest.task.clone(),
        parent_plan_summary: parent_manifest
            .plan_todo
            .as_ref()
            .map(|item| item.summary.clone()),
        parent_summary_overview: parent_manifest
            .final_summary
            .as_ref()
            .map(|item| item.overview.clone()),
        parent_recommended_next_action: parent_manifest
            .final_summary
            .as_ref()
            .map(|item| item.recommended_next_action.clone())
            .unwrap_or_default(),
        review_fix: Some(ReviewFixRequest {
            target_file: target_file.to_string(),
            issue_summary: issue_summary.to_string(),
        }),
    };

    let config = SessionConfig {
        task: format!("修复人工审查文件 `{}`：{}", target_file, issue_summary),
        workers: parent_manifest.workers_requested.max(1),
        role_set,
        model: parent_manifest
            .model
            .clone()
            .or(loaded.settings.model.clone()),
        thinking_mode: parent_manifest.thinking_mode,
        ui_mode,
        target_dir: target_dir.to_path_buf(),
        cleanup_success: false,
        apply_mode: ApplyMode::None,
        max_retries: parent_manifest.max_retries.max(1),
        fail_fast: parent_manifest.fail_fast,
        verification_commands: if parent_manifest.verification_commands.is_empty() {
            loaded.settings.verification_commands.clone()
        } else {
            parent_manifest.verification_commands.clone()
        },
        config_path: loaded.path.or(parent_manifest.config_path.clone()),
        global_rule_prompt: resources.rules.global.clone(),
        reviewer_rule_prompt: resources.rules.reviewer.clone(),
        plan_only: false,
        preset: parent_manifest.preset,
        source_plan_session_id: None,
        resume_session_id: None,
        continuation: Some(continuation),
    };
    Ok((config, roles))
}

async fn run_replay(args: ReplayArgs) -> Result<()> {
    let target_dir = resolve_target_dir(args.target_dir.as_deref())?.path;
    replay_session(
        &target_dir,
        args.session_id.as_deref(),
        into_ui_mode(args.ui),
        args.timeline,
    )
    .await
}

async fn run_doctor_command(args: DoctorArgs) -> Result<()> {
    let target_dir = resolve_target_dir(args.target_dir.as_deref())?.path;
    let loaded = load_project_config(&target_dir, args.config.as_deref())?;
    let resources = load_resource_catalog(&target_dir)?;
    let report = run_doctor(
        &target_dir,
        &loaded,
        &resources,
        args.apply_mode.map(into_apply_mode),
        args.demo,
    )
    .await?;

    for check in &report.checks {
        println!(
            "[{}] {} - {}",
            check.status.label(),
            check.name,
            check.detail
        );
    }

    println!(
        "doctor 结论：{} / {}",
        report.readiness.label(),
        report.summary
    );
    if args.demo {
        println!(
            "推荐 role_set：{}；推荐 apply_mode：{}",
            report.recommended_role_set, report.recommended_apply_mode
        );
    }

    if report.ok {
        println!("Git 预处理：{}", describe_git_readiness(&target_dir)?);
        println!("doctor 通过");
        Ok(())
    } else {
        bail!("doctor 检查未通过")
    }
}

fn run_clean_command(args: CleanArgs) -> Result<()> {
    let target_dir = resolve_target_dir(args.target_dir.as_deref())?.path;
    let report = if args.all {
        cleanup_all_forge_artifacts(&target_dir)?
    } else {
        cleanup_session_lineage(
            &target_dir,
            args.session
                .as_deref()
                .context("clean 需要指定 --session 或 --all")?,
        )?
    };

    match report.scope {
        CleanupScope::AllArtifacts => {
            if !report.had_artifacts {
                println!(
                    "当前仓库下没有可清理的 .codex-forge 产物：{}",
                    report.repo_root.display()
                );
            } else {
                println!("已清空：{}", report.forge_dir.display());
                println!("删除 session 数：{}", report.removed_sessions.len());
            }
        }
        CleanupScope::SessionCascade => {
            println!("已清理指定历史及其后续迭代。");
            println!("删除 session 数：{}", report.removed_sessions.len());
            println!("目标仓库：{}", report.repo_root.display());
        }
    }

    if !report.removed_sessions.is_empty() {
        println!("删除的 session：{}", report.removed_sessions.join("、"));
    }

    Ok(())
}

fn run_reset_command(args: ResetArgs) -> Result<()> {
    let target_dir = resolve_target_dir(args.target_dir.as_deref())?.path;
    let report = reset_session_lineage(&target_dir, &args.session)?;

    if let Some(reset_to) = &report.reset_to {
        println!(
            "已回滚 {} 个 commit，仓库重置到：{}",
            report.reset_commits.len(),
            reset_to
        );
    } else {
        println!("目标 session 没有落地 commit，仅清理历史痕迹。");
    }
    println!("删除 session 数：{}", report.removed_sessions.len());
    if !report.removed_sessions.is_empty() {
        println!("删除的 session：{}", report.removed_sessions.join("、"));
    }

    Ok(())
}

fn run_config_validate(args: ConfigValidateArgs) -> Result<()> {
    let target_dir = resolve_target_dir(args.target_dir.as_deref())?.path;
    let loaded = validate_project_config(&target_dir, args.config.as_deref())
        .with_context(|| "配置校验失败")?;
    let resources = load_resource_catalog(&target_dir).with_context(|| "资源校验失败")?;
    let roles = resolve_role_set(&resources, &loaded.settings.role_set)
        .with_context(|| "角色集合校验失败")?;
    println!(
        "配置有效：{}",
        loaded
            .path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "使用内置默认值".to_string())
    );
    println!("默认 workers：{}", loaded.settings.workers);
    println!("默认 thinking_mode：{}", loaded.settings.thinking_mode);
    println!("默认 apply_mode：{}", loaded.settings.apply_mode);
    println!("默认 max_retries：{}", loaded.settings.max_retries);
    println!("默认 role_set：{}", loaded.settings.role_set);
    println!("命中全局规则：{}", resources.rules.global_origin.describe());
    if let Some(origin) = &resources.rules.reviewer_origin {
        println!("命中 reviewer 规则：{}", origin.describe());
    }
    println!("角色集合节点数：{}", roles.len());
    Ok(())
}

fn print_agents(args: AgentListArgs) -> Result<()> {
    let target_dir = resolve_target_dir(args.target_dir.as_deref())?.path;
    let resources = load_resource_catalog(&target_dir)?;
    println!("codex-forge v6 可用角色：");
    let mut role_items = resources.roles.values().collect::<Vec<_>>();
    role_items.sort_by(|left, right| left.role.key.cmp(&right.role.key));
    for role in role_items {
        println!(
            "- {} (`{}`)：{} | skills: {} | can_edit: {} | source: {}",
            role.role.title,
            role.role.key,
            role.role.mission,
            role.role.skills.join("、"),
            role.role.can_edit,
            role.origin.describe()
        );
    }
    println!("角色集合：");
    let mut role_sets = resources.role_sets.iter().collect::<Vec<_>>();
    role_sets.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (name, role_set) in role_sets {
        println!(
            "- {}：{} | source: {}",
            name,
            role_set.roles.join("、"),
            role_set.origin.describe()
        );
    }
    Ok(())
}

fn validated_task_input(task: &str) -> Result<String> {
    let trimmed = task.trim();
    if trimmed.is_empty() {
        bail!("任务描述不能为空；请先输入提示词，再执行 plan 或 run")
    }
    Ok(trimmed.to_string())
}

fn ensure_run_delivered(manifest: &SessionManifest) -> Result<()> {
    if manifest.wrote_to_target() {
        return Ok(());
    }

    let apply_result = manifest.apply_result.as_ref();
    let review_gate = apply_result
        .and_then(|item| item.review_gate)
        .map(|item| item.label().to_string())
        .unwrap_or_else(|| "无".to_string());
    let apply_status = apply_result
        .map(|item| item.status.label().to_string())
        .unwrap_or_else(|| "无".to_string());
    let bundle_path = apply_result
        .and_then(|item| item.bundle_dir.as_ref())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "无".to_string());
    let accepted_files = apply_result
        .map(|item| item.accepted_files.len())
        .unwrap_or(0);

    bail!(
        "代码未交付到目标目录：{}\nreview gate：{}\napply 状态：{}\nbundle 路径：{}\naccepted_files：{}\n当前版本已关闭人工审核与手动交付；请根据 apply / verify 结果处理失败原因。",
        manifest.repo_root().display(),
        review_gate,
        apply_status,
        bundle_path,
        accepted_files
    )
}

fn into_ui_mode(mode: UiModeArg) -> UiMode {
    match mode {
        UiModeArg::Rich => UiMode::Rich,
        UiModeArg::Minimal => UiMode::Minimal,
    }
}

fn into_apply_mode(mode: ApplyModeArg) -> ApplyMode {
    match mode {
        ApplyModeArg::InPlace => ApplyMode::InPlace,
        ApplyModeArg::AutoSafe => ApplyMode::AutoSafe,
        ApplyModeArg::Bundle => ApplyMode::Bundle,
        ApplyModeArg::None => ApplyMode::None,
    }
}

fn resolve_continuation_kind(
    mode: ContinueModeArg,
    _parent_manifest: &SessionManifest,
) -> ContinuationKind {
    match mode {
        ContinueModeArg::Run => ContinuationKind::RunRefine,
        ContinueModeArg::Auto => ContinuationKind::RunRefine,
    }
}

fn build_feedback_record(feedback: &str, title: Option<String>) -> FeedbackRecord {
    let trimmed = feedback.trim();
    let intent_summary = trimmed.chars().take(80).collect::<String>();
    FeedbackRecord {
        author: "human".to_string(),
        title,
        raw_feedback: trimmed.to_string(),
        intent_summary: if intent_summary.is_empty() {
            "补充一轮反馈".to_string()
        } else {
            intent_summary
        },
        scope_delta: vec![trimmed.to_string()],
        accepted_assumptions: vec![
            "默认继承上一轮的角色集合、worker 数和仓库规则。".to_string(),
            "默认把本轮结果视为可继续反馈的阶段性交付。".to_string(),
        ],
        created_at: Utc::now(),
    }
}

fn baseline_artifacts_from_manifest(manifest: &SessionManifest) -> BaselineArtifacts {
    BaselineArtifacts {
        parent_plan_todo_path: manifest.artifact_manifest.plan_todo_path.clone(),
        parent_summary_markdown_path: Some(manifest.summary_markdown_path.clone()),
        parent_summary_json_path: Some(manifest.summary_json_path.clone()),
        parent_apply_result_path: Some(manifest.apply_result_path.clone()),
        parent_verification_report_path: Some(manifest.verification_report_path.clone()),
    }
}

fn into_thinking_mode(mode: ThinkingModeArg) -> ThinkingMode {
    match mode {
        ThinkingModeArg::Quick => ThinkingMode::Quick,
        ThinkingModeArg::Balanced => ThinkingMode::Balanced,
        ThinkingModeArg::HardThink => ThinkingMode::HardThink,
    }
}

fn into_preset(preset: PresetArg) -> SessionPreset {
    match preset {
        PresetArg::FeatureDemo => SessionPreset::FeatureDemo,
    }
}

fn apply_run_preset(mut config: SessionConfig) -> SessionConfig {
    match config.preset {
        Some(SessionPreset::FeatureDemo) => {
            config.workers = config.workers.max(4);
            config.ui_mode = UiMode::Rich;
            if config.role_set == "default" || config.role_set.is_empty() {
                config.role_set = "default".to_string();
            }
            if matches!(config.apply_mode, ApplyMode::None) {
                config.apply_mode = ApplyMode::AutoSafe;
            }
            config.cleanup_success = true;
        }
        None => {}
    }
    config
}

#[cfg(test)]
mod tests {
    use super::validated_task_input;

    #[test]
    fn rejects_empty_task_input() {
        let error = validated_task_input("   \n\t").expect_err("should reject empty task");
        assert!(error.to_string().contains("任务描述不能为空"));
    }

    #[test]
    fn trims_valid_task_input() {
        let task = validated_task_input("  修复 TUI 交互  ").expect("valid task");
        assert_eq!(task, "修复 TUI 交互");
    }
}
