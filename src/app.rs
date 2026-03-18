use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::cli::{
    AgentCommands, ApplyModeArg, Cli, Commands, ConfigCommands, ConfigValidateArgs, DoctorArgs,
    PlanArgs, ReplayArgs, RunArgs, UiModeArg,
};
use crate::config::{load_project_config, validate_project_config};
use crate::doctor::run_doctor;
use crate::model::{ApplyMode, SessionConfig, UiMode};
use crate::orchestrator::{plan_session, run_session};
use crate::replay::replay_session;
use crate::roles::{agents_overview, resolve_role_set};

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run(args) => {
            let (config, roles) = resolve_run_config(args)?;
            let _manifest = run_session(config, roles).await?;
        }
        Commands::Plan(args) => {
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
        Commands::Replay(args) => run_replay(args).await?,
        Commands::Agents(args) => match args.command {
            AgentCommands::List => print_agents(),
        },
        Commands::Doctor(args) => run_doctor_command(args).await?,
        Commands::Config(args) => match args.command {
            ConfigCommands::Validate(validate_args) => run_config_validate(validate_args)?,
        },
    }
    Ok(())
}

fn resolve_run_config(args: RunArgs) -> Result<(SessionConfig, Vec<crate::model::RoleConfig>)> {
    let loaded = load_project_config(&args.shared.target_dir, args.shared.config.as_deref())?;
    let role_set = args
        .shared
        .role_set
        .clone()
        .unwrap_or_else(|| loaded.settings.role_set.clone());
    let roles = resolve_role_set(&role_set, &loaded.settings.role_overrides);
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
        ui_mode: into_ui_mode(args.shared.ui),
        target_dir: args.shared.target_dir,
        cleanup_success: args.cleanup_success || loaded.settings.cleanup_success,
        apply_mode: args
            .apply_mode
            .map(into_apply_mode)
            .unwrap_or(loaded.settings.apply_mode),
        max_retries: args.max_retries.unwrap_or(loaded.settings.max_retries),
        fail_fast: args.fail_fast || loaded.settings.fail_fast,
        verification_commands: loaded.settings.verification_commands.clone(),
        config_path: loaded.path,
        plan_only: false,
    };
    Ok((config, roles))
}

fn resolve_plan_config(args: PlanArgs) -> Result<(SessionConfig, Vec<crate::model::RoleConfig>)> {
    let loaded = load_project_config(&args.shared.target_dir, args.shared.config.as_deref())?;
    let role_set = args
        .shared
        .role_set
        .clone()
        .unwrap_or_else(|| loaded.settings.role_set.clone());
    let roles = resolve_role_set(&role_set, &loaded.settings.role_overrides);

    let config = SessionConfig {
        task: args.shared.task,
        workers: args
            .shared
            .workers
            .unwrap_or(loaded.settings.workers)
            .max(1),
        role_set,
        model: args.shared.model.or(loaded.settings.model.clone()),
        ui_mode: into_ui_mode(args.shared.ui),
        target_dir: args.shared.target_dir,
        cleanup_success: false,
        apply_mode: ApplyMode::None,
        max_retries: loaded.settings.max_retries,
        fail_fast: loaded.settings.fail_fast,
        verification_commands: loaded.settings.verification_commands.clone(),
        config_path: loaded.path,
        plan_only: true,
    };
    Ok((config, roles))
}

async fn run_replay(args: ReplayArgs) -> Result<()> {
    replay_session(
        &args.target_dir,
        args.session_id.as_deref(),
        into_ui_mode(args.ui),
    )
    .await
}

async fn run_doctor_command(args: DoctorArgs) -> Result<()> {
    let loaded = load_project_config(&args.target_dir, args.config.as_deref())?;
    let report = run_doctor(
        &args.target_dir,
        &loaded,
        args.apply_mode.map(into_apply_mode),
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

    if report.ok {
        println!("doctor 通过");
        Ok(())
    } else {
        bail!("doctor 检查未通过")
    }
}

fn run_config_validate(args: ConfigValidateArgs) -> Result<()> {
    let loaded = validate_project_config(&args.target_dir, args.config.as_deref())
        .with_context(|| "配置校验失败")?;
    println!(
        "配置有效：{}",
        loaded
            .path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "使用内置默认值".to_string())
    );
    println!("默认 workers：{}", loaded.settings.workers);
    println!("默认 apply_mode：{}", loaded.settings.apply_mode);
    println!("默认 max_retries：{}", loaded.settings.max_retries);
    Ok(())
}

fn print_agents() {
    println!("codex-forge 内置角色：");
    for role in agents_overview() {
        println!(
            "- {} (`{}`)：{} | skills: {} | can_edit: {}",
            role.title,
            role.key,
            role.mission,
            role.skills.join("、"),
            role.can_edit
        );
    }
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
