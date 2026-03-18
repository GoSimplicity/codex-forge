use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "codex-forge",
    version,
    about = "一个多 Agent Codex 指挥台 CLI，支持显式执行图、自动收敛与验证"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Run(RunArgs),
    Plan(PlanArgs),
    Replay(ReplayArgs),
    Agents(AgentsArgs),
    Doctor(DoctorArgs),
    Config(ConfigArgs),
}

#[derive(Debug, Clone, Args)]
pub struct SharedTaskArgs {
    #[arg(help = "要交给多 Agent 团队完成的任务描述")]
    pub task: String,
    #[arg(
        long,
        help = "项目配置文件路径；默认优先读取目标仓库下的 codex-forge.toml"
    )]
    pub config: Option<PathBuf>,
    #[arg(long, help = "并发 worker 数量；不传时走配置文件或默认值")]
    pub workers: Option<usize>,
    #[arg(long, help = "角色模板集合；默认 core")]
    pub role_set: Option<String>,
    #[arg(long, help = "统一指定 Codex model")]
    pub model: Option<String>,
    #[arg(long, value_enum, default_value_t = UiModeArg::Rich, help = "终端展示模式")]
    pub ui: UiModeArg,
    #[arg(long, default_value = ".", help = "要协同开发的目标仓库目录")]
    pub target_dir: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub shared: SharedTaskArgs,
    #[arg(long, value_enum, help = "结果应用模式：auto-safe、bundle、none")]
    pub apply_mode: Option<ApplyModeArg>,
    #[arg(long, help = "worker 最大重试次数")]
    pub max_retries: Option<usize>,
    #[arg(long, help = "任一节点失败后尽快停止调度")]
    pub fail_fast: bool,
    #[arg(long, help = "成功完成后自动清理 worker worktree")]
    pub cleanup_success: bool,
}

#[derive(Debug, Clone, Args)]
pub struct PlanArgs {
    #[command(flatten)]
    pub shared: SharedTaskArgs,
    #[arg(long, help = "使用指定配置文件做纯规划校验")]
    pub config_only: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ReplayArgs {
    #[arg(help = "要回放的 session id；不传则回放最新一次")]
    pub session_id: Option<String>,
    #[arg(long, value_enum, default_value_t = UiModeArg::Rich, help = "回放时的终端展示模式")]
    pub ui: UiModeArg,
    #[arg(
        long,
        default_value = ".",
        help = "仓库目录，用于定位 .codex-forge 会话"
    )]
    pub target_dir: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct AgentsArgs {
    #[command(subcommand)]
    pub command: AgentCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum AgentCommands {
    List,
}

#[derive(Debug, Clone, Args)]
pub struct DoctorArgs {
    #[arg(long, default_value = ".", help = "要检查的目标仓库目录")]
    pub target_dir: PathBuf,
    #[arg(long, help = "项目配置文件路径；默认尝试读取 codex-forge.toml")]
    pub config: Option<PathBuf>,
    #[arg(long, value_enum, help = "覆盖 apply mode，便于提前检查运行条件")]
    pub apply_mode: Option<ApplyModeArg>,
}

#[derive(Debug, Clone, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ConfigCommands {
    Validate(ConfigValidateArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ConfigValidateArgs {
    #[arg(long, default_value = ".", help = "目标仓库目录")]
    pub target_dir: PathBuf,
    #[arg(long, help = "显式指定配置文件路径")]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UiModeArg {
    Rich,
    Minimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ApplyModeArg {
    #[value(name = "auto-safe")]
    AutoSafe,
    Bundle,
    None,
}
