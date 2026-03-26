use anyhow::{Result, bail};

use crate::cli::{ConfigArgs, ConfigCommands, ConfigSetArgs, ConfigTargetArgs};
use crate::config::{
    BackendProvider, init_default_config, init_global_config, load_global_config,
    load_project_config, set_global_backend_provider, validate_global_config,
    validate_project_config,
};
use crate::workspace::resolve_target_dir;

pub fn run(args: ConfigArgs) -> Result<()> {
    match args.command {
        ConfigCommands::Init(args) => run_init(args),
        ConfigCommands::Show(args) => run_show(args),
        ConfigCommands::Validate(args) => run_validate(args),
        ConfigCommands::Set(args) => run_set(args),
    }
}

fn run_init(args: ConfigTargetArgs) -> Result<()> {
    if args.global {
        let path = init_global_config()?;
        println!("已初始化全局配置：{}", path.display());
        return Ok(());
    }
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let path = init_default_config(&repo_root)?;
    println!("已初始化配置：{}", path.display());
    Ok(())
}

fn run_show(args: ConfigTargetArgs) -> Result<()> {
    if args.global {
        let loaded = load_global_config()?;
        println!(
            "{}",
            toml::to_string_pretty(&redacted_global_config(&loaded.value))?
        );
        return Ok(());
    }
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let loaded = load_project_config(&repo_root)?;
    println!("{}", toml::to_string_pretty(&loaded.value)?);
    Ok(())
}

fn run_validate(args: ConfigTargetArgs) -> Result<()> {
    if args.global {
        let loaded = load_global_config()?;
        validate_global_config(&loaded.value)?;
        println!("全局配置有效：{}", loaded.path.display());
        return Ok(());
    }
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let loaded = load_project_config(&repo_root)?;
    validate_project_config(&loaded.value)?;
    println!("配置有效：{}", loaded.path.display());
    Ok(())
}

fn run_set(args: ConfigSetArgs) -> Result<()> {
    if !args.global {
        bail!(
            "`config set` 目前仅支持 `--global`，会写入全局配置（默认 ~/.codex-forge/config.toml，也支持 CODEX_FORGE_HOME/config.toml）"
        );
    }
    if args.key != "backend.provider" {
        bail!("`config set` 目前仅支持 `backend.provider`");
    }
    let provider = BackendProvider::parse_config_value(&args.value)?;
    let loaded = set_global_backend_provider(provider)?;
    println!(
        "已更新全局 backend.provider = \"{}\"：{}",
        loaded.value.backend.provider.config_value(),
        loaded.path.display()
    );
    Ok(())
}

fn redacted_global_config(config: &crate::config::GlobalConfig) -> crate::config::GlobalConfig {
    let mut redacted = config.clone();
    if redacted.backend.key.is_some() {
        redacted.backend.key = Some("***".to_string());
    }
    redacted
}
