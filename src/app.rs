use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::app_shell::run_app_shell;
use crate::cli::{
    AgentCommands, AgentListArgs, ApplyModeArg, Cli, Commands, ConfigCommands, ConfigValidateArgs,
    DoctorArgs, PlanArgs, PresetArg, ReplayArgs, RunArgs, ThinkingModeArg, TuiArgs, UiModeArg,
};
use crate::config::{load_project_config, validate_project_config};
use crate::doctor::run_doctor;
use crate::model::{ApplyMode, SessionConfig, SessionPreset, ThinkingMode, UiMode};
use crate::orchestrator::{plan_session, run_session};
use crate::replay::replay_session;
use crate::resources::{load_resource_catalog, resolve_role_set};
use crate::workspace::{describe_git_readiness, resolve_target_dir};

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => run_tui(TuiArgs { target_dir: None }).await?,
        Some(Commands::Tui(args)) => run_tui(args).await?,
        Some(Commands::Run(args)) => {
            let (config, roles) = resolve_run_config(args)?;
            let _manifest = run_session(config, roles).await?;
        }
        Some(Commands::Plan(args)) => {
            if args.config_only {
                run_config_validate(ConfigValidateArgs {
                    target_dir: args.shared.target_dir,
                    config: args.shared.config,
                })?;
            } else {
                let (config, roles) = resolve_plan_config(args)?;
                let _manifest = plan_session(config, roles).await?;
            }
        }
        Some(Commands::Replay(args)) => run_replay(args).await?,
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

pub(crate) fn resolve_run_config(
    args: RunArgs,
) -> Result<(SessionConfig, Vec<crate::model::RoleConfig>)> {
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
        task: args.shared.task,
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
        resume_session_id: args.resume,
    };
    Ok((apply_run_preset(config), roles))
}

pub(crate) fn resolve_plan_config(
    args: PlanArgs,
) -> Result<(SessionConfig, Vec<crate::model::RoleConfig>)> {
    let target_dir = resolve_target_dir(args.shared.target_dir.as_deref())?.path;
    let loaded = load_project_config(&target_dir, args.shared.config.as_deref())?;
    let resources = load_resource_catalog(&target_dir)?;
    let role_set = args
        .shared
        .role_set
        .clone()
        .unwrap_or_else(|| loaded.settings.role_set.clone());
    let roles = resolve_role_set(&resources, &role_set)?;

    let config = SessionConfig {
        task: args.shared.task,
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
        fail_fast: loaded.settings.fail_fast,
        verification_commands: loaded.settings.verification_commands.clone(),
        config_path: loaded.path,
        global_rule_prompt: resources.rules.global.clone(),
        reviewer_rule_prompt: resources.rules.reviewer.clone(),
        plan_only: true,
        preset: None,
        resume_session_id: None,
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
    println!("codex-forge v5 可用角色：");
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

fn into_ui_mode(mode: UiModeArg) -> UiMode {
    match mode {
        UiModeArg::Rich => UiMode::Rich,
        UiModeArg::Minimal => UiMode::Minimal,
    }
}

fn into_apply_mode(mode: ApplyModeArg) -> ApplyMode {
    match mode {
        ApplyModeArg::AutoSafe => ApplyMode::AutoSafe,
        ApplyModeArg::Bundle => ApplyMode::Bundle,
        ApplyModeArg::None => ApplyMode::None,
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
