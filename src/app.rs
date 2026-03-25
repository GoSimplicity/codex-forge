use anyhow::{Result, bail};
use clap::Parser;

use crate::cli::{
    ApprovalArgs, ApprovalCommands, ApprovalListArgs, ApprovalResolveArgs, ArtifactArgs,
    ArtifactCommands, ArtifactListArgs, ArtifactShowArgs, ChatArgs, Cli, Commands, ConfigArgs,
    ConfigCommands, ConfigTargetArgs, ReplayArgs, RunArgs, RunCommands, RunListArgs, RunShowArgs,
    ThinkingModeArg, ThreadArgs, ThreadCommands, ThreadListArgs, ThreadNewArgs, ThreadShowArgs,
    TuiArgs,
};
use crate::config::{init_default_config, load_project_config};
use crate::harness::{
    ApprovalStatus, ArtifactKind, ChatRequest, HarnessEvent, HarnessMessageRole, HarnessStore,
    chat_once, resolve_approval_and_resume,
};
use crate::model::ThinkingMode;
use crate::tui::run_tui;
use crate::workspace::resolve_target_dir;

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => {
            run_tui(TuiArgs {
                target_dir: None,
                thread: None,
            })
            .await?
        }
        Some(Commands::Tui(args)) => run_tui(args).await?,
        Some(Commands::Thread(args)) => run_thread_command(args)?,
        Some(Commands::Chat(args)) => run_chat_command(args).await?,
        Some(Commands::Run(args)) => run_run_command(args)?,
        Some(Commands::Replay(args)) => run_replay_command(args)?,
        Some(Commands::Approval(args)) => run_approval_command(args).await?,
        Some(Commands::Artifact(args)) => run_artifact_command(args)?,
        Some(Commands::Config(args)) => run_config_command(args)?,
    }
    Ok(())
}

fn run_thread_command(args: ThreadArgs) -> Result<()> {
    match args.command {
        ThreadCommands::New(args) => run_thread_new(args),
        ThreadCommands::List(args) => run_thread_list(args),
        ThreadCommands::Show(args) => run_thread_show(args),
    }
}

fn run_thread_new(args: ThreadNewArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let thread = store.create_thread(args.title.as_deref())?;
    println!("id: {}", thread.id);
    println!("title: {}", thread.title);
    println!("repo: {}", thread.repo_root.display());
    Ok(())
}

fn run_thread_list(args: ThreadListArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let threads = store.list_threads()?;
    if threads.is_empty() {
        println!("当前没有 thread");
        return Ok(());
    }

    for thread in threads {
        println!(
            "{}\t{}\tmessages={}\truns={}\tupdated={}",
            thread.id,
            thread.title,
            thread.message_count,
            thread.run_count,
            thread.updated_at.format("%Y-%m-%d %H:%M:%S")
        );
    }
    Ok(())
}

fn run_thread_show(args: ThreadShowArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let thread = store.load_thread(&args.thread_id)?;
    let messages = store.list_messages(&args.thread_id)?;
    let runs = store.list_runs(&args.thread_id)?;
    let approvals = store.list_pending_approvals(Some(&args.thread_id))?;
    let artifacts = store.list_artifacts(Some(&args.thread_id), None)?;

    println!("id: {}", thread.id);
    println!("title: {}", thread.title);
    println!("repo: {}", thread.repo_root.display());
    println!("messages: {}", thread.message_count);
    println!("runs: {}", thread.run_count);
    println!("pending approvals: {}", approvals.len());
    println!("artifacts: {}", artifacts.len());
    println!();
    println!("最近消息：");
    for message in messages.iter().rev().take(10).rev() {
        println!(
            "- [{}] {}",
            role_label(message.role),
            first_line(&message.content)
        );
    }
    println!();
    println!("最近运行：");
    for run in runs.iter().take(10) {
        println!(
            "- {} [{}] {}",
            run.id,
            status_label(run.status),
            run.summary.as_deref().unwrap_or("无摘要")
        );
    }
    Ok(())
}

