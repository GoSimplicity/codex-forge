use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "codex-forge",
    version,
    about = "一个多 Agent 协作终端，默认低负担启动，高级参数按需展开"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Tui(TuiArgs),
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
        help = "高级参数：项目配置文件路径；默认优先读取目标仓库下的 codex-forge.toml"
    )]
    pub config: Option<PathBuf>,
    #[arg(long, help = "高级参数：并发 worker 数量；通常不需要手动指定")]
    pub workers: Option<usize>,
    #[arg(
        long,
        help = "高级参数：角色集合标识；默认读取 `.roles/sets.toml` 中的 default"
    )]
    pub role_set: Option<String>,
    #[arg(long, help = "高级参数：统一指定 Codex model")]
    pub model: Option<String>,
    #[arg(long, value_enum, help = "任务强度模式：quick、balanced、hard-think")]
    pub thinking_mode: Option<ThinkingModeArg>,
    #[arg(long, value_enum, default_value_t = UiModeArg::Rich, help = "终端展示模式")]
    pub ui: UiModeArg,
    #[arg(long, help = "要协同开发的目标仓库目录；不传则优先复用上次指定目录")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub shared: SharedTaskArgs,
    #[arg(
        long,
        value_enum,
        help = "高级参数：使用内置运行预设；feature-demo 适合黑客松现场展示"
    )]
    pub preset: Option<PresetArg>,
    #[arg(
        long,
        help = "高级参数：从指定 session 恢复运行，优先复用其执行图与已成功节点"
    )]
    pub resume: Option<String>,
    #[arg(
        long,
        value_enum,
        help = "高级参数：结果应用模式：auto-safe、bundle、none"
    )]
    pub apply_mode: Option<ApplyModeArg>,
    #[arg(long, help = "高级参数：worker 最大重试次数")]
    pub max_retries: Option<usize>,
    #[arg(long, help = "高级参数：任一节点失败后尽快停止调度")]
    pub fail_fast: bool,
    #[arg(long, help = "高级参数：成功完成后自动清理 worker worktree")]
    pub cleanup_success: bool,
}

#[derive(Debug, Clone, Args)]
pub struct TuiArgs {
    #[arg(long, help = "主界面默认打开的目标仓库目录；不传则优先复用上次目录")]
    pub target_dir: Option<PathBuf>,
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
        help = "仓库目录，用于定位 .codex-forge 会话；不传则优先复用上次指定目录"
    )]
    pub target_dir: Option<PathBuf>,
    #[arg(long, help = "按关键决策时间线输出，不走动态 UI 回放")]
    pub timeline: bool,
}

#[derive(Debug, Clone, Args)]
pub struct AgentsArgs {
    #[command(subcommand)]
    pub command: AgentCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum AgentCommands {
    List(AgentListArgs),
}

#[derive(Debug, Clone, Args)]
pub struct AgentListArgs {
    #[arg(long, help = "目标仓库目录；不传则优先复用上次指定目录")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct DoctorArgs {
    #[arg(long, help = "要检查的目标仓库目录；不传则优先复用上次指定目录")]
    pub target_dir: Option<PathBuf>,
    #[arg(long, help = "项目配置文件路径；默认尝试读取 codex-forge.toml")]
    pub config: Option<PathBuf>,
    #[arg(long, value_enum, help = "覆盖 apply mode，便于提前检查运行条件")]
    pub apply_mode: Option<ApplyModeArg>,
    #[arg(long, help = "按黑客松演示模式输出红黄绿结论与推荐执行参数")]
    pub demo: bool,
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
    #[arg(long, help = "目标仓库目录；不传则优先复用上次指定目录")]
    pub target_dir: Option<PathBuf>,
    #[arg(long, help = "显式指定配置文件路径")]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UiModeArg {
    Rich,
    Minimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ThinkingModeArg {
    Quick,
    Balanced,
    #[value(name = "hard-think")]
    HardThink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ApplyModeArg {
    #[value(name = "auto-safe")]
    AutoSafe,
    Bundle,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PresetArg {
    #[value(name = "feature-demo")]
    FeatureDemo,
}
