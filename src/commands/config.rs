use anyhow::{Result, bail};

use crate::cli::{ConfigArgs, ConfigCommands, ConfigTargetArgs};
use crate::config::{init_default_config, load_project_config};
use crate::workspace::resolve_target_dir;

pub fn run(args: ConfigArgs) -> Result<()> {
    match args.command {
        ConfigCommands::Init(args) => run_init(args),
        ConfigCommands::Show(args) => run_show(args),
        ConfigCommands::Validate(args) => run_validate(args),
    }
}

fn run_init(args: ConfigTargetArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let path = init_default_config(&repo_root)?;
    println!("已初始化配置：{}", path.display());
    Ok(())
}

fn run_show(args: ConfigTargetArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let loaded = load_project_config(&repo_root)?;
    println!("{}", toml::to_string_pretty(&loaded.value)?);
    Ok(())
}

fn run_validate(args: ConfigTargetArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let loaded = load_project_config(&repo_root)?;
    if loaded.value.runtime.max_turns == 0 {
        bail!("runtime.max_turns 必须大于 0");
    }
    if loaded.value.runtime.max_feature_retries == 0 {
        bail!("runtime.max_feature_retries 必须大于 0");
    }
    if loaded.value.runtime.max_evaluator_loops == 0 {
        bail!("runtime.max_evaluator_loops 必须大于 0");
    }
    if loaded.value.runtime.bootstrap_message_limit == 0 {
        bail!("runtime.bootstrap_message_limit 必须大于 0");
    }
    if loaded.value.backend.turn_timeout_secs == 0 {
        bail!("backend.turn_timeout_secs 必须大于 0");
    }
    if loaded.value.sandbox.docker_image.trim().is_empty() {
        bail!("sandbox.docker_image 不能为空");
    }
    println!("配置有效：{}", loaded.path.display());
    Ok(())
}
