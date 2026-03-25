use anyhow::Result;

use crate::cli::{ApprovalArgs, ApprovalCommands, ApprovalListArgs, ApprovalResolveArgs};
use crate::commands::format::status_label;
use crate::config::load_project_config;
use crate::harness::{ApprovalStatus, HarnessStore, resolve_approval_and_resume};
use crate::workspace::resolve_target_dir;

pub async fn run(args: ApprovalArgs) -> Result<()> {
    match args.command {
        ApprovalCommands::List(args) => run_list(args),
        ApprovalCommands::Approve(args) => run_resolve(args, ApprovalStatus::Approved).await,
        ApprovalCommands::Deny(args) => run_resolve(args, ApprovalStatus::Denied).await,
    }
}

fn run_list(args: ApprovalListArgs) -> Result<()> {
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

async fn run_resolve(args: ApprovalResolveArgs, status: ApprovalStatus) -> Result<()> {
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
