use std::path::Path;

use anyhow::Result;

use crate::config::ProjectConfig;
use crate::model::ThinkingMode;

use super::backend::{AgentBackend, BackendTurnRequest, CodexBackend, built_in_tools};
use super::sandbox::DockerSandboxProvider;
use super::store::HarnessStore;
use super::tools::{approval_reason, execute_tool_call, mark_tool_approved, mark_tool_resolution, tool_requires_approval};
use super::types::{
    ApprovalStatus, ChatRunOutcome, HarnessEvent, HarnessMessageRole, HarnessRunManifest,
    HarnessRunStatus, HarnessThreadManifest, SubagentKind, ToolCallRequest, ToolCallStatus,
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
        .find(|message| message.run_id.as_deref() == Some(&run.id) && message.role == HarnessMessageRole::Assistant);
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

async fn run_execution(
    repo_root: &Path,
    config: &ProjectConfig,
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
) -> Result<()> {
    let backend = CodexBackend;
    let tools = built_in_tools();
    let thread = store.load_thread(&run.thread_id)?;
    if run.sandbox.is_none() {
        let sandbox = DockerSandboxProvider {
            image: config.sandbox.docker_image.clone(),
        }
        .start(repo_root, run)?;
        run.sandbox = Some(sandbox.clone());
        store.update_run(&run.thread_id, run)?;
        store.append_run_event(
            &run.thread_id,
            &run.id,
            HarnessEvent::RunCreated {
                thread_id: run.thread_id.clone(),
                run_id: run.id.clone(),
            },
        )?;
        store.append_run_event(
            &run.thread_id,
            &run.id,
            HarnessEvent::SandboxReady {
                thread_id: run.thread_id.clone(),
                run_id: run.id.clone(),
                image: sandbox.image.clone(),
                container_name: sandbox.container_name.clone(),
            },
        )?;
    }

    run.status = HarnessRunStatus::Running;
    store.update_run(&run.thread_id, run)?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::RunStarted {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
        },
    )?;

    for turn in 0..config.runtime.max_turns {
        run.turn_count += 1;
        store.update_run(&run.thread_id, run)?;
        let messages = store.list_messages(&run.thread_id)?;
        let turn_request = BackendTurnRequest {
            thread: &thread,
            messages: &messages,
            thinking_mode: run.thinking_mode,
            model: run.model.as_deref(),
            tools: &tools,
            system_hint: "你处于本地闭环 harness 中，可通过工具和子代理继续推进。",
        };
        let envelope = backend
            .execute_turn(repo_root, &turn_request, &run.output_path, &run.log_path)
            .await?;
        store.append_run_event(
            &run.thread_id,
            &run.id,
            HarnessEvent::BackendTurnCompleted {
                thread_id: run.thread_id.clone(),
                run_id: run.id.clone(),
                turn: turn + 1,
            },
        )?;

        let mut assistant_message = None;
        if let Some(message) = envelope.assistant_message {
            assistant_message = Some(store.append_message(
                &run.thread_id,
                HarnessMessageRole::Assistant,
                message,
                Some(run.id.clone()),
            )?);
        }

        for subagent in envelope.subagent_calls {
            execute_subagent(store, run, &subagent.kind, &subagent.task)?;
        }

        if !envelope.tool_calls.is_empty() {
            for call in envelope.tool_calls {
                let mut record = store.append_tool_call(run, &call)?;
                store.append_run_event(
                    &run.thread_id,
                    &run.id,
                    HarnessEvent::ToolCallPlanned {
                        thread_id: run.thread_id.clone(),
                        run_id: run.id.clone(),
                        tool_call_id: record.id.clone(),
                        tool_name: record.name.clone(),
                    },
                )?;
                if tool_requires_approval(&call.name) {
                    let approval = store.append_approval(
                        &thread,
                        run,
                        &record,
                        approval_reason(&call.name).to_string(),
                        call.clone(),
                    )?;
                    record.approval_id = Some(approval.id.clone());
                    record.status = ToolCallStatus::PendingApproval;
                    store.update_tool_call(run, &record)?;
                    run.status = HarnessRunStatus::WaitingForInput;
                    run.summary = Some(format!("等待审批：{}", approval.tool_name));
                    store.update_run(&run.thread_id, run)?;
                    store.append_run_event(
                        &run.thread_id,
                        &run.id,
                        HarnessEvent::ApprovalRequested {
                            thread_id: run.thread_id.clone(),
                            run_id: run.id.clone(),
                            approval_id: approval.id,
                            tool_name: record.name,
                        },
                    )?;
                    return Ok(());
                }
                execute_and_record_tool(store, &thread, run, &call, &record)?;
            }
        }

        if envelope.final_response || (assistant_message.is_some() && run.turn_count >= 1) {
            finish_run(store, run, None)?;
            return Ok(());
        }
    }

    finish_run(
        store,
        run,
        Some("达到最大 turn 次数仍未完成".to_string()),
    )
}

