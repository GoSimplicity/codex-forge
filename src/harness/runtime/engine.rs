use std::path::Path;

use anyhow::{Result, anyhow};
use chrono::Utc;

use crate::config::ProjectConfig;

use super::subagent::{evaluate_feature, execute_subagent};
use crate::harness::sandbox::DockerSandboxProvider;
use crate::harness::skills::SkillAdapter;
use crate::harness::store::HarnessStore;
use crate::harness::tools::{
    approval_reason, execute_tool_call, mark_tool_resolution, tool_requires_approval,
};
use crate::harness::types::{
    AcceptanceCriterion, EvaluationDecision, ExecutionContract, FeatureSlice, FeatureSliceStatus,
    HarnessEvent, HarnessMessageRole, HarnessRunManifest, HarnessRunStatus, HarnessThreadManifest,
    MemoryLayer, ProgressLedger, SubagentKind, TaskGraphStrategy, TaskNodeKind, TaskNodeRecord,
    TaskNodeStatus, ToolCallRecord, ToolCallRequest, ToolCallStatus,
};

pub(super) async fn run_execution(
    repo_root: &Path,
    config: &ProjectConfig,
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
            let skills_context = render_skills_context();
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

            execute_task_node(
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
            .await?;
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

fn ensure_sandbox_ready(
    repo_root: &Path,
    config: &ProjectConfig,
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
) -> Result<()> {
    if run.sandbox.is_some() {
        return Ok(());
    }

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
    Ok(())
}

fn ensure_task_graph(
    config: &ProjectConfig,
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

fn infer_strategy(config: &ProjectConfig, goal: &str) -> TaskGraphStrategy {
    let lower = goal.to_lowercase();
    let needs_implementation = [
        "修改",
        "实现",
        "新增",
        "重构",
        "修复",
        "补齐",
        "完善",
        "update",
        "fix",
        "implement",
        "refactor",
    ]
    .iter()
    .any(|keyword| lower.contains(keyword));
    if needs_implementation && config.runtime.enable_long_running_delivery {
        TaskGraphStrategy::LongRunningDelivery
    } else {
        TaskGraphStrategy::Research
    }
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
    config: &ProjectConfig,
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
        completed_features: Vec::new(),
        current_feature: None,
        known_failures: Vec::new(),
        decisions: vec!["已建立 execution contract".to_string()],
        open_questions: Vec::new(),
        next_step: Some("选择下一个 feature".to_string()),
        updated_at: Utc::now(),
    };
    store.save_execution_contract(&thread.id, &contract)?;
    store.save_progress_ledger(&thread.id, &progress)?;
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
            "已生成 contract，features={}",
            contract.ordered_features.len()
        ),
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
        progress.current_feature = Some(feature_id.clone());
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
    config: &ProjectConfig,
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
        progress
            .known_failures
            .push(format!("feature 重试前失败：{}", decision.reason));
        progress.next_step = Some("根据 evaluator 结论重新执行当前 feature".to_string());
        progress.updated_at = Utc::now();
        store.save_progress_ledger(&thread.id, &progress)?;
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
    progress.current_feature = None;
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
    let progress = store.load_progress_ledger(&thread.id)?;
    let latest_evaluation = store.list_evaluations(run)?.into_iter().next();
    let final_response = format!(
        "长期任务已完成。\n目标：{}\n\n已完成 feature：{}\n下一步：{}\n最近评估：{}",
        contract.goal,
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
            done_when: vec![
                AcceptanceCriterion {
                    id: format!("acc-{}-1", index + 1),
                    description: "相关代码或产物已经落地".to_string(),
                },
                AcceptanceCriterion {
                    id: format!("acc-{}-2", index + 1),
                    description: "evaluator 给出通过结论".to_string(),
                },
            ],
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

fn split_goal_into_features(goal: &str) -> Vec<String> {
    goal.split(['\n', '；', ';'])
        .flat_map(|part| part.split("然后"))
        .flat_map(|part| part.split("并"))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
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
        "# Session Bootstrap\n\ngoal: {}\ncompleted_features: {}\ncurrent_feature: {}\nnext_step: {}\nlatest_evaluation: {}\n\ncontract_features:\n{}\n",
        contract.goal,
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
) -> Result<()> {
    let sandbox = run
        .sandbox
        .clone()
        .ok_or_else(|| anyhow!("run 缺少 sandbox"))?;
    let result = execute_tool_call(
        store,
        thread,
        run,
        &sandbox,
        call,
        record.task_node_id.as_deref(),
        record.subagent_id.as_deref(),
    )?;
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

pub(super) fn record_tool_planned(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    call: &ToolCallRequest,
    task_node_id: Option<String>,
    subagent_id: Option<String>,
) -> Result<ToolCallRecord> {
    let record = store.append_tool_call(run, call, task_node_id, subagent_id)?;
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

pub(super) fn cancel_run(store: &HarnessStore, run: &mut HarnessRunManifest) -> Result<()> {
    run.status = HarnessRunStatus::Cancelled;
    run.active_task_node_id = None;
    run.blocked_reason = Some("用户取消当前 run".to_string());
    if let Some(sandbox) = &run.sandbox {
        DockerSandboxProvider {
            image: sandbox.image.clone(),
        }
        .destroy(sandbox)?;
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

pub(super) fn tool_needs_approval(call: &ToolCallRequest) -> bool {
    tool_requires_approval(&call.name)
}

fn render_memory_context(store: &HarnessStore, thread_id: &str) -> String {
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

fn render_skills_context() -> String {
    let skills = SkillAdapter::list();
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

fn render_session_context(
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
            "[progress]\ncompleted: {}\ncurrent: {}\nnext_step: {}",
            if progress.completed_features.is_empty() {
                "-".to_string()
            } else {
                progress.completed_features.join(", ")
            },
            progress.current_feature.as_deref().unwrap_or("-"),
            progress.next_step.as_deref().unwrap_or("-"),
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
