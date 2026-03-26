use std::fs;
use std::path::Path;

use anyhow::{Result, anyhow};
use chrono::Utc;

use crate::config::AppConfig;

use super::subagent::{evaluate_feature, execute_subagent};
use crate::harness::sandbox::DockerSandboxProvider;
use crate::harness::skills::SkillAdapter;
use crate::harness::store::HarnessStore;
use crate::harness::tools::{
    approval_reason, execute_tool_call, mark_tool_resolution, normalize_tool_call,
    tool_requires_approval,
};
use crate::harness::types::{
    AcceptanceCriterion, EvaluationDecision, ExecutionContract, FeatureSlice, FeatureSliceStatus,
    HarnessEvent, HarnessMessageRole, HarnessRunManifest, HarnessRunStatus, HarnessThreadManifest,
    MemoryLayer, ProgressLedger, SubagentKind, TaskGraphStrategy, TaskNodeKind, TaskNodeRecord,
    TaskNodeStatus, ToolCallRecord, ToolCallRequest, ToolCallStatus,
};

const PLAN_CONFIRMATION_MARKER: &str = "用户已确认执行计划";

pub(super) enum ToolExecutionOutcome {
    Succeeded,
    RecoverableFailure,
}

pub(super) async fn run_execution(
    repo_root: &Path,
    config: &AppConfig,
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
) -> Result<()> {
    let thread = store.load_thread(&run.thread_id)?;
    ensure_sandbox_ready(repo_root, config, store, run)?;
    ensure_task_graph(config, store, run)?;

    run.status = HarnessRunStatus::Running;
    run.blocked_reason = None;
    store.update_run(&run.thread_id, run)?;
    if run.turn_count == 0 {
        store.append_run_event(
            &run.thread_id,
            &run.id,
            HarnessEvent::RunStarted {
                thread_id: run.thread_id.clone(),
                run_id: run.id.clone(),
            },
        )?;
    }

    let max_cycles = config.runtime.max_turns.saturating_mul(4).max(16);
    for _ in 0..max_cycles {
        let mut nodes = store.list_task_nodes(run)?;
        promote_ready_nodes(store, run, &mut nodes)?;

        if let Some(node) = next_executable_node(run, &nodes) {
            let memory_context = render_memory_context(store, &thread.id);
            let skills_context = render_skills_context(config.backend.provider);
            let session_context =
                render_session_context(store, &thread.id, config.runtime.bootstrap_message_limit);
            run.active_task_node_id = Some(node.id.clone());
            run.status = if node.status == TaskNodeStatus::WaitingForInput {
                HarnessRunStatus::WaitingForInput
            } else {
                HarnessRunStatus::Running
            };
            store.update_run(&run.thread_id, run)?;

            if node.status == TaskNodeStatus::WaitingForInput {
                run.blocked_reason = Some(format!("等待节点 `{}` 的人工输入", node.title));
                store.update_run(&run.thread_id, run)?;
                return Ok(());
            }

            if let Err(error) = execute_task_node(
                repo_root,
                config,
                store,
                &thread,
                run,
                &node,
                &memory_context,
                &skills_context,
                &session_context,
            )
            .await
            {
                let error_text = format!("{error:#}");
                finish_run(
                    store,
                    run,
                    Some(first_non_empty_line(&error_text).to_string()),
                    Some(error_text),
                )?;
                return Err(error);
            }
            *run = store.load_run(&run.thread_id, &run.id)?;
            if matches!(
                run.status,
                HarnessRunStatus::Completed
                    | HarnessRunStatus::Failed
                    | HarnessRunStatus::Cancelled
                    | HarnessRunStatus::WaitingForInput
            ) {
                return Ok(());
            }
            continue;
        }

        if let Some(failed) = nodes
            .iter()
            .find(|node| node.status == TaskNodeStatus::Failed)
            .cloned()
        {
            finish_run(
                store,
                run,
                failed.output_summary.clone(),
                Some(
                    failed
                        .error
                        .clone()
                        .unwrap_or_else(|| format!("节点 `{}` 失败", failed.title)),
                ),
            )?;
            return Ok(());
        }

        if nodes.iter().all(|node| {
            matches!(
                node.status,
                TaskNodeStatus::Completed | TaskNodeStatus::Skipped
            )
        }) {
            finish_run(store, run, run.summary.clone(), None)?;
            return Ok(());
        }

        finish_run(
            store,
            run,
            None,
            Some("任务图没有可执行节点，执行已阻塞".to_string()),
        )?;
        return Ok(());
    }

    finish_run(
        store,
        run,
        None,
        Some("达到最大 turn 次数仍未完成".to_string()),
    )
}

pub(super) fn ensure_sandbox_ready(
    repo_root: &Path,
    config: &AppConfig,
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
) -> Result<()> {
    if run.sandbox.is_some() {
        return Ok(());
    }

    let sandbox = DockerSandboxProvider::from(&config.sandbox).start(repo_root, run)?;
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
    Ok(())
}

fn ensure_task_graph(
    config: &AppConfig,
    store: &HarnessStore,
    run: &HarnessRunManifest,
) -> Result<()> {
    if run.task_graph_path.exists() && run.task_nodes_path.exists() {
        return Ok(());
    }

    let messages = store.list_messages(&run.thread_id)?;
    let goal = messages
        .iter()
        .rev()
        .find(|message| message.role == HarnessMessageRole::User)
        .map(|message| message.content.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "处理当前任务".to_string());
    let strategy = infer_strategy(config, &goal);
    let graph = store.create_task_graph(
        run,
        goal.clone(),
        strategy,
        default_success_criteria(&goal, strategy),
    )?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::TaskGraphCreated {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            graph_id: graph.id.clone(),
            strategy,
        },
    )?;

    let (kind, title, instructions) = match strategy {
        TaskGraphStrategy::Research => (
            TaskNodeKind::Plan,
            "规划执行路径".to_string(),
            format!("为当前目标建立研究路径，并明确完成标准：{goal}"),
        ),
        TaskGraphStrategy::LongRunningDelivery => (
            TaskNodeKind::Initialize,
            "初始化长期任务".to_string(),
            format!("为长期交付任务准备 bootstrap、memory 与执行上下文：{goal}"),
        ),
    };
    let node = store.append_task_node(
        run,
        &graph,
        kind,
        title,
        instructions,
        Vec::new(),
        0,
        TaskNodeStatus::Ready,
    )?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::TaskNodeReady {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            task_node_id: node.id,
            kind,
        },
    )?;
    Ok(())
}

fn infer_strategy(config: &AppConfig, goal: &str) -> TaskGraphStrategy {
    let lower = goal.to_lowercase();
    let needs_implementation = [
        "修改",
        "实现",
        "新增",
        "重构",
        "修复",
        "补齐",
        "完善",
        "生成",
        "创建",
        "建立",
        "初始化",
        "搭建",
        "编写",
        "落地",
        "执行",
        "update",
        "fix",
        "implement",
        "refactor",
        "generate",
        "create",
        "run",
        "scaffold",
        "skeleton",
        "bootstrap",
    ]
    .iter()
    .any(|keyword| lower.contains(keyword))
        || looks_like_scaffold_request(&lower);
    if needs_implementation && config.runtime.enable_long_running_delivery {
        TaskGraphStrategy::LongRunningDelivery
    } else {
        TaskGraphStrategy::Research
    }
}

fn looks_like_scaffold_request(goal: &str) -> bool {
    let scaffold_nouns = [
        "项目骨架",
        "目录骨架",
        "代码骨架",
        "脚手架",
        "scaffold",
        "skeleton",
        "bootstrap",
    ];
    let scaffold_verbs = ["完成", "生成", "创建", "建立", "搭", "搭建", "初始化"];
    scaffold_nouns.iter().any(|noun| goal.contains(noun))
        && scaffold_verbs.iter().any(|verb| goal.contains(verb))
}