async fn run_chat_command(args: ChatArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let loaded = load_project_config(&repo_root)?;
    let store = HarnessStore::new(&repo_root);
    let thread_id = match args.thread {
        Some(thread_id) => thread_id,
        None => store.create_thread(args.title.as_deref())?.id,
    };
    let outcome = chat_once(
        &repo_root,
        &loaded.value,
        ChatRequest {
            thread_id: thread_id.clone(),
            message: validated_input(&args.message)?,
            model: args.model.or(loaded.value.backend.default_model.clone()),
            thinking_mode: args
                .thinking_mode
                .map(into_thinking_mode)
                .unwrap_or_default(),
        },
    )
    .await?;

    println!("thread: {}", thread_id);
    println!("run: {}", outcome.run.id);
    println!("status: {}", status_label(outcome.run.status));
    println!();
    if let Some(message) = outcome.assistant_message {
        println!("{}", message.content.trim());
    } else {
        println!("{}", outcome.run.summary.unwrap_or_else(|| "已进入下一阶段".to_string()));
    }
    Ok(())
}

fn run_run_command(args: RunArgs) -> Result<()> {
    match args.command {
        RunCommands::List(args) => run_run_list(args),
        RunCommands::Show(args) => run_run_show(args),
    }
}

fn run_run_list(args: RunListArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let runs = store.list_runs(&args.thread)?;
    if runs.is_empty() {
        println!("当前 thread 没有 run");
        return Ok(());
    }

    for run in runs {
        println!(
            "{}\t{}\tturns={}\t{}",
            run.id,
            status_label(run.status),
            run.turn_count,
            run.summary.as_deref().unwrap_or("无摘要")
        );
    }
    Ok(())
}

fn run_run_show(args: RunShowArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let run = store.load_run(&args.thread, &args.run_id)?;
    let tool_calls = store.list_tool_calls(&run)?;
    let artifacts = store.list_artifacts(Some(&args.thread), Some(&args.run_id))?;
    let subagents = store.list_subagents(&run)?;
    println!("thread: {}", run.thread_id);
    println!("run: {}", run.id);
    println!("status: {}", status_label(run.status));
    println!("thinking: {}", run.thinking_mode.label());
    println!("backend: codex");
    if let Some(model) = &run.model {
        println!("model: {model}");
    }
    println!("turns: {}", run.turn_count);
    println!("tool calls: {}", tool_calls.len());
    println!("artifacts: {}", artifacts.len());
    println!("subagents: {}", subagents.len());
    println!("output: {}", run.output_path.display());
    println!("log: {}", run.log_path.display());
    if let Some(summary) = &run.summary {
        println!("summary: {summary}");
    }
    if let Some(error) = &run.last_error {
        println!("error: {error}");
    }
    if let Some(sandbox) = &run.sandbox {
        println!("sandbox: {} ({})", sandbox.container_name, sandbox.image);
    }
    Ok(())
}

fn run_replay_command(args: ReplayArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let events = store.list_run_events(&args.thread, &args.run_id)?;
    if events.is_empty() {
        println!("当前 run 没有事件");
        return Ok(());
    }

    for event in events {
        println!(
            "{} {}",
            event.at.format("%Y-%m-%d %H:%M:%S"),
            describe_event(&event.payload)
        );
    }
    Ok(())
}

async fn run_approval_command(args: ApprovalArgs) -> Result<()> {
    match args.command {
        ApprovalCommands::List(args) => run_approval_list(args),
        ApprovalCommands::Approve(args) => run_approval_resolve(args, ApprovalStatus::Approved).await,
        ApprovalCommands::Deny(args) => run_approval_resolve(args, ApprovalStatus::Denied).await,
    }
}

fn run_approval_list(args: ApprovalListArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let approvals = store.list_pending_approvals(args.thread.as_deref())?;
    if approvals.is_empty() {
        println!("当前没有待处理审批");
        return Ok(());
    }
    for approval in approvals {
        println!(
            "{}\tthread={}\trun={}\ttool={}\t{}",
            approval.id, approval.thread_id, approval.run_id, approval.tool_name, approval.reason
        );
    }
    Ok(())
}

