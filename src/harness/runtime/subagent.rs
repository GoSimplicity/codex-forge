use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use serde_json::Value;

use crate::config::AppConfig;
use crate::harness::backend::{
    AgentBackend, BackendTurnRequest, ResolvedBackend, ToolDescriptor, built_in_tools,
};
use crate::harness::store::HarnessStore;
use crate::harness::tools::normalize_tool_call;
use crate::harness::types::{
    ArtifactKind, EvaluationDecision, HarnessEvent, HarnessMessageRole, HarnessRunManifest,
    HarnessRunStatus, SubagentKind, TaskNodeRecord, ToolCallRecord, ToolCallRequest,
    ToolCallStatus, TurnEnvelope,
};

use super::engine::{
    ToolExecutionOutcome, complete_task_node, execute_and_record_tool, fail_task_node,
    record_tool_planned, request_tool_approval, tool_needs_approval,
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_subagent(
    repo_root: &Path,
    config: &AppConfig,
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
    node: &TaskNodeRecord,
    kind: SubagentKind,
    memory_context: &str,
    skills_context: &str,
    session_context: &str,
) -> Result<()> {
    let backend = ResolvedBackend::from_config(&config.backend)?;
    let thread = store.load_thread(&run.thread_id)?;
    let tools = tools_for_subagent(kind);
    let max_turns = subagent_turn_budget(config, kind);
    let execution_root = run
        .sandbox
        .as_ref()
        .map(|sandbox| sandbox.repo_workdir.clone())
        .unwrap_or_else(|| repo_root.to_path_buf());
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

    for turn in 0..max_turns {
        run.turn_count += 1;
        store.update_run(&run.thread_id, run)?;
        let messages = store.list_messages(&run.thread_id)?;
        let hint = subagent_hint(kind, node);
        let request = BackendTurnRequest {
            thread: &thread,
            execution_root: &execution_root,
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
        let envelope = match backend
            .execute_turn(
                &execution_root,
                &request,
                &subagent.output_path,
                &subagent.log_path,
            )
            .await
        {
            Ok(envelope) => envelope,
            Err(error) if backend_error_is_recoverable(&error) && turn + 1 < max_turns => {
                let detail = format!("backend turn 可恢复异常：{}", error);
                store.append_message(
                    &run.thread_id,
                    HarnessMessageRole::System,
                    detail.clone(),
                    Some(run.id.clone()),
                )?;
                store.append_run_event(
                    &run.thread_id,
                    &run.id,
                    HarnessEvent::RecoverableFailureDetected {
                        thread_id: run.thread_id.clone(),
                        run_id: run.id.clone(),
                        source: "backend_turn".to_string(),
                        detail,
                    },
                )?;
                continue;
            }
            Err(error) => return Err(error),
        };
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
        let handoff_requested = handoff_to_evaluator_requested(&envelope);
        let mut executed_mutating_tool = false;
        if has_tool_calls {
            for raw_call in envelope.tool_calls.clone() {
                let call = normalize_tool_call(&raw_call);
                let record = record_tool_planned(
                    store,
                    run,
                    &call,
                    Some(node.id.clone()),
                    Some(subagent.id.clone()),
                )?;
                if subagent_tool_needs_approval(config, &call) {
                    subagent.status = HarnessRunStatus::WaitingForInput;
                    subagent.summary = Some(format!("等待审批后继续：{}", call.name));
                    store.update_subagent(run, &subagent)?;
                    request_tool_approval(store, &thread, run, node, &call, &record)?;
                    return Ok(());
                }
                match execute_and_record_tool(store, &thread, run, &call, &record)? {
                    ToolExecutionOutcome::Succeeded => {
                        if tool_call_creates_implementation_evidence(&call) {
                            executed_mutating_tool = true;
                        }
                    }
                    ToolExecutionOutcome::RecoverableFailure => {}
                }
            }
        }

        if let Some(feedback) = invalid_subagent_delegation_feedback(kind, &envelope) {
            store.append_message(
                &run.thread_id,
                HarnessMessageRole::System,
                feedback,
                Some(run.id.clone()),
            )?;
            subagent.summary = Some("子代理尝试继续派生，已要求直接收敛结论".to_string());
            subagent.updated_at = Utc::now();
            store.update_subagent(run, &subagent)?;
            continue;
        }

        if kind == SubagentKind::Generator {
            let requires_mutating_evidence = node_requires_mutating_evidence(node);
            let has_evidence = if requires_mutating_evidence {
                node_has_implementation_evidence(store, run, node)?
            } else {
                node_has_tool_result_evidence(store, run, node)?
            };
            if executed_mutating_tool || (!requires_mutating_evidence && has_evidence) {
                complete_subagent_success(store, run, node, &mut subagent, assistant_summary)?;
                return Ok(());
            }
            if generator_requires_real_changes_feedback(
                requires_mutating_evidence,
                has_tool_calls,
                executed_mutating_tool,
            ) {
                request_generator_real_changes(
                    store,
                    run,
                    node,
                    &mut subagent,
                    &assistant_summary,
                )?;
                continue;
            }
            if handoff_requested {
                if has_evidence {
                    complete_subagent_success(store, run, node, &mut subagent, assistant_summary)?;
                    return Ok(());
                }
                request_generator_real_changes(
                    store,
                    run,
                    node,
                    &mut subagent,
                    &assistant_summary,
                )?;
                continue;
            }
            if envelope.final_response || (envelope.assistant_message.is_some() && !has_tool_calls)
            {
                request_generator_real_changes(
                    store,
                    run,
                    node,
                    &mut subagent,
                    &assistant_summary,
                )?;
                continue;
            }
        } else if kind == SubagentKind::Planner {
            let has_evidence = planner_has_minimum_evidence(store, run, node)?;
            let evidence_insufficient = planner_reports_insufficient_evidence(&assistant_summary);
            if envelope.final_response || envelope.assistant_message.is_some() {
                if has_evidence || evidence_insufficient {
                    if !has_evidence {
                        store.append_run_event(
                            &run.thread_id,
                            &run.id,
                            HarnessEvent::EvidenceInsufficient {
                                thread_id: run.thread_id.clone(),
                                run_id: run.id.clone(),
                                task_node_id: node.id.clone(),
                                detail: assistant_summary.clone(),
                            },
                        )?;
                    }
                    complete_subagent_success(store, run, node, &mut subagent, assistant_summary)?;
                    return Ok(());
                }
                request_planner_more_evidence(store, run, node, &mut subagent, &assistant_summary)?;
                continue;
            }
        } else if envelope.final_response
            || (envelope.assistant_message.is_some() && !has_tool_calls)
        {
            complete_subagent_success(store, run, node, &mut subagent, assistant_summary)?;
            return Ok(());
        }
    }

    let error = subagent_max_turn_error(kind, max_turns, &node.title);
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

fn subagent_turn_budget(config: &AppConfig, kind: SubagentKind) -> usize {
    match kind {
        SubagentKind::Generator => config.runtime.max_generator_turns.max(1),
        SubagentKind::Planner | SubagentKind::Evaluator => config.runtime.max_turns.max(1),
    }
}

fn subagent_max_turn_error(kind: SubagentKind, max_turns: usize, node_title: &str) -> String {
    match kind {
        SubagentKind::Generator => format!(
            "子代理 {:?} 在当前 feature `{}` 上达到最大 turn 仍未完成（已执行 {max_turns}/{max_turns} 轮，可调大 runtime.max_generator_turns；如需继续，请重试当前节点）。",
            kind, node_title
        ),
        SubagentKind::Planner => format!(
            "子代理 Planner 在节点 `{}` 上经过 {max_turns}/{max_turns} 轮后仍未收敛，通常是事实证据不足或一直没有直接给出结论；请补充入口/实现文件证据后重试，或明确写“证据不足”。",
            node_title
        ),
        SubagentKind::Evaluator => format!(
            "子代理 Evaluator 在节点 `{}` 上经过 {max_turns}/{max_turns} 轮后仍未完成评估；请直接基于现有 evidence 给出通过/失败结论。",
            node_title
        ),
    }
}

fn complete_subagent_success(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
    subagent: &mut crate::harness::types::SubagentRecord,
    summary: String,
) -> Result<()> {
    subagent.status = HarnessRunStatus::Completed;
    subagent.summary = Some(summary.clone());
    subagent.updated_at = Utc::now();
    store.update_subagent(run, subagent)?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::SubagentCompleted {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            subagent_id: subagent.id.clone(),
            status: HarnessRunStatus::Completed,
        },
    )?;
    complete_task_node(store, run, node, summary)
}

fn request_generator_real_changes(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
    subagent: &mut crate::harness::types::SubagentRecord,
    assistant_summary: &str,
) -> Result<()> {
    let feedback = format!(
        "generator 反馈：当前节点 `{}` 还没有任何已验证的实现落地证据，不能按“已完成”继续。上一轮摘要：{}。如果你已经读过文档或目录，不要重复调用 read_file / list_tree / search_files 读取同一份上下文；下一轮只允许实际调用 write_file / apply_patch / run_shell 之一来落地修改，审批会由 harness 挂起并恢复。如果你判断当前节点确实无法实施，就直接 final_response=true 明确说明阻塞原因，但不要继续只读探索。",
        node.title, assistant_summary
    );
    store.append_message(
        &run.thread_id,
        HarnessMessageRole::System,
        feedback.clone(),
        Some(run.id.clone()),
    )?;
    subagent.summary = Some("缺少实际落地证据，继续要求 generator 给出真实修改".to_string());
    subagent.updated_at = Utc::now();
    store.update_subagent(run, subagent)
}

fn generator_requires_real_changes_feedback(
    requires_mutating_evidence: bool,
    has_tool_calls: bool,
    executed_mutating_tool: bool,
) -> bool {
    requires_mutating_evidence && has_tool_calls && !executed_mutating_tool
}

fn request_planner_more_evidence(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
    subagent: &mut crate::harness::types::SubagentRecord,
    assistant_summary: &str,
) -> Result<()> {
    let feedback = format!(
        "planner 反馈：当前节点 `{}` 的事实证据还不够，不能宣称“已完成事实采集”。上一轮摘要：{}。下一轮至少读取入口/配置文件和一个实现文件；如果仓库中确实证据不足，请明确写“证据不足”。",
        node.title, assistant_summary
    );
    store.append_message(
        &run.thread_id,
        HarnessMessageRole::System,
        feedback,
        Some(run.id.clone()),
    )?;
    subagent.summary = Some("缺少最小证据，继续要求 planner 补充探索".to_string());
    subagent.updated_at = Utc::now();
    store.update_subagent(run, subagent)
}

fn backend_error_is_recoverable(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("疑似截断")
        || text.contains("解析")
        || text.contains("EOF")
        || text.contains("empty")
}

fn handoff_to_evaluator_requested(envelope: &TurnEnvelope) -> bool {
    envelope.needs_handoff
        || envelope
            .subagent_calls
            .iter()
            .any(|call| call.kind == SubagentKind::Evaluator)
}

fn invalid_subagent_delegation_feedback(
    kind: SubagentKind,
    envelope: &TurnEnvelope,
) -> Option<String> {
    if envelope.subagent_calls.is_empty() {
        return None;
    }

    let requested = envelope
        .subagent_calls
        .iter()
        .map(|call| match call.kind {
            SubagentKind::Planner => "planner",
            SubagentKind::Generator => "generator",
            SubagentKind::Evaluator => "evaluator",
        })
        .collect::<Vec<_>>()
        .join(", ");

    match kind {
        SubagentKind::Planner => Some(format!(
            "planner 反馈：当前 harness 不支持在 planner 子代理内部继续使用 subagent_calls（请求了：{requested}）。下一轮请直接用 tool_calls 补充事实，或 final_response=true 直接给出结论；如果仓库证据不足，请明确写“证据不足”。"
        )),
        SubagentKind::Evaluator => Some(format!(
            "evaluator 反馈：当前 harness 不支持在 evaluator 子代理内部继续使用 subagent_calls（请求了：{requested}）。请直接基于现有证据返回 evaluation 结论。"
        )),
        SubagentKind::Generator => envelope
            .subagent_calls
            .iter()
            .any(|call| call.kind != SubagentKind::Evaluator)
            .then(|| {
                format!(
                    "generator 反馈：当前 harness 不支持 generator 继续派生 {requested}。如果需要验收，只设置 needs_handoff=true 或请求 evaluator，不要再派生 planner/generator。"
                )
            }),
    }
}

fn planner_reports_insufficient_evidence(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("证据不足")
        || lower.contains("信息不足")
        || lower.contains("insufficient evidence")
        || lower.contains("not enough evidence")
}

fn planner_has_minimum_evidence(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
) -> Result<bool> {
    let tool_calls = store.list_tool_calls(run)?;
    let mut has_context_source = false;
    let mut has_implementation = false;
    let requires_implementation = planner_requires_implementation_evidence(node);
    for record in tool_calls {
        if record.task_node_id.as_deref() != Some(node.id.as_str())
            || record.status != ToolCallStatus::Succeeded
        {
            continue;
        }
        match record.name.as_str() {
            "read_file" => {
                let path = record
                    .arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if path_looks_like_planner_context(path) {
                    has_context_source = true;
                }
                if path_looks_like_implementation(path) {
                    has_implementation = true;
                }
            }
            "search_files" => {
                has_context_source = true;
            }
            "list_tree" => {
                has_context_source = true;
            }
            _ => {}
        }
    }
    Ok(if requires_implementation {
        has_context_source && has_implementation
    } else {
        has_context_source
    })
}

fn planner_requires_implementation_evidence(node: &TaskNodeRecord) -> bool {
    let text = format!("{}\n{}", node.title, node.instructions).to_lowercase();
    ![
        "根据项目文档",
        "根据文档",
        "项目文档",
        "plan.md",
        "readme",
        "文档",
        "架构",
        "方案",
        "设计",
        "spec",
        "骨架",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn path_looks_like_planner_context(path: &str) -> bool {
    let lower = path.to_lowercase();
    matches!(
        lower.as_str(),
        "cargo.toml"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "pyproject.toml"
            | "go.mod"
            | "makefile"
            | "readme.md"
            | "plan.md"
    ) || lower.ends_with("/cargo.toml")
        || lower.ends_with("/package.json")
        || lower.ends_with("/pyproject.toml")
        || lower.ends_with("/go.mod")
        || lower.ends_with("/plan.md")
        || lower.ends_with("/readme.md")
        || lower.ends_with(".md")
        || lower.ends_with("/main.rs")
        || lower.ends_with("/lib.rs")
        || lower.ends_with("/main.go")
        || lower.ends_with("/main.py")
        || lower.ends_with("/main.ts")
        || lower.ends_with("/main.tsx")
}

fn path_looks_like_implementation(path: &str) -> bool {
    let lower = path.to_lowercase();
    let is_source = [".rs", ".go", ".py", ".ts", ".tsx", ".js", ".jsx", ".php"]
        .iter()
        .any(|suffix| lower.ends_with(suffix));
    is_source && !lower.ends_with("readme.md")
}

fn node_has_implementation_evidence(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
) -> Result<bool> {
    let tool_calls = store.list_tool_calls(run)?;
    if tool_calls.iter().any(|record| {
        record.task_node_id.as_deref() == Some(node.id.as_str())
            && record.status == ToolCallStatus::Succeeded
            && tool_record_creates_implementation_evidence(record)
    }) {
        return Ok(true);
    }

    let artifacts = store.list_artifacts(Some(&run.thread_id), Some(&run.id))?;
    Ok(artifacts.iter().any(|artifact| {
        artifact.task_node_id.as_deref() == Some(node.id.as_str())
            && artifact.kind == ArtifactKind::File
    }))
}

fn node_has_tool_result_evidence(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
) -> Result<bool> {
    if node_has_implementation_evidence(store, run, node)? {
        return Ok(true);
    }
    let tool_calls = store.list_tool_calls(run)?;
    Ok(tool_calls.iter().any(|record| {
        record.task_node_id.as_deref() == Some(node.id.as_str())
            && record.status == ToolCallStatus::Succeeded
    }))
}

fn node_requires_mutating_evidence(node: &TaskNodeRecord) -> bool {
    let text = format!("{}\n{}", node.title, node.instructions).to_lowercase();
    if [
        "不要修改",
        "不修改",
        "无需修改",
        "不需要修改",
        "不要改动",
        "只读",
        "read-only",
        "readonly",
    ]
    .iter()
    .any(|marker| text.contains(marker))
    {
        return false;
    }

    [
        "修改",
        "实现",
        "修复",
        "重构",
        "生成",
        "创建",
        "写入",
        "落地",
        "patch",
        "write",
        "generate",
        "create",
        "implement",
        "fix",
        "refactor",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn tool_record_creates_implementation_evidence(record: &ToolCallRecord) -> bool {
    match record.name.as_str() {
        "write_file" | "apply_patch" => true,
        "run_shell" => shell_command_looks_mutating(
            record
                .arguments
                .get("command")
                .and_then(Value::as_str)
                .or_else(|| record.arguments.get("cmd").and_then(Value::as_str))
                .unwrap_or_default(),
        ),
        _ => false,
    }
}

fn tool_call_creates_implementation_evidence(call: &ToolCallRequest) -> bool {
    match call.name.as_str() {
        "write_file" | "apply_patch" => true,
        "run_shell" => shell_command_looks_mutating(
            call.arguments
                .get("command")
                .and_then(Value::as_str)
                .or_else(|| call.arguments.get("cmd").and_then(Value::as_str))
                .unwrap_or_default(),
        ),
        _ => false,
    }
}

fn shell_command_looks_mutating(command: &str) -> bool {
    let lower = command.to_lowercase();
    let markers = [
        ">",
        ">>",
        "mkdir ",
        "touch ",
        "rm ",
        "mv ",
        "cp ",
        "install ",
        "tee ",
        "sed -i",
        "perl -i",
        "patch ",
        "git apply",
        "cat >",
        "cat >>",
    ];
    markers.iter().any(|marker| lower.contains(marker))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn evaluate_feature(
    repo_root: &Path,
    config: &AppConfig,
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
    node: &TaskNodeRecord,
    memory_context: &str,
    skills_context: &str,
    session_context: &str,
) -> Result<EvaluationDecision> {
    let backend = ResolvedBackend::from_config(&config.backend)?;
    let thread = store.load_thread(&run.thread_id)?;
    let tools = tools_for_subagent(SubagentKind::Evaluator);
    let execution_root = run
        .sandbox
        .as_ref()
        .map(|sandbox| sandbox.repo_workdir.clone())
        .unwrap_or_else(|| repo_root.to_path_buf());
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
            execution_root: &execution_root,
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
        let envelope = match backend
            .execute_turn(
                &execution_root,
                &request,
                &subagent.output_path,
                &subagent.log_path,
            )
            .await
        {
            Ok(envelope) => envelope,
            Err(error)
                if backend_error_is_recoverable(&error)
                    && turn + 1 < config.runtime.max_evaluator_loops =>
            {
                let detail = format!("evaluator backend 可恢复异常：{}", error);
                store.append_message(
                    &run.thread_id,
                    HarnessMessageRole::System,
                    detail.clone(),
                    Some(run.id.clone()),
                )?;
                store.append_run_event(
                    &run.thread_id,
                    &run.id,
                    HarnessEvent::RecoverableFailureDetected {
                        thread_id: run.thread_id.clone(),
                        run_id: run.id.clone(),
                        source: "evaluator_backend_turn".to_string(),
                        detail,
                    },
                )?;
                continue;
            }
            Err(error) => return Err(error),
        };
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
        for raw_call in envelope.tool_calls {
            let call = normalize_tool_call(&raw_call);
            let record = record_tool_planned(
                store,
                run,
                &call,
                Some(node.id.clone()),
                Some(subagent.id.clone()),
            )?;
            if subagent_tool_needs_approval(config, &call) {
                subagent.status = HarnessRunStatus::WaitingForInput;
                subagent.summary = Some(format!("evaluator 等待审批：{}", call.name));
                store.update_subagent(run, &subagent)?;
                request_tool_approval(store, &thread, run, node, &call, &record)?;
                return Ok(infer_evaluation_from_text(
                    envelope.assistant_message.as_deref(),
                    node.feature_id.clone(),
                ));
            }
            let _ = execute_and_record_tool(store, &thread, run, &call, &record)?;
        }

        if has_tool_calls {
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
            "你是 planner 子代理，负责读取现状、收敛 contract 和下一步，不直接写代码，也不要继续使用 subagent_calls 派生其他子代理。拿到最小证据后就直接总结，不要为了“更完整”反复探索。"
        }
        SubagentKind::Generator => {
            "你是 generator 子代理，只围绕当前 feature 做最小充分实现，不做无关重构。对于搭骨架/创建文件/修复代码这类落地任务，最多做少量只读探索后就必须直接发起 write_file / apply_patch / run_shell；即使会触发审批也要先提交，不要停留在 read_file / list_tree / search_files 的循环里。如需交给 evaluator 验收，使用 needs_handoff=true，不要继续派生 planner/generator。"
        }
        SubagentKind::Evaluator => {
            "你是 evaluator 子代理，只根据 done_when、工具结果和现状给出通过/失败结论，不直接写代码，也不要继续使用 subagent_calls 派生其他子代理。"
        }
    };
    format!(
        "{role_hint} 当前节点：{}。节点任务：{}",
        node.title, node.instructions
    )
}

fn subagent_tool_needs_approval(config: &AppConfig, call: &ToolCallRequest) -> bool {
    tool_needs_approval(config, call)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tempfile::TempDir;

    use crate::harness::store::HarnessStore;
    use crate::harness::types::{TaskNodeKind, TaskNodeRecord, TaskNodeStatus, ToolCallRequest};
    use crate::model::ThinkingMode;

    use super::{
        generator_requires_real_changes_feedback, handoff_to_evaluator_requested,
        invalid_subagent_delegation_feedback, node_has_implementation_evidence,
        node_requires_mutating_evidence, planner_requires_implementation_evidence,
        shell_command_looks_mutating, subagent_hint, subagent_max_turn_error,
        subagent_tool_needs_approval, subagent_turn_budget,
        tool_call_creates_implementation_evidence,
    };
    use crate::config::AppConfig;

    fn make_node() -> TaskNodeRecord {
        let now = Utc::now();
        TaskNodeRecord {
            id: "task-1".to_string(),
            graph_id: "graph-1".to_string(),
            thread_id: "thread-1".to_string(),
            run_id: "run-1".to_string(),
            kind: TaskNodeKind::ExecuteFeature,
            title: "执行 feature".to_string(),
            instructions: "demo".to_string(),
            depends_on: Vec::new(),
            position: 0,
            status: TaskNodeStatus::Running,
            created_at: now,
            updated_at: now,
            started_at: Some(now),
            completed_at: None,
            output_summary: None,
            error: None,
            last_subagent_id: Some("subagent-1".to_string()),
            attempt_count: 1,
            feature_id: Some("feature-1".to_string()),
        }
    }

    #[test]
    fn shell_mutation_heuristic_distinguishes_pwd() {
        assert!(shell_command_looks_mutating(
            "mkdir -p out && printf artifact > out/result.txt"
        ));
        assert!(!shell_command_looks_mutating("pwd"));
    }

    #[test]
    fn handoff_detection_respects_evaluator_request() {
        let envelope = crate::harness::types::TurnEnvelope {
            assistant_message: Some("done".to_string()),
            tool_calls: Vec::new(),
            subagent_calls: vec![crate::harness::types::BackendSubagentCall {
                kind: crate::harness::types::SubagentKind::Evaluator,
                task: "验收".to_string(),
            }],
            final_response: false,
            state_update: None,
            selected_feature_id: None,
            evaluation: None,
            needs_handoff: false,
        };
        assert!(handoff_to_evaluator_requested(&envelope));
    }

    #[test]
    fn planner_subagent_delegation_is_rejected() {
        let envelope = crate::harness::types::TurnEnvelope {
            assistant_message: Some("继续拆分".to_string()),
            tool_calls: Vec::new(),
            subagent_calls: vec![crate::harness::types::BackendSubagentCall {
                kind: crate::harness::types::SubagentKind::Planner,
                task: "继续规划".to_string(),
            }],
            final_response: false,
            state_update: None,
            selected_feature_id: None,
            evaluation: None,
            needs_handoff: false,
        };
        let feedback = invalid_subagent_delegation_feedback(
            crate::harness::types::SubagentKind::Planner,
            &envelope,
        )
        .expect("planner feedback");
        assert!(feedback.contains("不支持在 planner 子代理内部继续使用 subagent_calls"));
        assert!(feedback.contains("证据不足"));
    }

    #[test]
    fn generator_allows_evaluator_handoff_without_feedback() {
        let envelope = crate::harness::types::TurnEnvelope {
            assistant_message: Some("请验收".to_string()),
            tool_calls: Vec::new(),
            subagent_calls: vec![crate::harness::types::BackendSubagentCall {
                kind: crate::harness::types::SubagentKind::Evaluator,
                task: "验收".to_string(),
            }],
            final_response: false,
            state_update: None,
            selected_feature_id: None,
            evaluation: None,
            needs_handoff: false,
        };
        assert!(
            invalid_subagent_delegation_feedback(
                crate::harness::types::SubagentKind::Generator,
                &envelope,
            )
            .is_none()
        );
    }

    #[test]
    fn planner_hint_forbids_recursive_subagents() {
        let hint = subagent_hint(crate::harness::types::SubagentKind::Planner, &make_node());
        assert!(hint.contains("不要继续使用 subagent_calls"));
    }

    #[test]
    fn document_driven_planner_task_does_not_require_source_files() {
        let mut node = make_node();
        node.title = "采集事实".to_string();
        node.instructions =
            "围绕目标收集证据并形成结论：根据项目文档，给我完成项目基本骨架，以支持后续开发"
                .to_string();
        assert!(!planner_requires_implementation_evidence(&node));
    }

    #[test]
    fn tool_call_evidence_requires_mutating_shell() {
        assert!(tool_call_creates_implementation_evidence(
            &ToolCallRequest {
                name: "write_file".to_string(),
                arguments: serde_json::json!({"path":"demo.txt","content":"ok\n"}),
            }
        ));
        assert!(!tool_call_creates_implementation_evidence(
            &ToolCallRequest {
                name: "run_shell".to_string(),
                arguments: serde_json::json!({"command":"pwd"}),
            }
        ));
    }

    #[test]
    fn generator_uses_dedicated_turn_budget() {
        let config = AppConfig::default();
        assert_eq!(
            subagent_turn_budget(&config, crate::harness::types::SubagentKind::Generator),
            config.runtime.max_generator_turns
        );
        assert_eq!(
            subagent_turn_budget(&config, crate::harness::types::SubagentKind::Planner),
            config.runtime.max_turns
        );
    }

    #[test]
    fn generator_max_turn_error_points_to_config_override() {
        let error = subagent_max_turn_error(
            crate::harness::types::SubagentKind::Generator,
            16,
            "执行 feature",
        );
        assert!(error.contains("runtime.max_generator_turns"));
        assert!(error.contains("16/16"));
    }

    #[test]
    fn planner_max_turn_error_mentions_evidence_and_resolution() {
        let error = subagent_max_turn_error(
            crate::harness::types::SubagentKind::Planner,
            6,
            "规划执行路径",
        );
        assert!(error.contains("6/6"));
        assert!(error.contains("事实证据不足"));
        assert!(error.contains("证据不足"));
    }

    #[test]
    fn readonly_generator_tools_trigger_real_change_feedback() {
        assert!(generator_requires_real_changes_feedback(true, true, false));
        assert!(!generator_requires_real_changes_feedback(true, true, true));
        assert!(!generator_requires_real_changes_feedback(
            true, false, false
        ));
        assert!(!generator_requires_real_changes_feedback(
            false, true, false
        ));
    }

    #[test]
    fn readonly_run_shell_is_auto_approved_by_default() {
        let config = AppConfig::default();
        assert!(!subagent_tool_needs_approval(
            &config,
            &ToolCallRequest {
                name: "run_shell".to_string(),
                arguments: serde_json::json!({"command":"mkdir -p out && touch out/x"}),
            }
        ));
    }

    #[test]
    fn manual_approval_can_be_enabled_for_mutating_tools() {
        let mut config = AppConfig::default();
        config.runtime.require_tool_approval = true;
        assert!(!subagent_tool_needs_approval(
            &config,
            &ToolCallRequest {
                name: "run_shell".to_string(),
                arguments: serde_json::json!({"command":"cat README.md"}),
            }
        ));
        assert!(subagent_tool_needs_approval(
            &config,
            &ToolCallRequest {
                name: "run_shell".to_string(),
                arguments: serde_json::json!({"command":"mkdir -p out && touch out/x"}),
            }
        ));
    }

    #[test]
    fn readonly_explanation_node_does_not_require_mutating_evidence() {
        let mut node = make_node();
        node.title = "说明 file-a.txt 当前内容。不要修改任何文件。".to_string();
        node.instructions = "只读说明，不要修改".to_string();
        assert!(!node_requires_mutating_evidence(&node));
    }

    #[test]
    fn node_evidence_comes_from_succeeded_mutating_tools() {
        let dir = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(dir.path(), crate::config::BackendProvider::Codex);
        let thread = store.create_thread(Some("demo")).expect("thread");
        let run = store
            .create_run(
                &thread.id,
                None,
                ThinkingMode::Balanced,
                crate::harness::types::AgentBackendKind::Codex,
            )
            .expect("run");
        let call = ToolCallRequest {
            name: "write_file".to_string(),
            arguments: serde_json::json!({"path":"demo.txt","content":"ok\n"}),
        };
        let mut record = store
            .append_tool_call(
                &run,
                &call,
                Some("task-1".to_string()),
                Some("subagent-1".to_string()),
            )
            .expect("append");
        record.status = crate::harness::types::ToolCallStatus::Succeeded;
        store.update_tool_call(&run, &record).expect("update");

        assert!(node_has_implementation_evidence(&store, &run, &make_node()).expect("evidence"));
    }
}