fn default_success_criteria(goal: &str, strategy: TaskGraphStrategy) -> Vec<String> {
    let mut criteria = vec![format!("目标已被准确响应：{goal}")];
    match strategy {
        TaskGraphStrategy::Research => {
            criteria.push("关键事实已经查明，并给出可读结论".to_string());
        }
        TaskGraphStrategy::LongRunningDelivery => {
            criteria.push("execution contract 与 progress ledger 已落盘".to_string());
            criteria.push("至少一个 feature 完成 generator + evaluator 闭环".to_string());
            criteria.push("已生成可供下次 run 接手的 bootstrap".to_string());
        }
    }
    criteria
}

fn promote_ready_nodes(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    nodes: &mut [TaskNodeRecord],
) -> Result<()> {
    let snapshot = nodes.to_vec();
    for node in snapshot {
        if node.status != TaskNodeStatus::Pending {
            continue;
        }
        let ready = node.depends_on.iter().all(|dependency| {
            nodes
                .iter()
                .find(|candidate| &candidate.id == dependency)
                .is_some_and(|candidate| {
                    matches!(
                        candidate.status,
                        TaskNodeStatus::Completed | TaskNodeStatus::Skipped
                    )
                })
        });
        if !ready {
            continue;
        }
        let mut updated = node.clone();
        updated.status = TaskNodeStatus::Ready;
        store.update_task_node(run, &updated)?;
        store.append_run_event(
            &run.thread_id,
            &run.id,
            HarnessEvent::TaskNodeReady {
                thread_id: run.thread_id.clone(),
                run_id: run.id.clone(),
                task_node_id: updated.id.clone(),
                kind: updated.kind,
            },
        )?;
        if let Some(slot) = nodes
            .iter_mut()
            .find(|candidate| candidate.id == updated.id)
        {
            *slot = updated;
        }
    }
    Ok(())
}

fn next_executable_node(
    run: &HarnessRunManifest,
    nodes: &[TaskNodeRecord],
) -> Option<TaskNodeRecord> {
    if let Some(active) = run.active_task_node_id.as_deref()
        && let Some(node) = nodes
            .iter()
            .find(|node| node.id == active && !is_terminal_node(node.status))
    {
        return Some(node.clone());
    }
    nodes
        .iter()
        .find(|node| matches!(node.status, TaskNodeStatus::Ready | TaskNodeStatus::Running))
        .cloned()
}

fn is_terminal_node(status: TaskNodeStatus) -> bool {
    matches!(
        status,
        TaskNodeStatus::Completed | TaskNodeStatus::Failed | TaskNodeStatus::Skipped
    )
}

#[allow(clippy::too_many_arguments)]
async fn execute_task_node(
    repo_root: &Path,
    config: &AppConfig,
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &mut HarnessRunManifest,
    node: &TaskNodeRecord,
    memory_context: &str,
    skills_context: &str,
    session_context: &str,
) -> Result<()> {
    let mut active = node.clone();
    active.status = TaskNodeStatus::Running;
    active.started_at.get_or_insert_with(Utc::now);
    active.attempt_count += 1;
    store.update_task_node(run, &active)?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::TaskNodeStarted {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            task_node_id: active.id.clone(),
            kind: active.kind,
        },
    )?;

    match active.kind {
        TaskNodeKind::Plan => execute_plan_node(store, run, &active)?,
        TaskNodeKind::Initialize => execute_initialize_node(store, thread, run, &active)?,
        TaskNodeKind::BuildExecutionContract => {
            execute_build_contract_node(store, thread, run, &active)?
        }
        TaskNodeKind::PlanReview => execute_plan_review_node(config, store, thread, run, &active)?,
        TaskNodeKind::SelectNextFeature => execute_select_feature_node(store, run, &active)?,
        TaskNodeKind::ExecuteFeature => {
            execute_subagent(
                repo_root,
                config,
                store,
                run,
                &active,
                SubagentKind::Generator,
                memory_context,
                skills_context,
                session_context,
            )
            .await?;
            if store.load_task_node(run, &active.id)?.status == TaskNodeStatus::Completed {
                store.append_run_event(
                    &run.thread_id,
                    &run.id,
                    HarnessEvent::AgentHandoff {
                        thread_id: run.thread_id.clone(),
                        run_id: run.id.clone(),
                        from: "generator".to_string(),
                        to: "evaluator".to_string(),
                        reason: format!("feature `{}` 已有产出，进入评估", active.title),
                    },
                )?;
                append_feature_node(
                    store,
                    run,
                    TaskNodeKind::EvaluateFeature,
                    "评估当前 feature".to_string(),
                    format!(
                        "基于 done_when、工具结果和当前输出，评估 feature 是否完成：{}",
                        active.title
                    ),
                    vec![active.id.clone()],
                    active.feature_id.clone(),
                )?;
            }
        }
        TaskNodeKind::EvaluateFeature => {
            execute_evaluate_node(
                repo_root,
                config,
                store,
                run,
                &active,
                memory_context,
                skills_context,
                session_context,
            )
            .await?;
        }
        TaskNodeKind::CheckpointProgress => execute_checkpoint_node(store, thread, run, &active)?,
        TaskNodeKind::FinalizeDelivery => {
            execute_finalize_node(store, thread, run, &active)?;
        }
        TaskNodeKind::Explore => {
            execute_subagent(
                repo_root,
                config,
                store,
                run,
                &active,
                SubagentKind::Planner,
                memory_context,
                skills_context,
                session_context,
            )
            .await?;
        }
        TaskNodeKind::Summarize => execute_summarize_node(store, run, thread, &active)?,
        TaskNodeKind::Implement
        | TaskNodeKind::Review
        | TaskNodeKind::Test
        | TaskNodeKind::ApprovalGate => {
            let mut skipped = active.clone();
            skipped.status = TaskNodeStatus::Skipped;
            skipped.completed_at = Some(Utc::now());
            skipped.output_summary = Some("旧节点类型已由新长期运行状态机接管".to_string());
            store.update_task_node(run, &skipped)?;
            store.append_run_event(
                &run.thread_id,
                &run.id,
                HarnessEvent::TaskNodeCompleted {
                    thread_id: run.thread_id.clone(),
                    run_id: run.id.clone(),
                    task_node_id: skipped.id,
                    kind: skipped.kind,
                    status: skipped.status,
                },
            )?;
        }
    }
    Ok(())
}

fn execute_plan_node(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
) -> Result<()> {
    let graph = store.load_task_graph(run)?;
    let existing = store.list_task_nodes(run)?;
    if existing.len() == 1 {
        let explore = store.append_task_node(
            run,
            &graph,
            TaskNodeKind::Explore,
            "采集事实".to_string(),
            format!("围绕目标收集证据并形成结论：{}", graph.goal),
            vec![node.id.clone()],
            1,
            TaskNodeStatus::Pending,
        )?;
        store.append_task_node(
            run,
            &graph,
            TaskNodeKind::Summarize,
            "汇总结论".to_string(),
            "基于已完成节点产出最终回复".to_string(),
            vec![explore.id],
            2,
            TaskNodeStatus::Pending,
        )?;
    }

    complete_task_node(
        store,
        run,
        node,
        format!(
            "已建立研究任务图：strategy={:?}，success_criteria={}",
            graph.strategy,
            graph.success_criteria.join("；")
        ),
    )
}

