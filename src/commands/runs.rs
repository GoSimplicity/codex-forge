use anyhow::Result;

use crate::cli::{RunArgs, RunCommands, RunListArgs, RunShowArgs};
use crate::commands::format::status_label;
use crate::harness::HarnessStore;
use crate::workspace::resolve_target_dir;

pub fn run(args: RunArgs) -> Result<()> {
    match args.command {
        RunCommands::List(args) => run_list(args),
        RunCommands::Show(args) => run_show(args),
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
            "{}\t{}\tturns={}\t{}",
            run.id,
            status_label(run.status),
            run.turn_count,
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
