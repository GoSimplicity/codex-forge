use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "codex-forge",
    version,
    about = "基于 thread/run 的本地 Codex harness"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Tui(TuiArgs),
    Thread(ThreadArgs),
    Chat(ChatArgs),
    Run(RunArgs),
    Replay(ReplayArgs),
    Approval(ApprovalArgs),
    Artifact(ArtifactArgs),
    Config(ConfigArgs),
}

#[derive(Debug, Clone, Args)]
pub struct TuiArgs {
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
    #[arg(long, help = "启动时默认选中的 thread id")]
    pub thread: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct ThreadArgs {
    #[command(subcommand)]
    pub command: ThreadCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ThreadCommands {
    New(ThreadNewArgs),
    List(ThreadListArgs),
    Show(ThreadShowArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ThreadNewArgs {
    #[arg(long, help = "线程标题；不传则按仓库名生成")]
    pub title: Option<String>,
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct ThreadListArgs {
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct ThreadShowArgs {
    #[arg(help = "thread id")]
    pub thread_id: String,
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct ChatArgs {
    #[arg(help = "要发送给主代理的消息")]
    pub message: String,
    #[arg(long, help = "目标 thread id；不传则自动创建新 thread")]
    pub thread: Option<String>,
    #[arg(long, help = "自动创建新 thread 时的标题")]
    pub title: Option<String>,
    #[arg(long, help = "覆盖 Codex model")]
    pub model: Option<String>,
    #[arg(long, value_enum, help = "思考强度")]
    pub thinking_mode: Option<ThinkingModeArg>,
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    #[command(subcommand)]
    pub command: RunCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum RunCommands {
    List(RunListArgs),
    Show(RunShowArgs),
    Resume(RunResumeArgs),
    Cancel(RunCancelArgs),
    RetryNode(RunRetryNodeArgs),
    Node(RunNodeShowArgs),
}

#[derive(Debug, Clone, Args)]
pub struct RunListArgs {
    #[arg(long, help = "所属 thread id")]
    pub thread: String,
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct RunShowArgs {
    #[arg(long, help = "所属 thread id")]
    pub thread: String,
    #[arg(help = "run id")]
    pub run_id: String,
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct RunResumeArgs {
    #[arg(long, help = "所属 thread id")]
    pub thread: String,
    #[arg(help = "run id")]
    pub run_id: String,
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct RunCancelArgs {
    #[arg(long, help = "所属 thread id")]
    pub thread: String,
    #[arg(help = "run id")]
    pub run_id: String,
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct RunRetryNodeArgs {
    #[arg(long, help = "所属 thread id")]
    pub thread: String,
    #[arg(long, help = "所属 run id")]
    pub run: String,
    #[arg(help = "task node id")]
    pub task_node_id: String,
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct RunNodeShowArgs {
    #[arg(long, help = "所属 thread id")]
    pub thread: String,
    #[arg(long, help = "所属 run id")]
    pub run: String,
    #[arg(help = "task node id")]
    pub task_node_id: String,
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct ReplayArgs {
    #[arg(long, help = "所属 thread id")]
    pub thread: String,
    #[arg(help = "run id")]
    pub run_id: String,
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct ApprovalArgs {
    #[command(subcommand)]
    pub command: ApprovalCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ApprovalCommands {
    List(ApprovalListArgs),
    Approve(ApprovalResolveArgs),
    Deny(ApprovalResolveArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ApprovalListArgs {
    #[arg(long, help = "只看某个 thread 的审批")]
    pub thread: Option<String>,
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct ApprovalResolveArgs {
    #[arg(long, help = "所属 thread id")]
    pub thread: String,
    #[arg(help = "approval id")]
    pub approval_id: String,
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct ArtifactArgs {
    #[command(subcommand)]
    pub command: ArtifactCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ArtifactCommands {
    List(ArtifactListArgs),
    Show(ArtifactShowArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ArtifactListArgs {
    #[arg(long, help = "只看某个 thread 的 artifact")]
    pub thread: Option<String>,
    #[arg(long, help = "只看某个 run 的 artifact；使用时必须同时指定 --thread")]
    pub run: Option<String>,
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct ArtifactShowArgs {
    #[arg(help = "artifact id")]
    pub artifact_id: String,
    #[arg(long, help = "只在某个 thread 范围内查找")]
    pub thread: Option<String>,
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ConfigCommands {
    Init(ConfigTargetArgs),
    Show(ConfigTargetArgs),
    Validate(ConfigTargetArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ConfigTargetArgs {
    #[arg(long, help = "目标仓库目录；不传则默认使用当前目录或其 Git 根")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ThinkingModeArg {
    Quick,
    Balanced,
    #[value(name = "hard-think")]
    HardThink,
}