fn execute_initialize_node(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
) -> Result<()> {
    let messages = store.list_messages(&thread.id)?;
    let summary = format!(
        "已初始化长期任务上下文，thread={}，messages={}，bootstrap={}",
        thread.id,
        messages.len(),
        if thread.bootstrap_path.exists() {
            "present"
        } else {
            "missing"
        }
    );
    update_progress_state(
        store,
        &thread.id,
        "计划",
        Some("构建 execution contract".to_string()),
        None,
        None,
    )?;
    complete_task_node(store, run, node, summary)?;
    append_feature_node(
        store,
        run,
        TaskNodeKind::BuildExecutionContract,
        "构建执行契约".to_string(),
        "根据最新用户目标生成 execution contract 与初始 progress".to_string(),
        vec![node.id.clone()],
        None,
    )?;
    Ok(())
}

fn execute_build_contract_node(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
) -> Result<()> {
    let messages = store.list_messages(&thread.id)?;
    let goal = messages
        .iter()
        .rev()
        .find(|message| message.role == HarnessMessageRole::User)
        .map(|message| message.content.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "处理当前任务".to_string());
    let contract = build_execution_contract(&goal);
    let progress = ProgressLedger {
        goal: goal.clone(),
        current_phase: Some("计划".to_string()),
        completed_features: Vec::new(),
        current_feature: None,
        latest_recoverable_failure: None,
        blocking_reason: None,
        known_failures: Vec::new(),
        decisions: vec![
            "已建立 execution contract".to_string(),
            "系统将先完成显式计划检查，再进入执行阶段".to_string(),
        ],
        open_questions: Vec::new(),
        next_step: Some("审查计划并确认验收标准".to_string()),
        updated_at: Utc::now(),
    };
    store.save_execution_contract(&thread.id, &contract)?;
    store.save_progress_ledger(&thread.id, &progress)?;
    write_plan_snapshot_artifact(store, thread, run, node, &contract, &progress)?;
    store.append_artifact(
        &thread.id,
        &run.id,
        Some(node.id.clone()),
        None,
        "execution-contract".to_string(),
        crate::harness::ArtifactKind::ContractSnapshot,
        thread.contract_path.clone(),
    )?;
    store.append_artifact(
        &thread.id,
        &run.id,
        Some(node.id.clone()),
        None,
        "progress-ledger".to_string(),
        crate::harness::ArtifactKind::ProgressSnapshot,
        thread.progress_path.clone(),
    )?;
    complete_task_node(
        store,
        run,
        node,
        format!(
            "已生成 contract 与初始计划，features={}",
            contract.ordered_features.len()
        ),
    )?;
    append_feature_node(
        store,
        run,
        TaskNodeKind::PlanReview,
        "审查执行计划".to_string(),
        "检查计划摘要、范围、约束、最小验证方案和下一步 feature 是否完整。".to_string(),
        vec![node.id.clone()],
        None,
    )?;
    Ok(())
}

fn execute_plan_review_node(
    config: &AppConfig,
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
) -> Result<()> {
    let contract = store.load_execution_contract(&thread.id)?;
    let mut progress = store.load_progress_ledger(&thread.id)?;
    let missing = validate_execution_plan(&contract, &progress);
    if !missing.is_empty() {
        let detail = format!("计划仍缺少：{}", missing.join("、"));
        progress.open_questions.extend(missing.clone());
        progress.blocking_reason = Some(detail.clone());
        progress.next_step = Some("补齐计划缺失项后再进入执行".to_string());
        progress.updated_at = Utc::now();
        store.save_progress_ledger(&thread.id, &progress)?;
        store.append_run_event(
            &run.thread_id,
            &run.id,
            HarnessEvent::EvidenceInsufficient {
                thread_id: run.thread_id.clone(),
                run_id: run.id.clone(),
                task_node_id: node.id.clone(),
                detail: detail.clone(),
            },
        )?;
        fail_task_node(store, run, node, detail)?;
        return Ok(());
    }

    if config.runtime.interactive_plan_confirmation && !plan_confirmation_acknowledged(&progress) {
        block_on_plan_confirmation(store, thread, run, node, &contract, &mut progress)?;
        return Ok(());
    }

    progress.current_phase = Some("计划".to_string());
    progress.blocking_reason = None;
    progress.next_step = Some("选择下一个 feature".to_string());
    progress.updated_at = Utc::now();
    if !progress
        .decisions
        .iter()
        .any(|item| item == "计划检查通过，可以开始执行 feature")
    {
        progress
            .decisions
            .push("计划检查通过，可以开始执行 feature".to_string());
    }
    store.save_progress_ledger(&thread.id, &progress)?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::AgentHandoff {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            from: "planner".to_string(),
            to: "generator".to_string(),
            reason: "计划检查通过，开始进入执行阶段".to_string(),
        },
    )?;
    complete_task_node(
        store,
        run,
        node,
        render_plan_review_summary(&contract, &progress),
    )?;
    append_feature_node(
        store,
        run,
        TaskNodeKind::SelectNextFeature,
        "选择下一个 feature".to_string(),
        "读取 contract/progress，选择当前要推进的最小 feature".to_string(),
        vec![node.id.clone()],
        None,
    )?;
    Ok(())
}

fn plan_confirmation_acknowledged(progress: &ProgressLedger) -> bool {
    progress
        .decisions
        .iter()
        .any(|item| item == PLAN_CONFIRMATION_MARKER)
}

fn block_on_plan_confirmation(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
    contract: &ExecutionContract,
    progress: &mut ProgressLedger,
) -> Result<()> {
    let summary = render_plan_review_summary(contract, progress);
    let mut waiting = node.clone();
    waiting.status = TaskNodeStatus::WaitingForInput;
    waiting.output_summary = Some(format!(
        "{}\n\n等待操作：按 Enter 确认计划继续执行，或在 Composer 输入反馈后重新生成计划。",
        summary
    ));
    waiting.error = None;
    store.update_task_node(run, &waiting)?;

    progress.current_phase = Some("计划".to_string());
    progress.blocking_reason = Some("等待用户确认计划或补充反馈".to_string());
    progress.next_step =
        Some("按 Enter 确认计划，或在 Composer 提交反馈后重新生成计划".to_string());
    progress.updated_at = Utc::now();
    if !progress
        .decisions
        .iter()
        .any(|item| item == "计划已生成，等待用户确认")
    {
        progress
            .decisions
            .push("计划已生成，等待用户确认".to_string());
    }
    store.save_progress_ledger(&thread.id, progress)?;

    let mut updated_run = run.clone();
    updated_run.status = HarnessRunStatus::WaitingForInput;
    updated_run.summary = Some("计划已生成，等待用户确认".to_string());
    updated_run.blocked_reason = Some("计划已生成，等待用户确认或补充反馈".to_string());
    updated_run.active_task_node_id = Some(node.id.clone());
    store.update_run(&run.thread_id, &updated_run)
}

pub(super) fn confirm_plan_review_and_prepare_resume(
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
    task_node_id: &str,
) -> Result<()> {
    let thread = store.load_thread(&run.thread_id)?;
    let mut node = store.load_task_node(run, task_node_id)?;
    if node.kind != TaskNodeKind::PlanReview {
        return Err(anyhow!("当前节点不是计划确认节点"));
    }
    if node.status != TaskNodeStatus::WaitingForInput {
        return Err(anyhow!("当前计划节点不在等待确认状态"));
    }

    let mut progress = store.load_progress_ledger(&thread.id)?;
    if !plan_confirmation_acknowledged(&progress) {
        progress
            .decisions
            .push(PLAN_CONFIRMATION_MARKER.to_string());
    }
    progress.current_phase = Some("计划".to_string());
    progress.blocking_reason = None;
    progress.next_step = Some("选择下一个 feature".to_string());
    progress.updated_at = Utc::now();
    store.save_progress_ledger(&thread.id, &progress)?;

    node.status = TaskNodeStatus::Ready;
    node.error = None;
    node.output_summary = Some("用户已确认计划，继续执行".to_string());
    store.update_task_node(run, &node)?;

    run.status = HarnessRunStatus::Running;
    run.summary = Some("用户已确认计划，继续执行".to_string());
    run.blocked_reason = None;
    run.last_error = None;
    run.active_task_node_id = Some(node.id.clone());
    store.update_run(&run.thread_id, run)
}