async fn run_approval_resolve(args: ApprovalResolveArgs, status: ApprovalStatus) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let loaded = load_project_config(&repo_root)?;
    let run = resolve_approval_and_resume(
        &repo_root,
        &loaded.value,
        &args.thread,
        &args.approval_id,
        status,
    )
    .await?;
    println!("run: {}", run.id);
    println!("status: {}", status_label(run.status));
    if let Some(summary) = run.summary {
        println!("summary: {summary}");
    }
    Ok(())
}

fn run_artifact_command(args: ArtifactArgs) -> Result<()> {
    match args.command {
        ArtifactCommands::List(args) => run_artifact_list(args),
        ArtifactCommands::Show(args) => run_artifact_show(args),
    }
}

fn run_artifact_list(args: ArtifactListArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let artifacts = store.list_artifacts(args.thread.as_deref(), args.run.as_deref())?;
    if artifacts.is_empty() {
        println!("当前没有 artifact");
        return Ok(());
    }
    for artifact in artifacts {
        println!(
            "{}\trun={}\tkind={}\t{}\t{}",
            artifact.id,
            artifact.run_id,
            artifact_kind_label(artifact.kind),
            artifact.label,
            artifact.path.display()
        );
    }
    Ok(())
}

fn run_artifact_show(args: ArtifactShowArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let artifacts = store.list_artifacts(args.thread.as_deref(), None)?;
    let artifact = artifacts
        .into_iter()
        .find(|artifact| artifact.id == args.artifact_id)
        .ok_or_else(|| anyhow::anyhow!("未找到 artifact：{}", args.artifact_id))?;
    println!("id: {}", artifact.id);
    println!("run: {}", artifact.run_id);
    println!("kind: {}", artifact_kind_label(artifact.kind));
    println!("label: {}", artifact.label);
    println!("path: {}", artifact.path.display());
    if matches!(artifact.kind, ArtifactKind::Text | ArtifactKind::ToolResult | ArtifactKind::SandboxLog)
    {
        println!();
        println!("{}", std::fs::read_to_string(&artifact.path)?);
    }
    Ok(())
}

fn run_config_command(args: ConfigArgs) -> Result<()> {
    match args.command {
        ConfigCommands::Init(args) => run_config_init(args),
        ConfigCommands::Show(args) => run_config_show(args),
        ConfigCommands::Validate(args) => run_config_validate(args),
    }
}

fn run_config_init(args: ConfigTargetArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let path = init_default_config(&repo_root)?;
    println!("已初始化配置：{}", path.display());
    Ok(())
}

fn run_config_show(args: ConfigTargetArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let loaded = load_project_config(&repo_root)?;
    println!("{}", toml::to_string_pretty(&loaded.value)?);
    Ok(())
}

fn run_config_validate(args: ConfigTargetArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let loaded = load_project_config(&repo_root)?;
    if loaded.value.runtime.max_turns == 0 {
        bail!("runtime.max_turns 必须大于 0");
    }
    if loaded.value.sandbox.docker_image.trim().is_empty() {
        bail!("sandbox.docker_image 不能为空");
    }
    println!("配置有效：{}", loaded.path.display());
    Ok(())
}

fn validated_input(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("输入不能为空");
    }
    Ok(trimmed.to_string())
}

pub fn into_thinking_mode(value: ThinkingModeArg) -> ThinkingMode {
    match value {
        ThinkingModeArg::Quick => ThinkingMode::Quick,
        ThinkingModeArg::Balanced => ThinkingMode::Balanced,
        ThinkingModeArg::HardThink => ThinkingMode::HardThink,
    }
}

