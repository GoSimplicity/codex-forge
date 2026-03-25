use std::path::Path;

use anyhow::Result;
use chrono::Utc;

use crate::config::ProjectConfig;
use crate::harness::backend::{
    AgentBackend, BackendTurnRequest, CodexBackend, ToolDescriptor, built_in_tools,
};
use crate::harness::store::HarnessStore;
use crate::harness::types::{
    EvaluationDecision, HarnessEvent, HarnessRunManifest, HarnessRunStatus, SubagentKind,
    TaskNodeRecord,
};

use super::engine::{
    complete_task_node, execute_and_record_tool, fail_task_node, record_tool_planned,
    request_tool_approval, tool_needs_approval,
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_subagent(
    repo_root: &Path,
    config: &ProjectConfig,
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
    node: &TaskNodeRecord,
    kind: SubagentKind,
    memory_context: &str,
    skills_context: &str,
    session_context: &str,
) -> Result<()> {
    let backend = CodexBackend;
    let thread = store.load_thread(&run.thread_id)?;
    let tools = tools_for_subagent(kind);
    let mut subagent = store.append_subagent(
        run,
        kind,
        node.instructions.clone(),
        Some(node.id.clone()),
        run.model.clone(),
        run.thinking_mode,
    )?;
    let mut task_node = node.clone();
    task_node.last_subagent_id = Some(subagent.id.clone());
    store.update_task_node(run, &task_node)?;

    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::SubagentStarted {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            subagent_id: subagent.id.clone(),
            kind,
        },
    )?;

    for turn in 0..config.runtime.max_turns {
        run.turn_count += 1;
        store.update_run(&run.thread_id, run)?;
        let messages = store.list_messages(&run.thread_id)?;
        let hint = subagent_hint(kind, node);
        let request = BackendTurnRequest {
            thread: &thread,
            messages: &messages,
            thinking_mode: run.thinking_mode,
            model: run.model.as_deref(),
            timeout_secs: config.backend.turn_timeout_secs,
            tools: &tools,
            system_hint: &hint,
            memory_context,
            skills_context,
            session_context,
        };
        let envelope = backend
            .execute_turn(
                repo_root,
                &request,
                &subagent.output_path,
                &subagent.log_path,
            )
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

        let assistant_summary = envelope
            .assistant_message
            .clone()
            .unwrap_or_else(|| format!("{kind:?} 节点已执行"));

        let has_tool_calls = !envelope.tool_calls.is_empty();
        if has_tool_calls {
            for call in envelope.tool_calls {
                let record = record_tool_planned(
                    store,
                    run,
                    &call,
                    Some(node.id.clone()),
                    Some(subagent.id.clone()),
                )?;
                if tool_needs_approval(&call) {
                    subagent.status = HarnessRunStatus::WaitingForInput;
                    subagent.summary = Some(format!("等待审批后继续：{}", call.name));
                    store.update_subagent(run, &subagent)?;
                    request_tool_approval(store, &thread, run, node, &call, &record)?;
                    return Ok(());
                }
                execute_and_record_tool(store, &thread, run, &call, &record)?;
            }
        }

        if envelope.final_response || (envelope.assistant_message.is_some() && !has_tool_calls) {
            subagent.status = HarnessRunStatus::Completed;
            subagent.summary = Some(assistant_summary.clone());
            store.update_subagent(run, &subagent)?;
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
            complete_task_node(store, run, node, assistant_summary)?;
            return Ok(());
        }
    }

    let error = format!("子代理 {:?} 达到最大 turn 仍未完成", kind);
    subagent.status = HarnessRunStatus::Failed;
    subagent.error = Some(error.clone());
    store.update_subagent(run, &subagent)?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::SubagentCompleted {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            subagent_id: subagent.id,
            status: HarnessRunStatus::Failed,
        },
    )?;
    fail_task_node(store, run, node, error)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn evaluate_feature(
    repo_root: &Path,
    config: &ProjectConfig,
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
    node: &TaskNodeRecord,
    memory_context: &str,
    skills_context: &str,
    session_context: &str,
) -> Result<EvaluationDecision> {
    let backend = CodexBackend;
    let thread = store.load_thread(&run.thread_id)?;
    let tools = tools_for_subagent(SubagentKind::Evaluator);
    let mut subagent = store.append_subagent(
        run,
        SubagentKind::Evaluator,
        node.instructions.clone(),
        Some(node.id.clone()),
        run.model.clone(),
        run.thinking_mode,
    )?;
    let mut task_node = node.clone();
    task_node.last_subagent_id = Some(subagent.id.clone());
    store.update_task_node(run, &task_node)?;

    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::SubagentStarted {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            subagent_id: subagent.id.clone(),
            kind: SubagentKind::Evaluator,
        },
    )?;

    for turn in 0..config.runtime.max_evaluator_loops {
        run.turn_count += 1;
        store.update_run(&run.thread_id, run)?;
        let messages = store.list_messages(&run.thread_id)?;
        let request = BackendTurnRequest {
            thread: &thread,
            messages: &messages,
            thinking_mode: run.thinking_mode,
            model: run.model.as_deref(),
            timeout_secs: config.backend.turn_timeout_secs,
            tools: &tools,
            system_hint: &subagent_hint(SubagentKind::Evaluator, node),
            memory_context,
            skills_context,
            session_context,
        };
        let envelope = backend
            .execute_turn(
                repo_root,
                &request,
                &subagent.output_path,
                &subagent.log_path,
            )
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

        let has_tool_calls = !envelope.tool_calls.is_empty();
        for call in envelope.tool_calls {
            let record = record_tool_planned(
                store,
                run,
                &call,
                Some(node.id.clone()),
                Some(subagent.id.clone()),
            )?;
            if tool_needs_approval(&call) {
                subagent.status = HarnessRunStatus::WaitingForInput;
                subagent.summary = Some(format!("evaluator 等待审批：{}", call.name));
                store.update_subagent(run, &subagent)?;
                request_tool_approval(store, &thread, run, node, &call, &record)?;
                return Ok(infer_evaluation_from_text(
                    envelope.assistant_message.as_deref(),
                    node.feature_id.clone(),
                ));
            }
            execute_and_record_tool(store, &thread, run, &call, &record)?;
        }

        if has_tool_calls && envelope.evaluation.is_none() && !envelope.final_response {
            continue;
        }

        let decision = envelope.evaluation.unwrap_or_else(|| {
            infer_evaluation_from_text(
                envelope.assistant_message.as_deref(),
                envelope
                    .selected_feature_id
                    .or_else(|| node.feature_id.clone()),
            )
        });
        subagent.status = HarnessRunStatus::Completed;
        subagent.summary = Some(decision.reason.clone());
        store.update_subagent(run, &subagent)?;
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
        return Ok(decision);
    }

    Ok(EvaluationDecision {
        passed: false,
        reason: "evaluator 达到最大轮次，未形成稳定结论".to_string(),
        follow_up_actions: vec!["人工检查当前 feature 状态".to_string()],
        retryable: true,
        feature_id: node.feature_id.clone(),
        created_at: Utc::now(),
    })
}