fn execute_select_feature_node(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
) -> Result<()> {
    let thread = store.load_thread(&run.thread_id)?;
    let mut contract = store.load_execution_contract(&thread.id)?;
    let mut progress = store.load_progress_ledger(&thread.id)?;
    let next_feature = contract
        .ordered_features
        .iter_mut()
        .find(|feature| feature.status != FeatureSliceStatus::Completed);

    if let Some(feature) = next_feature {
        let feature_id = feature.id.clone();
        let feature_title = feature.title.clone();
        let feature_instructions = render_feature_execution_instructions(feature);
        feature.status = FeatureSliceStatus::InProgress;
        progress.current_phase = Some("执行".to_string());
        progress.current_feature = Some(feature_id.clone());
        progress.blocking_reason = None;
        progress.next_step = Some(format!("执行 feature `{}`", feature_title));
        progress.updated_at = Utc::now();
        store.save_execution_contract(&thread.id, &contract)?;
        store.save_progress_ledger(&thread.id, &progress)?;
        complete_task_node(
            store,
            run,
            node,
            format!("已选择 feature：{} ({})", feature_title, feature_id),
        )?;
        append_feature_node(
            store,
            run,
            TaskNodeKind::ExecuteFeature,
            format!("执行 feature：{}", feature_title),
            feature_instructions,
            vec![node.id.clone()],
            Some(feature_id),
        )?;
        return Ok(());
    }

    progress.current_phase = Some("交付".to_string());
    progress.current_feature = None;
    progress.next_step = Some("生成最终交付".to_string());
    progress.updated_at = Utc::now();
    store.save_progress_ledger(&thread.id, &progress)?;
    complete_task_node(
        store,
        run,
        node,
        "所有 feature 已完成，进入最终交付".to_string(),
    )?;
    append_feature_node(
        store,
        run,
        TaskNodeKind::FinalizeDelivery,
        "生成最终交付".to_string(),
        "汇总 contract/progress/evaluation，形成最终回复".to_string(),
        vec![node.id.clone()],
        None,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn execute_evaluate_node(
    repo_root: &Path,
    config: &AppConfig,
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
    node: &TaskNodeRecord,
    memory_context: &str,
    skills_context: &str,
    session_context: &str,
) -> Result<()> {
    let decision = evaluate_feature(
        repo_root,
        config,
        store,
        run,
        node,
        memory_context,
        skills_context,
        session_context,
    )
    .await?;
    store.append_evaluation(run, &decision)?;
    let thread = store.load_thread(&run.thread_id)?;
    store.append_artifact(
        &thread.id,
        &run.id,
        Some(node.id.clone()),
        None,
        format!(
            "evaluation:{}",
            decision.feature_id.as_deref().unwrap_or("unknown")
        ),
        crate::harness::ArtifactKind::EvaluationSnapshot,
        run.evaluation_log_path.clone(),
    )?;

    if decision.passed {
        let mut contract = store.load_execution_contract(&thread.id)?;
        let mut progress = store.load_progress_ledger(&thread.id)?;
        if let Some(feature_id) = decision.feature_id.clone()
            && let Some(feature) = contract
                .ordered_features
                .iter_mut()
                .find(|feature| feature.id == feature_id)
        {
            feature.status = FeatureSliceStatus::Completed;
            if !progress.completed_features.contains(&feature.id) {
                progress.completed_features.push(feature.id.clone());
            }
        }
        progress
            .decisions
            .push(format!("evaluator 通过：{}", decision.reason));
        progress.current_phase = Some("评估".to_string());
        progress.latest_recoverable_failure = None;
        progress.blocking_reason = None;
        progress.next_step = Some("写入 checkpoint 并准备下一个 feature".to_string());
        progress.updated_at = Utc::now();
        store.save_execution_contract(&thread.id, &contract)?;
        store.save_progress_ledger(&thread.id, &progress)?;
        complete_task_node(
            store,
            run,
            node,
            format!("evaluator 通过：{}", decision.reason),
        )?;
        append_feature_node(
            store,
            run,
            TaskNodeKind::CheckpointProgress,
            "写入进度检查点".to_string(),
            "更新 progress、生成 bootstrap，并决定是否继续下一个 feature".to_string(),
            vec![node.id.clone()],
            decision.feature_id.clone(),
        )?;
        return Ok(());
    }

    let execute_attempts = find_execute_attempts(store, run, node.feature_id.as_deref())?;
    if decision.retryable && execute_attempts < config.runtime.max_feature_retries {
        let mut progress = store.load_progress_ledger(&thread.id)?;
        progress.current_phase = Some("评估".to_string());
        progress
            .known_failures
            .push(format!("feature 重试前失败：{}", decision.reason));
        progress.blocking_reason = Some(decision.reason.clone());
        progress.next_step = Some("根据 evaluator 结论重新执行当前 feature".to_string());
        progress.updated_at = Utc::now();
        store.save_progress_ledger(&thread.id, &progress)?;
        store.append_run_event(
            &run.thread_id,
            &run.id,
            HarnessEvent::AgentHandoff {
                thread_id: run.thread_id.clone(),
                run_id: run.id.clone(),
                from: "evaluator".to_string(),
                to: "generator".to_string(),
                reason: format!("评估未通过，准备重试：{}", decision.reason),
            },
        )?;
        complete_task_node(
            store,
            run,
            node,
            format!("evaluator 未通过，准备重试：{}", decision.reason),
        )?;
        append_feature_node(
            store,
            run,
            TaskNodeKind::ExecuteFeature,
            format!(
                "重试 feature：{}",
                node.feature_id.as_deref().unwrap_or("unknown")
            ),
            format!("根据 evaluator 结论重试当前 feature：{}", decision.reason),
            vec![node.id.clone()],
            node.feature_id.clone(),
        )?;
        return Ok(());
    }

    if decision.retryable {
        fail_task_node(
            store,
            run,
            node,
            format!("feature 重试次数已达上限：{}", decision.reason),
        )?;
        return Ok(());
    }

    block_on_manual_input(store, run, node, &decision)?;
    Ok(())
}

fn execute_checkpoint_node(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
) -> Result<()> {
    let contract = store.load_execution_contract(&thread.id)?;
    let mut progress = store.load_progress_ledger(&thread.id)?;
    let latest_evaluation = store.list_evaluations(run)?.into_iter().next();
    progress.current_phase = Some("交付".to_string());
    progress.current_feature = None;
    progress.blocking_reason = None;
    progress.next_step = contract
        .ordered_features
        .iter()
        .find(|feature| feature.status != FeatureSliceStatus::Completed)
        .map(|feature| format!("继续处理 feature `{}`", feature.title))
        .or(Some("所有 feature 已完成，准备最终交付".to_string()));
    progress.updated_at = Utc::now();
    store.save_progress_ledger(&thread.id, &progress)?;

    let bootstrap = render_bootstrap(&contract, &progress, latest_evaluation.as_ref());
    store.write_session_bootstrap(&thread.id, run, &bootstrap)?;
    store.append_artifact(
        &thread.id,
        &run.id,
        Some(node.id.clone()),
        None,
        "session-bootstrap".to_string(),
        crate::harness::ArtifactKind::SessionBootstrap,
        run.bootstrap_path.clone(),
    )?;
    complete_task_node(
        store,
        run,
        node,
        "已更新 progress 并生成 session bootstrap".to_string(),
    )?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::AgentHandoff {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            from: "evaluator".to_string(),
            to: "planner".to_string(),
            reason: "评估通过，写入 checkpoint 并决定下一步".to_string(),
        },
    )?;
    append_feature_node(
        store,
        run,
        TaskNodeKind::SelectNextFeature,
        "选择下一个 feature".to_string(),
        "根据最新 progress 决定继续推进哪个 feature".to_string(),
        vec![node.id.clone()],
        None,
    )?;
    Ok(())
}

fn execute_finalize_node(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &mut HarnessRunManifest,
    node: &TaskNodeRecord,
) -> Result<()> {
    let contract = store.load_execution_contract(&thread.id)?;
    let mut progress = store.load_progress_ledger(&thread.id)?;
    let latest_evaluation = store.list_evaluations(run)?.into_iter().next();
    progress.current_phase = Some("交付".to_string());
    progress.blocking_reason = None;
    progress.next_step = Some("向用户输出最终交付摘要".to_string());
    progress.updated_at = Utc::now();
    store.save_progress_ledger(&thread.id, &progress)?;
    let final_response = format!(
        "长期任务已完成。\n目标：{}\n\n当前阶段：{}\n已完成 feature：{}\n下一步：{}\n最近评估：{}",
        contract.goal,
        progress.current_phase.as_deref().unwrap_or("交付"),
        if progress.completed_features.is_empty() {
            "无".to_string()
        } else {
            progress.completed_features.join("、")
        },
        progress.next_step.unwrap_or_else(|| "无".to_string()),
        latest_evaluation
            .map(|item| item.reason)
            .unwrap_or_else(|| "无".to_string())
    );
    store.append_message(
        &thread.id,
        HarnessMessageRole::Assistant,
        final_response.clone(),
        Some(run.id.clone()),
    )?;
    complete_task_node(
        store,
        run,
        node,
        first_non_empty_line(&final_response).to_string(),
    )?;
    finish_run(
        store,
        run,
        Some(first_non_empty_line(&final_response).to_string()),
        None,
    )
}

fn execute_summarize_node(
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
    thread: &HarnessThreadManifest,
    node: &TaskNodeRecord,
) -> Result<()> {
    let graph = store.load_task_graph(run)?;
    let nodes = store.list_task_nodes(run)?;
    let relevant = nodes
        .iter()
        .filter(|candidate| candidate.id != node.id)
        .filter_map(|candidate| {
            candidate
                .output_summary
                .as_ref()
                .map(|summary| format!("{}：{}", candidate.title, summary))
        })
        .collect::<Vec<_>>();
    let final_response = {
        let facts = relevant.join("\n");
        if facts.trim().is_empty() {
            format!("已完成任务：{}", graph.goal)
        } else {
            format!("{}\n\n{}", graph.goal, facts)
        }
    };

    store.append_message(
        &thread.id,
        HarnessMessageRole::Assistant,
        final_response.clone(),
        Some(run.id.clone()),
    )?;
    let mut completed = node.clone();
    completed.status = TaskNodeStatus::Completed;
    completed.completed_at = Some(Utc::now());
    completed.output_summary = Some(first_non_empty_line(&final_response).to_string());
    store.update_task_node(run, &completed)?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::TaskNodeCompleted {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            task_node_id: completed.id,
            kind: completed.kind,
            status: completed.status,
        },
    )?;
    finish_run(
        store,
        run,
        Some(first_non_empty_line(&final_response).to_string()),
        None,
    )
}

