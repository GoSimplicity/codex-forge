use std::path::Path;

use anyhow::{Result, bail};

use crate::config::AppConfig;
use crate::harness::RunExecutionKind;
use crate::model::ThinkingMode;

use super::autonomous::run_autonomous_codex_execution;
use super::engine::{
    cancel_run, confirm_plan_review_and_prepare_resume, execute_and_record_tool,
    reset_task_node_for_retry, run_execution,
};
use crate::harness::sandbox::DockerSandboxProvider;
use crate::harness::store::HarnessStore;
use crate::harness::tools::{mark_tool_approved, mark_tool_resolution};
use crate::harness::types::{
    ApprovalStatus, ChatRunOutcome, HarnessEvent, HarnessMessageRole, HarnessRunManifest,
    HarnessRunStatus, TaskNodeStatus, ToolCallStatus,
};

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub thread_id: String,
    pub message: String,
    pub model: Option<String>,
    pub thinking_mode: ThinkingMode,
}

pub async fn chat_once(
    repo_root: &Path,
    config: &AppConfig,
    request: ChatRequest,
) -> Result<ChatRunOutcome> {
    let store = HarnessStore::new(repo_root, config.backend.provider);
    let user_message = store.append_message(
        &request.thread_id,
        HarnessMessageRole::User,
        request.message,
        None,
    )?;
    let mut run = store.create_run(
        &request.thread_id,
        config
            .backend
            .resolve_model(request.model.as_deref())
            .map(ToOwned::to_owned),
        request.thinking_mode,
        config.backend.provider.into(),
    )?;
    run_selected_execution(repo_root, config, &store, &mut run).await?;
    let thread = store.load_thread(&request.thread_id)?;
    let assistant_message = store
        .list_messages(&request.thread_id)?
        .into_iter()
        .rev()
        .find(|message| {
            message.run_id.as_deref() == Some(&run.id)
                && message.role == HarnessMessageRole::Assistant
        });
    Ok(ChatRunOutcome {
        thread,
        run,
        user_message,
        assistant_message,
    })
}

pub async fn resolve_approval_and_resume(
    repo_root: &Path,
    config: &AppConfig,
    thread_id: &str,
    approval_id: &str,
    status: ApprovalStatus,
) -> Result<HarnessRunManifest> {
    let store = HarnessStore::new(repo_root, config.backend.provider);
    let approval = store.resolve_approval(thread_id, approval_id, status)?;
    let mut run = store.load_run(thread_id, &approval.run_id)?;
    store.append_run_event(
        thread_id,
        &run.id,
        HarnessEvent::ApprovalResolved {
            thread_id: thread_id.to_string(),
            run_id: run.id.clone(),
            approval_id: approval.id.clone(),
            status: approval.status,
        },
    )?;

    let tool = mark_tool_approved(&store, &run, &approval.tool_call_id, &approval.id)?;
    if approval.status == ApprovalStatus::Denied {
        mark_tool_resolution(
            &store,
            &run,
            &tool.id,
            ToolCallStatus::Skipped,
            Some("用户拒绝执行".to_string()),
            None,
        )?;
        store.append_message(
            thread_id,
            HarnessMessageRole::Tool,
            format!("工具 `{}` 未执行：用户拒绝审批。", tool.name),
            Some(run.id.clone()),
        )?;
        if let Some(task_node_id) = approval.task_node_id.as_deref()
            && let Ok(mut node) = store.load_task_node(&run, task_node_id)
        {
            node.status = TaskNodeStatus::Skipped;
            node.output_summary = Some("审批被拒绝，节点已跳过".to_string());
            store.update_task_node(&run, &node)?;
        }
        run.status = HarnessRunStatus::Completed;
        run.summary = Some("审批被拒绝，run 已结束".to_string());
        if let Some(sandbox) = &run.sandbox {
            DockerSandboxProvider::from(sandbox).destroy(sandbox)?;
        }
        run.sandbox = None;
        store.update_run(thread_id, &run)?;
        return Ok(run);
    }

    let thread = store.load_thread(thread_id)?;
    let _ = execute_and_record_tool(&store, &thread, &mut run, &approval.tool_call, &tool)?;
    if let Some(task_node_id) = approval.task_node_id.as_deref()
        && let Ok(mut node) = store.load_task_node(&run, task_node_id)
    {
        node.status = TaskNodeStatus::Ready;
        node.output_summary = Some(format!("审批 `{}` 已通过，继续执行", approval.id));
        store.update_task_node(&run, &node)?;
        run.active_task_node_id = Some(node.id);
    }
    run.status = HarnessRunStatus::Running;
    run.blocked_reason = None;
    run.last_error = None;
    store.update_run(thread_id, &run)?;

    run_selected_execution(repo_root, config, &store, &mut run).await?;
    Ok(run)
}

pub async fn resume_run(
    repo_root: &Path,
    config: &AppConfig,
    thread_id: &str,
    run_id: &str,
) -> Result<HarnessRunManifest> {
    let store = HarnessStore::new(repo_root, config.backend.provider);
    let mut run = store.load_run(thread_id, run_id)?;
    run.status = HarnessRunStatus::Running;
    run.blocked_reason = None;
    run.last_error = None;
    store.update_run(thread_id, &run)?;
    run_selected_execution(repo_root, config, &store, &mut run).await?;
    Ok(run)
}

pub async fn confirm_plan_review_and_resume(
    repo_root: &Path,
    config: &AppConfig,
    thread_id: &str,
    run_id: &str,
    task_node_id: &str,
) -> Result<HarnessRunManifest> {
    let store = HarnessStore::new(repo_root, config.backend.provider);
    let mut run = store.load_run(thread_id, run_id)?;
    if run.execution_kind.is_autonomous_codex() {
        bail!("当前 run 使用 Codex 自主执行，不存在待确认计划");
    }
    confirm_plan_review_and_prepare_resume(&store, &mut run, task_node_id)?;
    run_selected_execution(repo_root, config, &store, &mut run).await?;
    Ok(run)
}

pub fn cancel_active_run(
    repo_root: &Path,
    config: &AppConfig,
    thread_id: &str,
    run_id: &str,
) -> Result<HarnessRunManifest> {
    let store = HarnessStore::new(repo_root, config.backend.provider);
    let mut run = store.load_run(thread_id, run_id)?;
    cancel_run(&store, &mut run)?;
    Ok(run)
}

pub async fn retry_task_node_and_resume(
    repo_root: &Path,
    config: &AppConfig,
    thread_id: &str,
    run_id: &str,
    task_node_id: &str,
) -> Result<HarnessRunManifest> {
    let store = HarnessStore::new(repo_root, config.backend.provider);
    let mut run = store.load_run(thread_id, run_id)?;
    if run.execution_kind.is_autonomous_codex() {
        bail!("当前 run 使用 Codex 自主执行，不支持按 task node 重试");
    }
    reset_task_node_for_retry(&store, &run, task_node_id)?;
    run.active_task_node_id = Some(task_node_id.to_string());
    run.status = HarnessRunStatus::Running;
    run.blocked_reason = None;
    run.last_error = None;
    store.update_run(thread_id, &run)?;
    run_selected_execution(repo_root, config, &store, &mut run).await?;
    Ok(run)
}

async fn run_selected_execution(
    repo_root: &Path,
    config: &AppConfig,
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
) -> Result<()> {
    match run.execution_kind {
        RunExecutionKind::AutonomousCodex => {
            run_autonomous_codex_execution(repo_root, config, store, run).await
        }
        RunExecutionKind::Orchestrated => run_execution(repo_root, config, store, run).await,
    }
}
