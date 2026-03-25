use std::path::Path;

use anyhow::Result;

use crate::config::ProjectConfig;
use crate::model::ThinkingMode;

use super::engine::{execute_and_record_tool, run_execution};
use crate::harness::sandbox::DockerSandboxProvider;
use crate::harness::store::HarnessStore;
use crate::harness::tools::{mark_tool_approved, mark_tool_resolution};
use crate::harness::types::{
    ApprovalStatus, ChatRunOutcome, HarnessEvent, HarnessMessageRole, HarnessRunManifest,
    HarnessRunStatus, ToolCallStatus,
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
    config: &ProjectConfig,
    request: ChatRequest,
) -> Result<ChatRunOutcome> {
    let store = HarnessStore::new(repo_root);
    let user_message = store.append_message(
        &request.thread_id,
        HarnessMessageRole::User,
        request.message,
        None,
    )?;
    let mut run = store.create_run(
        &request.thread_id,
        request
            .model
            .clone()
            .or(config.backend.default_model.clone()),
        request.thinking_mode,
    )?;
    run_execution(repo_root, config, &store, &mut run).await?;
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
    config: &ProjectConfig,
    thread_id: &str,
    approval_id: &str,
    status: ApprovalStatus,
) -> Result<HarnessRunManifest> {
    let store = HarnessStore::new(repo_root);
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
        run.status = HarnessRunStatus::Completed;
        run.summary = Some("审批被拒绝，run 已结束".to_string());
        if let Some(sandbox) = &run.sandbox {
            DockerSandboxProvider {
                image: sandbox.image.clone(),
            }
            .destroy(sandbox)?;
        }
        run.sandbox = None;
        store.update_run(thread_id, &run)?;
        return Ok(run);
    }

    let thread = store.load_thread(thread_id)?;
    execute_and_record_tool(&store, &thread, &mut run, &approval.tool_call, &tool)?;

    run_execution(repo_root, config, &store, &mut run).await?;
    Ok(run)
}