fn build_execution_contract(goal: &str) -> ExecutionContract {
    let features = split_goal_into_features(goal)
        .into_iter()
        .enumerate()
        .map(|(index, title)| FeatureSlice {
            id: format!("feature-{}", index + 1),
            title: title.clone(),
            intent: title.clone(),
            scope_paths: Vec::new(),
            done_when: infer_feature_acceptance(&title, index + 1),
            status: FeatureSliceStatus::Pending,
        })
        .collect::<Vec<_>>();
    ExecutionContract {
        goal: goal.to_string(),
        non_goals: vec!["不做与当前目标无关的大重构".to_string()],
        constraints: vec![
            "保持现有 CLI/TUI 与 thread/run 数据模型兼容".to_string(),
            "优先最小充分改动".to_string(),
        ],
        ordered_features: if features.is_empty() {
            vec![FeatureSlice {
                id: "feature-1".to_string(),
                title: goal.to_string(),
                intent: goal.to_string(),
                scope_paths: Vec::new(),
                done_when: vec![AcceptanceCriterion {
                    id: "acc-1".to_string(),
                    description: "目标已经闭环完成".to_string(),
                }],
                status: FeatureSliceStatus::Pending,
            }]
        } else {
            features
        },
        global_acceptance: vec![AcceptanceCriterion {
            id: "global-1".to_string(),
            description: "生成可继续接手的 bootstrap".to_string(),
        }],
        delivery_notes: vec!["每次只推进一个 feature".to_string()],
        updated_at: Utc::now(),
    }
}

