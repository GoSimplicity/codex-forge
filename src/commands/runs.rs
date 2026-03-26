use anyhow::Result;

use crate::cli::{
    RunArgs, RunCancelArgs, RunCommands, RunListArgs, RunNodeShowArgs, RunResumeArgs,
    RunRetryNodeArgs, RunShowArgs,
};
use crate::commands::format::status_label;
use crate::config::load_app_config;
use crate::harness::{
    HarnessStore, TaskNodeKind, TaskNodeStatus, cancel_active_run, resume_run,
    retry_task_node_and_resume,
};
use crate::workspace::resolve_target_dir;

pub async fn run(args: RunArgs) -> Result<()> {
    match args.command {
        RunCommands::List(args) => run_list(args),
        RunCommands::Show(args) => run_show(args),
        RunCommands::Resume(args) => run_resume(args).await,
        RunCommands::Cancel(args) => run_cancel(args),
        RunCommands::RetryNode(args) => run_retry_node(args).await,
        RunCommands::Node(args) => run_node_show(args),
    }
}

fn run_list(args: RunListArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let runs = store.list_runs(&args.thread)?;
    if runs.is_empty() {
        println!("当前 thread 没有 run");
        return Ok(());
    }

    for run in runs {
        println!(
            "{}\t{}\tturns={}\tactive={}\t{}",
            run.id,
            status_label(run.status),
            run.turn_count,
            run.active_task_node_id.as_deref().unwrap_or("-"),
            run.summary.as_deref().unwrap_or("无摘要")
        );
    }
    Ok(())
}

fn run_show(args: RunShowArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let run = store.load_run(&args.thread, &args.run_id)?;
    let tool_calls = store.list_tool_calls(&run)?;
    let artifacts = store.list_artifacts(Some(&args.thread), Some(&args.run_id))?;
    let subagents = store.list_subagents(&run)?;
    let graph = store.load_task_graph(&run)?;
    let nodes = store.list_task_nodes(&run)?;
    let contract = store.load_execution_contract(&args.thread).ok();
    let progress = store.load_progress_ledger(&args.thread).ok();
    let latest_evaluation = store
        .list_evaluations(&run)
        .ok()
        .and_then(|mut items| items.drain(..).next());
    println!("thread: {}", run.thread_id);
    println!("run: {}", run.id);
    println!("status: {}", status_label(run.status));
    println!("thinking: {}", run.thinking_mode.label());
    println!("backend: {}", run.backend.label());
    if let Some(model) = &run.model {
        println!("model: {model}");
    }
    println!("turns: {}", run.turn_count);
    println!("tool calls: {}", tool_calls.len());
    println!("artifacts: {}", artifacts.len());
    println!("subagents: {}", subagents.len());
    println!("task graph: {}", graph.id);
    println!("strategy: {}", graph_strategy_label(graph.strategy));
    println!("goal: {}", graph.goal);
    println!(
        "active node: {}",
        run.active_task_node_id.as_deref().unwrap_or("-")
    );
    println!("output: {}", run.output_path.display());
    println!("log: {}", run.log_path.display());
    if !graph.success_criteria.is_empty() {
        println!("success criteria:");
        for item in graph.success_criteria {
            println!("  - {item}");
        }
    }
    if let Some(contract) = contract {
        println!("features: {}", contract.ordered_features.len());
    }
    if let Some(progress) = progress {
        println!(
            "phase: {}",
            progress.current_phase.as_deref().unwrap_or("-")
        );
        println!(
            "completed features: {}",
            if progress.completed_features.is_empty() {
                "-".to_string()
            } else {
                progress.completed_features.join(", ")
            }
        );
        println!(
            "current feature: {}",
            progress.current_feature.as_deref().unwrap_or("-")
        );
        println!(
            "next step: {}",
            progress.next_step.as_deref().unwrap_or("-")
        );
        println!(
            "latest recoverable failure: {}",
            progress
                .latest_recoverable_failure
                .as_deref()
                .unwrap_or("-")
        );
        println!(
            "blocking reason: {}",
            progress.blocking_reason.as_deref().unwrap_or("-")
        );
    }
    if let Some(evaluation) = latest_evaluation {
        println!(
            "latest evaluation: {} ({})",
            if evaluation.passed {
                "passed"
            } else {
                "failed"
            },
            evaluation.reason
        );
    }
    if let Some(summary) = &run.summary {
        println!("summary: {summary}");
    }
    if let Some(reason) = &run.blocked_reason {
        println!("blocked: {reason}");
    }
    if let Some(error) = &run.last_error {
        println!("error: {error}");
    }
    if let Some(sandbox) = &run.sandbox {
        println!("sandbox: {} ({})", sandbox.container_name, sandbox.image);
    }
    if !nodes.is_empty() {
        println!("nodes:");
        for node in nodes {
            println!(
                "  {} [{}] {:?} attempts={} {}",
                node.id,
                task_status_label(node.status),
                node.kind,
                node.attempt_count,
                node.output_summary.as_deref().unwrap_or("无摘要")
            );
        }
    }
    Ok(())
}