fn execute_and_record_tool(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &mut HarnessRunManifest,
    call: &ToolCallRequest,
    record: &super::types::ToolCallRecord,
) -> Result<()> {
    let sandbox = run
        .sandbox
        .clone()
        .ok_or_else(|| anyhow::anyhow!("run 缺少 sandbox"))?;
    let result = execute_tool_call(store, thread, run, &sandbox, call)?;
    mark_tool_resolution(
        store,
        run,
        &record.id,
        ToolCallStatus::Succeeded,
        Some(result.message.clone()),
        None,
    )?;
    for artifact in result.artifacts {
        store.append_run_event(
            &run.thread_id,
            &run.id,
            HarnessEvent::ArtifactCreated {
                thread_id: run.thread_id.clone(),
                run_id: run.id.clone(),
                artifact_id: artifact.id,
                label: artifact.label,
            },
        )?;
    }
    store.append_message(
        &run.thread_id,
        HarnessMessageRole::Tool,
        result.message.clone(),
        Some(run.id.clone()),
    )?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::ToolCallCompleted {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            tool_call_id: record.id.clone(),
            status: ToolCallStatus::Succeeded,
        },
    )?;
    Ok(())
}

fn execute_subagent(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    kind: &SubagentKind,
    task: &str,
) -> Result<()> {
    let mut subagent = store.append_subagent(run, *kind, task.to_string())?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::SubagentStarted {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            subagent_id: subagent.id.clone(),
            kind: *kind,
        },
    )?;
    subagent.status = HarnessRunStatus::Completed;
    subagent.summary = Some(format!("{kind:?} 已分析任务：{task}"));
    store.update_subagent(run, &subagent)?;
    store.append_message(
        &run.thread_id,
        HarnessMessageRole::Summary,
        subagent.summary.clone().unwrap_or_default(),
        Some(run.id.clone()),
    )?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::SubagentCompleted {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            subagent_id: subagent.id,
            status: HarnessRunStatus::Completed,
        },
    )?;
    Ok(())
}

fn finish_run(
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
    error: Option<String>,
) -> Result<()> {
    if let Some(error) = error {
        run.status = HarnessRunStatus::Failed;
        run.last_error = Some(error.clone());
        store.update_run(&run.thread_id, run)?;
        store.append_run_event(
            &run.thread_id,
            &run.id,
            HarnessEvent::RunFailed {
                thread_id: run.thread_id.clone(),
                run_id: run.id.clone(),
                error,
            },
        )?;
    } else {
        run.status = HarnessRunStatus::Completed;
        if run.summary.is_none() {
            run.summary = Some("run 已完成".to_string());
        }
        store.update_run(&run.thread_id, run)?;
        store.append_run_event(
            &run.thread_id,
            &run.id,
            HarnessEvent::RunCompleted {
                thread_id: run.thread_id.clone(),
                run_id: run.id.clone(),
            },
        )?;
    }

    if let Some(sandbox) = &run.sandbox {
        DockerSandboxProvider {
            image: sandbox.image.clone(),
        }
        .destroy(sandbox)?;
    }
    run.sandbox = None;
    store.update_run(&run.thread_id, run)?;
    Ok(())
}