fn infer_feature_acceptance(title: &str, index: usize) -> Vec<AcceptanceCriterion> {
    let lower = title.to_lowercase();
    let is_explicitly_readonly = [
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
    .any(|marker| lower.contains(marker));
    let is_inspection = [
        "执行",
        "查看",
        "读取",
        "搜索",
        "列出",
        "总结",
        "说明",
        "解释",
        "分析",
        "run",
        "inspect",
        "read",
        "list",
        "search",
        "summarize",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        && (is_explicitly_readonly
            || ![
                "修改",
                "实现",
                "修复",
                "重构",
                "生成",
                "创建",
                "写入",
                "落地",
                "fix",
                "implement",
                "refactor",
                "generate",
                "create",
                "write",
            ]
            .iter()
            .any(|marker| lower.contains(marker)));

    if is_inspection {
        return vec![
            AcceptanceCriterion {
                id: format!("acc-{}-1", index),
                description: "相关命令或查询已经执行，并记录了结果".to_string(),
            },
            AcceptanceCriterion {
                id: format!("acc-{}-2", index),
                description: "evaluator 给出通过结论".to_string(),
            },
        ];
    }

    vec![
        AcceptanceCriterion {
            id: format!("acc-{}-1", index),
            description: "相关代码或产物已经落地".to_string(),
        },
        AcceptanceCriterion {
            id: format!("acc-{}-2", index),
            description: "evaluator 给出通过结论".to_string(),
        },
    ]
}

fn split_goal_into_features(goal: &str) -> Vec<String> {
    let features = goal
        .split(['\n', '；', ';'])
        .flat_map(|part| part.split("然后"))
        .flat_map(|part| part.split("并"))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let lower = goal.to_lowercase();
    if looks_like_large_project_delivery(&lower) && features.len() <= 2 {
        return vec![
            "需求与现状确认".to_string(),
            "项目骨架与执行入口搭建".to_string(),
            "核心功能实现与集成".to_string(),
            "验证、收尾与交付说明".to_string(),
        ];
    }

    features
}

fn looks_like_large_project_delivery(goal: &str) -> bool {
    let scale_markers = [
        "完整项目",
        "完整的项目",
        "整个项目",
        "从 0 到 1",
        "从0到1",
        "一步步",
        "按计划",
        "全流程",
        "端到端",
        "from scratch",
        "complete project",
        "end-to-end",
        "bootstrap",
        "scaffold",
        "skeleton",
    ];
    let implementation_markers = [
        "完成",
        "实现",
        "创建",
        "搭建",
        "搭一个",
        "生成",
        "落地",
        "build",
        "implement",
        "create",
        "generate",
    ];
    scale_markers.iter().any(|marker| goal.contains(marker))
        && implementation_markers
            .iter()
            .any(|marker| goal.contains(marker))
}

fn render_feature_execution_instructions(feature: &FeatureSlice) -> String {
    let done_when = feature
        .done_when
        .iter()
        .map(|item| format!("- {}", item.description))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "当前只处理这个 feature。\nfeature_id: {}\ntitle: {}\nintent: {}\ndone_when:\n{}",
        feature.id, feature.title, feature.intent, done_when
    )
}

fn append_feature_node(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    kind: TaskNodeKind,
    title: String,
    instructions: String,
    depends_on: Vec<String>,
    feature_id: Option<String>,
) -> Result<TaskNodeRecord> {
    let graph = store.load_task_graph(run)?;
    let position = next_task_position(store, run)?;
    let mut node = store.append_task_node(
        run,
        &graph,
        kind,
        title,
        instructions,
        depends_on,
        position,
        TaskNodeStatus::Pending,
    )?;
    node.feature_id = feature_id;
    store.update_task_node(run, &node)?;
    Ok(node)
}

fn validate_execution_plan(contract: &ExecutionContract, progress: &ProgressLedger) -> Vec<String> {
    let mut missing = Vec::new();
    if contract.goal.trim().is_empty() {
        missing.push("目标".to_string());
    }
    if contract.constraints.is_empty() {
        missing.push("关键约束".to_string());
    }
    if contract.ordered_features.is_empty() {
        missing.push("下一步 feature".to_string());
    }
    let has_validation = contract
        .ordered_features
        .iter()
        .any(|feature| !feature.done_when.is_empty());
    if !has_validation {
        missing.push("最小验证方案".to_string());
    }
    if contract.non_goals.is_empty() {
        missing.push("范围".to_string());
    }
    if progress
        .next_step
        .as_deref()
        .unwrap_or("")
        .trim()
        .is_empty()
    {
        missing.push("下一步动作".to_string());
    }
    missing
}

fn render_plan_review_summary(contract: &ExecutionContract, progress: &ProgressLedger) -> String {
    let first_feature = contract
        .ordered_features
        .first()
        .map(|feature| feature.title.as_str())
        .unwrap_or("-");
    let feature_titles = contract
        .ordered_features
        .iter()
        .map(|feature| feature.title.as_str())
        .collect::<Vec<_>>()
        .join(" -> ");
    format!(
        "计划检查通过：目标=`{}`，范围={}，约束={}，feature 总数={}，执行序列=`{}`，第一步 feature=`{}`，最小验证项={}",
        contract.goal,
        contract.non_goals.join("；"),
        contract.constraints.join("；"),
        contract.ordered_features.len(),
        feature_titles,
        first_feature,
        contract
            .ordered_features
            .first()
            .map(|feature| {
                feature
                    .done_when
                    .iter()
                    .map(|item| item.description.as_str())
                    .collect::<Vec<_>>()
                    .join(" / ")
            })
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| progress
                .next_step
                .clone()
                .unwrap_or_else(|| "-".to_string()))
    )
}

fn write_plan_snapshot_artifact(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
    contract: &ExecutionContract,
    progress: &ProgressLedger,
) -> Result<()> {
    let artifact_dir = run.run_dir.join("artifact-files");
    fs::create_dir_all(&artifact_dir)?;
    let path = artifact_dir.join(format!("plan-snapshot-{}.md", node.id));
    let body = format!(
        "# 执行计划\n\n目标：{}\n\n范围限制：\n{}\n\n关键约束：\n{}\n\n执行阶段：\n{}\n\n第一步 feature：{}\n\n最小验证：\n{}\n\n下一步：{}\n",
        contract.goal,
        contract
            .non_goals
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n"),
        contract
            .constraints
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n"),
        contract
            .ordered_features
            .iter()
            .map(|feature| format!("- {}: {}", feature.id, feature.title))
            .collect::<Vec<_>>()
            .join("\n"),
        contract
            .ordered_features
            .first()
            .map(|feature| feature.title.as_str())
            .unwrap_or("-"),
        contract
            .ordered_features
            .first()
            .map(|feature| {
                feature
                    .done_when
                    .iter()
                    .map(|item| format!("- {}", item.description))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "- 无".to_string()),
        progress.next_step.as_deref().unwrap_or("-")
    );
    fs::write(&path, body)?;
    store.append_artifact(
        &thread.id,
        &run.id,
        Some(node.id.clone()),
        None,
        "plan-snapshot".to_string(),
        crate::harness::ArtifactKind::PlanSnapshot,
        path,
    )?;
    Ok(())
}

fn update_progress_state(
    store: &HarnessStore,
    thread_id: &str,
    phase: &str,
    next_step: Option<String>,
    blocking_reason: Option<String>,
    latest_recoverable_failure: Option<String>,
) -> Result<()> {
    let mut progress = match store.load_progress_ledger(thread_id) {
        Ok(progress) => progress,
        Err(_) => return Ok(()),
    };
    progress.current_phase = Some(phase.to_string());
    if let Some(next_step) = next_step {
        progress.next_step = Some(next_step);
    }
    progress.blocking_reason = blocking_reason;
    progress.latest_recoverable_failure = latest_recoverable_failure;
    progress.updated_at = Utc::now();
    store.save_progress_ledger(thread_id, &progress)
}

fn next_task_position(store: &HarnessStore, run: &HarnessRunManifest) -> Result<usize> {
    Ok(store
        .list_task_nodes(run)?
        .into_iter()
        .map(|node| node.position)
        .max()
        .unwrap_or(0)
        + 1)
}

fn render_bootstrap(
    contract: &ExecutionContract,
    progress: &ProgressLedger,
    latest_evaluation: Option<&EvaluationDecision>,
) -> String {
    format!(
        "# Session Bootstrap\n\ngoal: {}\nphase: {}\ncompleted_features: {}\ncurrent_feature: {}\nnext_step: {}\nlatest_evaluation: {}\nlatest_recoverable_failure: {}\nblocking_reason: {}\n\ncontract_features:\n{}\n",
        contract.goal,
        progress.current_phase.as_deref().unwrap_or("-"),
        if progress.completed_features.is_empty() {
            "-".to_string()
        } else {
            progress.completed_features.join(", ")
        },
        progress.current_feature.as_deref().unwrap_or("-"),
        progress.next_step.as_deref().unwrap_or("-"),
        latest_evaluation
            .map(|item| item.reason.as_str())
            .unwrap_or("-"),
        progress
            .latest_recoverable_failure
            .as_deref()
            .unwrap_or("-"),
        progress.blocking_reason.as_deref().unwrap_or("-"),
        contract
            .ordered_features
            .iter()
            .map(|feature| format!("- {} [{:?}]", feature.title, feature.status))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn find_execute_attempts(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    feature_id: Option<&str>,
) -> Result<usize> {
    Ok(store
        .list_task_nodes(run)?
        .into_iter()
        .filter(|node| node.kind == TaskNodeKind::ExecuteFeature)
        .filter(|node| feature_id.is_none() || node.feature_id.as_deref() == feature_id)
        .map(|node| node.attempt_count)
        .max()
        .unwrap_or(0))
}

fn block_on_manual_input(
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
    node: &TaskNodeRecord,
    decision: &EvaluationDecision,
) -> Result<()> {
    let mut waiting = node.clone();
    waiting.status = TaskNodeStatus::WaitingForInput;
    waiting.output_summary = Some(format!("等待人工判断：{}", decision.reason));
    store.update_task_node(run, &waiting)?;
    if let Ok(thread) = store.load_thread(&run.thread_id)
        && let Ok(mut progress) = store.load_progress_ledger(&thread.id)
    {
        progress.current_phase = Some("评估".to_string());
        progress.blocking_reason = Some(decision.reason.clone());
        progress.next_step = Some("等待人工输入后继续".to_string());
        progress.updated_at = Utc::now();
        let _ = store.save_progress_ledger(&thread.id, &progress);
    }
    run.status = HarnessRunStatus::WaitingForInput;
    run.summary = Some(format!("等待人工判断：{}", decision.reason));
    run.blocked_reason = Some(format!("feature 需要人工处理：{}", decision.reason));
    run.active_task_node_id = Some(node.id.clone());
    store.update_run(&run.thread_id, run)
}

pub(super) fn execute_and_record_tool(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &mut HarnessRunManifest,
    call: &ToolCallRequest,
    record: &ToolCallRecord,
) -> Result<ToolExecutionOutcome> {
    let call = normalize_tool_call(call);
    let sandbox = run
        .sandbox
        .clone()
        .ok_or_else(|| anyhow!("run 缺少 sandbox"))?;
    let result = execute_tool_call(
        store,
        thread,
        run,
        &sandbox,
        &call,
        record.task_node_id.as_deref(),
        record.subagent_id.as_deref(),
    );
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            let detail = format!("工具 `{}` 执行失败：{}", call.name, error);
            mark_tool_resolution(
                store,
                run,
                &record.id,
                ToolCallStatus::Failed,
                Some(first_non_empty_line(&detail).to_string()),
                Some(format!("{error:#}")),
            )?;
            let _ = update_progress_state(
                store,
                &thread.id,
                "执行",
                Some("根据失败原因重试、换工具或请求人工输入".to_string()),
                None,
                Some(detail.clone()),
            );
            store.append_message(
                &run.thread_id,
                HarnessMessageRole::Tool,
                detail.clone(),
                Some(run.id.clone()),
            )?;
            store.append_run_event(
                &run.thread_id,
                &run.id,
                HarnessEvent::ToolCallCompleted {
                    thread_id: run.thread_id.clone(),
                    run_id: run.id.clone(),
                    tool_call_id: record.id.clone(),
                    status: ToolCallStatus::Failed,
                },
            )?;
            store.append_run_event(
                &run.thread_id,
                &run.id,
                HarnessEvent::RecoverableFailureDetected {
                    thread_id: run.thread_id.clone(),
                    run_id: run.id.clone(),
                    source: format!("tool:{}", call.name),
                    detail: detail.clone(),
                },
            )?;
            return Ok(ToolExecutionOutcome::RecoverableFailure);
        }
    };
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
    Ok(ToolExecutionOutcome::Succeeded)
}

pub(super) fn record_tool_planned(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    call: &ToolCallRequest,
    task_node_id: Option<String>,
    subagent_id: Option<String>,
) -> Result<ToolCallRecord> {
    let call = normalize_tool_call(call);
    let record = store.append_tool_call(run, &call, task_node_id, subagent_id)?;
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
    Ok(record)
}

pub(super) fn request_tool_approval(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &mut HarnessRunManifest,
    node: &TaskNodeRecord,
    call: &ToolCallRequest,
    record: &ToolCallRecord,
) -> Result<()> {
    let call = normalize_tool_call(call);
    let approval = store.append_approval(
        thread,
        run,
        record,
        approval_reason(&call.name).to_string(),
        call.clone(),
    )?;
    let mut updated_record = record.clone();
    updated_record.approval_id = Some(approval.id.clone());
    updated_record.status = ToolCallStatus::PendingApproval;
    store.update_tool_call(run, &updated_record)?;

    let mut waiting = node.clone();
    waiting.status = TaskNodeStatus::WaitingForInput;
    waiting.output_summary = Some(format!("等待审批：{}", approval.tool_name));
    store.update_task_node(run, &waiting)?;

    run.status = HarnessRunStatus::WaitingForInput;
    run.active_task_node_id = Some(node.id.clone());
    run.summary = Some(format!("等待审批：{}", approval.tool_name));
    run.blocked_reason = Some(format!("节点 `{}` 等待工具审批", node.title));
    store.update_run(&run.thread_id, run)?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::ApprovalRequested {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            approval_id: approval.id,
            tool_name: updated_record.name,
        },
    )?;
    Ok(())
}