pub fn describe_event(event: &HarnessEvent) -> String {
    match event {
        HarnessEvent::RunCreated { run_id, .. } => format!("创建 run `{run_id}`"),
        HarnessEvent::SandboxReady {
            image,
            container_name,
            ..
        } => format!("Docker 沙箱已启动：{container_name} ({image})"),
        HarnessEvent::RunStarted { run_id, .. } => format!("启动 run `{run_id}`"),
        HarnessEvent::BackendTurnCompleted { turn, .. } => format!("完成第 {turn} 个 turn"),
        HarnessEvent::MessageAppended {
            role, message_id, ..
        } => {
            format!("写入消息 `{message_id}`，角色={}", role_label(*role))
        }
        HarnessEvent::ToolCallPlanned {
            tool_call_id,
            tool_name,
            ..
        } => format!("计划工具 `{tool_name}`（{tool_call_id}）"),
        HarnessEvent::ToolCallCompleted {
            tool_call_id, status, ..
        } => format!("工具 `{tool_call_id}` 执行结束：{}", tool_status_label(*status)),
        HarnessEvent::ApprovalRequested {
            approval_id,
            tool_name,
            ..
        } => format!("等待审批 `{approval_id}`：{tool_name}"),
        HarnessEvent::ApprovalResolved {
            approval_id, status, ..
        } => format!("审批 `{approval_id}` 已处理：{}", approval_status_label(*status)),
        HarnessEvent::ArtifactCreated { artifact_id, label, .. } => {
            format!("生成 artifact `{artifact_id}`：{label}")
        }
        HarnessEvent::SubagentStarted {
            subagent_id, kind, ..
        } => format!("子代理 `{subagent_id}` 已启动：{:?}", kind),
        HarnessEvent::SubagentCompleted {
            subagent_id, status, ..
        } => format!("子代理 `{subagent_id}` 已结束：{}", status_label(*status)),
        HarnessEvent::RunCompleted { run_id, .. } => format!("run `{run_id}` 已完成"),
        HarnessEvent::RunFailed { run_id, error, .. } => {
            format!("run `{run_id}` 失败：{error}")
        }
    }
}

pub fn role_label(role: HarnessMessageRole) -> &'static str {
    match role {
        HarnessMessageRole::User => "user",
        HarnessMessageRole::Assistant => "assistant",
        HarnessMessageRole::System => "system",
        HarnessMessageRole::Tool => "tool",
        HarnessMessageRole::Summary => "summary",
    }
}

pub fn status_label(status: crate::harness::HarnessRunStatus) -> &'static str {
    match status {
        crate::harness::HarnessRunStatus::Pending => "pending",
        crate::harness::HarnessRunStatus::Running => "running",
        crate::harness::HarnessRunStatus::WaitingForInput => "waiting_for_input",
        crate::harness::HarnessRunStatus::Completed => "completed",
        crate::harness::HarnessRunStatus::Failed => "failed",
        crate::harness::HarnessRunStatus::Cancelled => "cancelled",
    }
}

pub fn tool_status_label(status: crate::harness::types::ToolCallStatus) -> &'static str {
    match status {
        crate::harness::types::ToolCallStatus::Pending => "pending",
        crate::harness::types::ToolCallStatus::PendingApproval => "pending_approval",
        crate::harness::types::ToolCallStatus::Running => "running",
        crate::harness::types::ToolCallStatus::Succeeded => "succeeded",
        crate::harness::types::ToolCallStatus::Failed => "failed",
        crate::harness::types::ToolCallStatus::Skipped => "skipped",
    }
}

pub fn approval_status_label(status: ApprovalStatus) -> &'static str {
    match status {
        ApprovalStatus::Pending => "pending",
        ApprovalStatus::Approved => "approved",
        ApprovalStatus::Denied => "denied",
    }
}

pub fn artifact_kind_label(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Text => "text",
        ArtifactKind::File => "file",
        ArtifactKind::ToolResult => "tool_result",
        ArtifactKind::SandboxLog => "sandbox_log",
        ArtifactKind::SandboxSnapshot => "sandbox_snapshot",
    }
}

fn first_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("空")
}
