use anyhow::Result;

use crate::cli::{ThreadArgs, ThreadCommands, ThreadListArgs, ThreadNewArgs, ThreadShowArgs};
use crate::commands::format::{first_line, role_label, status_label};
use crate::harness::{HarnessStore, MemoryLayer};
use crate::workspace::resolve_target_dir;

pub fn run(args: ThreadArgs) -> Result<()> {
    match args.command {
        ThreadCommands::New(args) => run_new(args),
        ThreadCommands::List(args) => run_list(args),
        ThreadCommands::Show(args) => run_show(args),
    }
}

fn run_new(args: ThreadNewArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let thread = store.create_thread(args.title.as_deref())?;
    println!("id: {}", thread.id);
    println!("title: {}", thread.title);
    println!("repo: {}", thread.repo_root.display());
    Ok(())
}

fn run_list(args: ThreadListArgs) -> Result<()> {
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

fn run_show(args: ThreadShowArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let thread = store.load_thread(&args.thread_id)?;
    let messages = store.list_messages(&args.thread_id)?;
    let runs = store.list_runs(&args.thread_id)?;
    let approvals = store.list_pending_approvals(Some(&args.thread_id))?;
    let artifacts = store.list_artifacts(Some(&args.thread_id), None)?;
    let working_memory = store.load_memory(&args.thread_id, MemoryLayer::Working)?;
    let project_memory = store.load_memory(&args.thread_id, MemoryLayer::Project)?;

    println!("id: {}", thread.id);
    println!("title: {}", thread.title);
    println!("repo: {}", thread.repo_root.display());
    println!("messages: {}", thread.message_count);
    println!("runs: {}", thread.run_count);
    println!("pending approvals: {}", approvals.len());
    println!("artifacts: {}", artifacts.len());
    println!("working memory: {}", working_memory.entries.len());
    println!("project memory: {}", project_memory.entries.len());
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
    println!();
    println!("working memory：");
    for entry in working_memory.entries.iter().rev().take(6).rev() {
        println!("- {}", entry.content);
    }
    println!();
    println!("project memory：");
    for entry in project_memory.entries.iter().rev().take(6).rev() {
        println!("- {}", entry.content);
    }
    Ok(())
}