pub(super) fn complete_task_node(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
    summary: String,
) -> Result<()> {
    let mut completed = node.clone();
    completed.status = TaskNodeStatus::Completed;
    completed.completed_at = Some(Utc::now());
    completed.output_summary = Some(summary.clone());
    completed.error = None;
    store.update_task_node(run, &completed)?;
    store.append_message(
        &run.thread_id,
        HarnessMessageRole::Summary,
        summary,
        Some(run.id.clone()),
    )?;
    let memory_layer = match node.kind {
        TaskNodeKind::FinalizeDelivery | TaskNodeKind::Summarize => MemoryLayer::Project,
        _ => MemoryLayer::Working,
    };
    store.append_memory_entry(
        &run.thread_id,
        memory_layer,
        completed.output_summary.clone().unwrap_or_default(),
        format!("task:{:?}", node.kind),
        Some(run.id.clone()),
        Some(node.id.clone()),
    )?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::TaskNodeCompleted {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            task_node_id: completed.id,
            kind: completed.kind,
            status: completed.status,
        },
    )?;
    Ok(())
}

pub(super) fn fail_task_node(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    node: &TaskNodeRecord,
    error: String,
) -> Result<()> {
    let mut failed = node.clone();
    failed.status = TaskNodeStatus::Failed;
    failed.completed_at = Some(Utc::now());
    failed.error = Some(error.clone());
    store.update_task_node(run, &failed)?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::TaskNodeFailed {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            task_node_id: failed.id,
            kind: failed.kind,
            error,
        },
    )?;
    Ok(())
}

pub(super) fn finish_run(
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
    final_summary: Option<String>,
    error: Option<String>,
) -> Result<()> {
    run.active_task_node_id = None;
    run.blocked_reason = None;
    if let Some(error) = error {
        run.status = HarnessRunStatus::Failed;
        run.summary = final_summary
            .or_else(|| run.summary.clone())
            .or(Some("run 已失败".to_string()));
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
        run.last_error = None;
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
        DockerSandboxProvider::from(sandbox).destroy(sandbox)?;
    }
    run.sandbox = None;
    store.update_run(&run.thread_id, run)?;
    Ok(())
}

pub(super) fn cancel_run(store: &HarnessStore, run: &mut HarnessRunManifest) -> Result<()> {
    run.status = HarnessRunStatus::Cancelled;
    run.active_task_node_id = None;
    run.summary = Some("当前 run 已取消".to_string());
    run.last_error = None;
    run.blocked_reason = None;
    if let Some(sandbox) = &run.sandbox {
        DockerSandboxProvider::from(sandbox).destroy(sandbox)?;
    }
    run.sandbox = None;
    store.update_run(&run.thread_id, run)?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::RunCancelled {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
        },
    )?;
    Ok(())
}

pub(super) fn reset_task_node_for_retry(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    task_node_id: &str,
) -> Result<TaskNodeRecord> {
    let mut node = store.load_task_node(run, task_node_id)?;
    node.status = TaskNodeStatus::Ready;
    node.error = None;
    node.output_summary = None;
    node.completed_at = None;
    store.update_task_node(run, &node)?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::TaskNodeRetried {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            task_node_id: node.id.clone(),
        },
    )?;
    Ok(node)
}

pub(super) fn first_non_empty_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("")
}