fn infer_evaluation_from_text(
    text: Option<&str>,
    feature_id: Option<String>,
) -> EvaluationDecision {
    let reason = text
        .unwrap_or("evaluator 未返回结构化结果")
        .trim()
        .to_string();
    let lower = reason.to_lowercase();
    let passed = !(lower.contains("失败")
        || lower.contains("未通过")
        || lower.contains("阻塞")
        || lower.contains("需要修改"));
    let retryable = lower.contains("重试") || lower.contains("再试") || lower.contains("补充验证");
    EvaluationDecision {
        passed,
        reason: if reason.is_empty() {
            "evaluator 未提供结论".to_string()
        } else {
            reason
        },
        follow_up_actions: if retryable {
            vec!["根据 evaluator 结论补充实现或验证".to_string()]
        } else {
            Vec::new()
        },
        retryable,
        feature_id,
        created_at: Utc::now(),
    }
}

fn tools_for_subagent(kind: SubagentKind) -> Vec<ToolDescriptor> {
    let tools = built_in_tools();
    match kind {
        SubagentKind::Planner => tools
            .into_iter()
            .filter(|tool| {
                matches!(
                    tool.name,
                    "list_tree"
                        | "read_file"
                        | "search_files"
                        | "read_contract"
                        | "read_progress"
                        | "list_artifacts"
                        | "read_artifact"
                        | "inspect_run"
                        | "list_skills"
                        | "read_skill"
                )
            })
            .collect(),
        SubagentKind::Generator => tools,
        SubagentKind::Evaluator => tools
            .into_iter()
            .filter(|tool| {
                matches!(
                    tool.name,
                    "list_tree"
                        | "read_file"
                        | "search_files"
                        | "run_shell"
                        | "read_contract"
                        | "read_progress"
                        | "list_artifacts"
                        | "read_artifact"
                        | "inspect_run"
                        | "read_memory"
                        | "list_skills"
                        | "read_skill"
                )
            })
            .collect(),
    }
}

fn subagent_hint(kind: SubagentKind, node: &TaskNodeRecord) -> String {
    let role_hint = match kind {
        SubagentKind::Planner => {
            "你是 planner 子代理，负责读取现状、收敛 contract 和下一步，不直接写代码。"
        }
        SubagentKind::Generator => {
            "你是 generator 子代理，只围绕当前 feature 做最小充分实现，不做无关重构。"
        }
        SubagentKind::Evaluator => {
            "你是 evaluator 子代理，只根据 done_when、工具结果和现状给出通过/失败结论，不直接写代码。"
        }
    };
    format!(
        "{role_hint} 当前节点：{}。节点任务：{}",
        node.title, node.instructions
    )
}
