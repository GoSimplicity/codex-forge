use std::path::Path;

use anyhow::Result;

use crate::config::ProjectConfig;

use super::subagent::execute_subagent;
use crate::harness::backend::{AgentBackend, BackendTurnRequest, CodexBackend, built_in_tools};
use crate::harness::sandbox::DockerSandboxProvider;
use crate::harness::store::HarnessStore;
use crate::harness::tools::{
    approval_reason, execute_tool_call, mark_tool_resolution, tool_requires_approval,
};
use crate::harness::types::{
    HarnessEvent, HarnessMessageRole, HarnessRunManifest, HarnessRunStatus, HarnessThreadManifest,
    ToolCallRecord, ToolCallRequest, ToolCallStatus,
};

pub(super) async fn run_execution(
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
        let has_tool_calls = !envelope.tool_calls.is_empty();
        let has_subagent_calls = !envelope.subagent_calls.is_empty();
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

        if has_tool_calls {
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

        if envelope.final_response
            || (assistant_message.is_some()
                && !has_tool_calls
                && !has_subagent_calls
                && run.turn_count >= 1)
        {
            let final_summary = assistant_message
                .as_ref()
                .map(|message| first_non_empty_line(&message.content).to_string())
                .filter(|value| !value.is_empty());
            finish_run(store, run, final_summary, None)?;
            return Ok(());
        }
    }

    finish_run(
        store,
        run,
        None,
        Some("达到最大 turn 次数仍未完成".to_string()),
    )
}

pub(super) fn execute_and_record_tool(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &mut HarnessRunManifest,
    call: &ToolCallRequest,
    record: &ToolCallRecord,
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

fn finish_run(
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
    final_summary: Option<String>,
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
        run.summary = final_summary
            .or_else(|| run.summary.clone())
            .or(Some("run 已完成".to_string()));
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

fn first_non_empty_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("")
}