pub(super) fn tool_needs_approval(config: &AppConfig, call: &ToolCallRequest) -> bool {
    if !config.runtime.require_tool_approval {
        return false;
    }

    let call = normalize_tool_call(call);
    if !tool_requires_approval(&call.name) {
        return false;
    }

    if !config.runtime.auto_approve_readonly {
        return true;
    }

    if call.name == "run_shell"
        && let Some(command) = call
            .arguments
            .get("command")
            .or_else(|| call.arguments.get("cmd"))
            .and_then(|value| value.as_str())
    {
        return shell_command_looks_mutating(command);
    }

    true
}

pub(super) fn shell_command_looks_mutating(command: &str) -> bool {
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

pub(super) fn render_memory_context(store: &HarnessStore, thread_id: &str) -> String {
    let working = store.load_memory(thread_id, MemoryLayer::Working).ok();
    let project = store.load_memory(thread_id, MemoryLayer::Project).ok();
    let mut sections = Vec::new();
    if let Some(memory) = working
        && !memory.entries.is_empty()
    {
        sections.push(format!(
            "[working]\n{}",
            memory
                .entries
                .iter()
                .rev()
                .take(6)
                .rev()
                .map(|entry| format!("- {}", entry.content))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if let Some(memory) = project
        && !memory.entries.is_empty()
    {
        sections.push(format!(
            "[project]\n{}",
            memory
                .entries
                .iter()
                .rev()
                .take(6)
                .rev()
                .map(|entry| format!("- {}", entry.content))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    sections.join("\n\n")
}

pub(super) fn render_skills_context(provider: crate::config::BackendProvider) -> String {
    let skills = SkillAdapter::list(provider);
    if skills.is_empty() {
        return String::new();
    }
    skills
        .into_iter()
        .take(12)
        .map(|skill| format!("- {}：{}", skill.name, skill.description))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn render_session_context(
    store: &HarnessStore,
    thread_id: &str,
    bootstrap_message_limit: usize,
) -> String {
    let mut sections = Vec::new();
    if let Ok(contract) = store.load_execution_contract(thread_id) {
        sections.push(format!(
            "[contract]\ngoal: {}\nfeatures:\n{}",
            contract.goal,
            contract
                .ordered_features
                .iter()
                .map(|feature| format!("- {} [{:?}]", feature.title, feature.status))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if let Ok(progress) = store.load_progress_ledger(thread_id) {
        sections.push(format!(
            "[progress]\nphase: {}\ncompleted: {}\ncurrent: {}\nnext_step: {}\nrecoverable_failure: {}\nblocking: {}",
            progress.current_phase.as_deref().unwrap_or("-"),
            if progress.completed_features.is_empty() {
                "-".to_string()
            } else {
                progress.completed_features.join(", ")
            },
            progress.current_feature.as_deref().unwrap_or("-"),
            progress.next_step.as_deref().unwrap_or("-"),
            progress
                .latest_recoverable_failure
                .as_deref()
                .unwrap_or("-"),
            progress.blocking_reason.as_deref().unwrap_or("-"),
        ));
    }
    if let Ok(bootstrap) = store.read_session_bootstrap(thread_id)
        && !bootstrap.trim().is_empty()
    {
        let limited = bootstrap
            .lines()
            .take(bootstrap_message_limit.max(1))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("[bootstrap]\n{limited}"));
    }
    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::{
        ToolExecutionOutcome, execute_and_record_tool, finish_run, infer_feature_acceptance,
        infer_strategy, split_goal_into_features,
    };
    use crate::config::AppConfig;
    use crate::harness::store::HarnessStore;
    use crate::harness::types::{
        AgentBackendKind, SandboxState, TaskGraphStrategy, ToolCallRequest, ToolCallStatus,
    };
    use crate::model::ThinkingMode;
    use tempfile::TempDir;

    #[test]
    fn finish_run_clears_stale_last_error_after_success() {
        let repo = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(repo.path(), crate::config::BackendProvider::Codex);
        let thread = store.create_thread(Some("测试线程")).expect("thread");
        let mut run = store
            .create_run(
                &thread.id,
                None,
                ThinkingMode::Balanced,
                crate::harness::types::AgentBackendKind::Codex,
            )
            .expect("run");
        run.last_error = Some("旧错误".to_string());
        store.update_run(&thread.id, &run).expect("update run");

        finish_run(&store, &mut run, Some("已完成".to_string()), None).expect("finish run");

        let updated = store.load_run(&thread.id, &run.id).expect("load run");
        assert!(updated.last_error.is_none());
        assert_eq!(updated.summary.as_deref(), Some("已完成"));
    }

    #[test]
    fn infer_strategy_treats_scaffold_goal_as_delivery() {
        let config = AppConfig::default();
        let strategy = infer_strategy(&config, "根据项目文档，给我完成项目骨架");
        assert_eq!(strategy, TaskGraphStrategy::LongRunningDelivery);
    }

    #[test]
    fn infer_strategy_keeps_analysis_goal_as_research() {
        let config = AppConfig::default();
        let strategy = infer_strategy(&config, "请分析这个项目骨架设计");
        assert_eq!(strategy, TaskGraphStrategy::Research);
    }

    #[test]
    fn infer_strategy_treats_run_goal_as_delivery() {
        let config = AppConfig::default();
        let strategy = infer_strategy(&config, "请在沙箱里执行 pwd");
        assert_eq!(strategy, TaskGraphStrategy::LongRunningDelivery);
    }

    #[test]
    fn readonly_summary_feature_uses_inspection_acceptance() {
        let done_when = infer_feature_acceptance("说明 file-a.txt 当前内容。不要修改任何文件。", 1);
        assert!(
            done_when
                .iter()
                .any(|item| item.description.contains("命令或查询"))
        );
        assert!(
            !done_when
                .iter()
                .any(|item| item.description.contains("代码或产物已经落地"))
        );
    }

    #[test]
    fn large_project_goal_is_split_into_stages() {
        let features =
            split_goal_into_features("请根据提示词一步步完成一个完整项目，从 0 到 1 搭建并落地");
        assert_eq!(
            features,
            vec![
                "需求与现状确认".to_string(),
                "项目骨架与执行入口搭建".to_string(),
                "核心功能实现与集成".to_string(),
                "验证、收尾与交付说明".to_string()
            ]
        );
    }

    #[test]
    fn tool_failure_is_recorded_as_recoverable() {
        let repo = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(repo.path(), crate::config::BackendProvider::Codex);
        let thread = store.create_thread(Some("工具失败")).expect("thread");
        let mut run = store
            .create_run(
                &thread.id,
                None,
                ThinkingMode::Balanced,
                AgentBackendKind::Codex,
            )
            .expect("run");
        run.sandbox = Some(SandboxState {
            provider: "test".to_string(),
            image: "test-image".to_string(),
            container_name: "test-box".to_string(),
            workspace_root: run.run_dir.join("sandbox"),
            repo_workdir: repo.path().to_path_buf(),
            container_repo_workdir: "/workspace/repo".into(),
            mount_strategy: "direct_rw".to_string(),
            repair_owner_on_exit: false,
            host_uid: None,
            host_gid: None,
            active: true,
        });
        store.update_run(&thread.id, &run).expect("update run");

        let call = ToolCallRequest {
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path":"missing.txt"}),
        };
        let record = store
            .append_tool_call(&run, &call, Some("task-1".to_string()), None)
            .expect("tool");

        let outcome =
            execute_and_record_tool(&store, &thread, &mut run, &call, &record).expect("execute");
        assert!(matches!(outcome, ToolExecutionOutcome::RecoverableFailure));

        let updated = store
            .list_tool_calls(&run)
            .expect("list tool calls")
            .into_iter()
            .find(|item| item.id == record.id)
            .expect("tool call record");
        assert_eq!(updated.status, ToolCallStatus::Failed);
        assert!(
            updated
                .error
                .as_deref()
                .unwrap_or("")
                .contains("读取文件失败")
        );
    }
}