async fn run_resume(args: RunResumeArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let config = load_app_config(&repo_root)?;
    let run = resume_run(&repo_root, &config, &args.thread, &args.run_id).await?;
    println!("run: {}", run.id);
    println!("status: {}", status_label(run.status));
    if let Some(summary) = run.summary {
        println!("summary: {summary}");
    }
    Ok(())
}

fn run_cancel(args: RunCancelArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let run = cancel_active_run(&repo_root, &args.thread, &args.run_id)?;
    println!("run: {}", run.id);
    println!("status: {}", status_label(run.status));
    Ok(())
}

async fn run_retry_node(args: RunRetryNodeArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let config = load_app_config(&repo_root)?;
    let run = retry_task_node_and_resume(
        &repo_root,
        &config,
        &args.thread,
        &args.run,
        &args.task_node_id,
    )
    .await?;
    println!("run: {}", run.id);
    println!("status: {}", status_label(run.status));
    if let Some(summary) = run.summary {
        println!("summary: {summary}");
    }
    Ok(())
}

fn run_node_show(args: RunNodeShowArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let run = store.load_run(&args.thread, &args.run)?;
    let node = store.load_task_node(&run, &args.task_node_id)?;
    println!("thread: {}", node.thread_id);
    println!("run: {}", node.run_id);
    println!("node: {}", node.id);
    println!("kind: {}", task_kind_label(node.kind));
    println!("status: {}", task_status_label(node.status));
    println!("title: {}", node.title);
    println!("attempts: {}", node.attempt_count);
    println!("depends_on: {}", node.depends_on.join(", "));
    println!("instructions: {}", node.instructions);
    if let Some(subagent_id) = node.last_subagent_id {
        println!("last subagent: {subagent_id}");
    }
    if let Some(feature_id) = node.feature_id {
        println!("feature: {feature_id}");
    }
    if let Some(summary) = node.output_summary {
        println!("summary: {summary}");
    }
    if let Some(error) = node.error {
        println!("error: {error}");
    }
    Ok(())
}

fn task_status_label(status: TaskNodeStatus) -> &'static str {
    match status {
        TaskNodeStatus::Pending => "pending",
        TaskNodeStatus::Ready => "ready",
        TaskNodeStatus::Running => "running",
        TaskNodeStatus::WaitingForInput => "waiting_for_input",
        TaskNodeStatus::Completed => "completed",
        TaskNodeStatus::Failed => "failed",
        TaskNodeStatus::Skipped => "skipped",
    }
}

fn task_kind_label(kind: TaskNodeKind) -> &'static str {
    match kind {
        TaskNodeKind::Plan => "plan",
        TaskNodeKind::Initialize => "initialize",
        TaskNodeKind::BuildExecutionContract => "build_execution_contract",
        TaskNodeKind::PlanReview => "plan_review",
        TaskNodeKind::SelectNextFeature => "select_next_feature",
        TaskNodeKind::ExecuteFeature => "execute_feature",
        TaskNodeKind::EvaluateFeature => "evaluate_feature",
        TaskNodeKind::CheckpointProgress => "checkpoint_progress",
        TaskNodeKind::FinalizeDelivery => "finalize_delivery",
        TaskNodeKind::Explore => "explore",
        TaskNodeKind::Implement => "implement",
        TaskNodeKind::Review => "review",
        TaskNodeKind::Test => "test",
        TaskNodeKind::Summarize => "summarize",
        TaskNodeKind::ApprovalGate => "approval_gate",
    }
}

fn graph_strategy_label(strategy: crate::harness::TaskGraphStrategy) -> &'static str {
    match strategy {
        crate::harness::TaskGraphStrategy::Research => "research",
        crate::harness::TaskGraphStrategy::LongRunningDelivery => "long_running_delivery",
    }
}
