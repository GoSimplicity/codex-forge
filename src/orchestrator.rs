use std::collections::{HashMap, HashSet};
use std::fs;

use anyhow::{Context, Result};
use tokio::sync::mpsc;

use crate::apply::{ApplyExecutionContext, build_apply_plan, execute_apply_plan};
use crate::codex::{ensure_codex_available, run_worker};
use crate::commander::{build_plan, build_plan_todo, derive_execution_contract, summarize_run};
use crate::model::{
    ExecutionGraph, ExecutionNode, HandoffArtifact, RoleConfig, RuntimeEvent, SessionConfig,
    SessionManifest, SessionStatus, WorkerLaunchSpec, WorkerResult, WorkerStatus,
};
use crate::repo::discover_repo;
use crate::roles::{find_role, render_worker_prompt};
use crate::session::{SessionContext, find_reusable_plan_session};
use crate::ui::UiController;
use crate::worktree::{WorktreeManager, git_is_clean};

pub async fn plan_session(
    config: SessionConfig,
    roles: Vec<RoleConfig>,
) -> Result<SessionManifest> {
    let repo_snapshot = discover_repo(&config.target_dir)?;
    let mut session = SessionContext::init(&config, repo_snapshot)?;
    let mut ui = UiController::new(&session.manifest.id, &session.manifest.task, config.ui_mode)?;

    record_event(
        &session,
        &mut ui,
        RuntimeEvent::PhaseChanged {
            phase: "规划中".to_string(),
        },
    )?;
    record_event(
        &session,
        &mut ui,
        RuntimeEvent::CommanderNote {
            message: "开始生成用户规划清单与 commander 执行图。".to_string(),
        },
    )?;

    let plan_todo = build_plan_todo(
        &config,
        &session.manifest.repo_snapshot,
        &session.commander_dir(),
    )
    .await?;
    session.set_plan_todo(plan_todo.clone())?;
    record_event(
        &session,
        &mut ui,
        RuntimeEvent::CommanderNote {
            message: format!("计划清单已生成，共 {} 项 todo。", plan_todo.todos.len()),
        },
    )?;

    let graph = build_plan(
        &config,
        &session.manifest.repo_snapshot,
        &roles,
        &session.commander_dir(),
        Some(&plan_todo),
    )
    .await?;
    record_event(
        &session,
        &mut ui,
        RuntimeEvent::GraphReady {
            nodes: graph.nodes.len(),
            dependencies: graph.dependency_count(),
        },
    )?;
    let contract = derive_execution_contract(&config, &graph);
    session.set_execution_contract(contract)?;
    session.set_graph(graph)?;
    session.set_status(SessionStatus::Completed)?;
    ui.finish()?;

    println!("计划已生成：`{}`", session.manifest.id);
    println!(
        "计划清单文件：`{}`",
        session.plan_todo_json_path().display()
    );
    println!("执行图文件：`{}`", session.manifest.graph_path.display());
    Ok(session.manifest)
}

pub async fn run_session(config: SessionConfig, roles: Vec<RoleConfig>) -> Result<SessionManifest> {
    let codex_path = ensure_codex_available()?;
    if matches!(config.apply_mode, crate::model::ApplyMode::AutoSafe) {
        let repo_snapshot = discover_repo(&config.target_dir)?;
        if !git_is_clean(&repo_snapshot.repo_root).await? {
            anyhow::bail!("目标工作区存在未提交改动，auto-safe 模式拒绝运行");
        }
    }

    let repo_snapshot = discover_repo(&config.target_dir)?;
    let mut session = SessionContext::init(&config, repo_snapshot)?;
    let mut ui = UiController::new(&session.manifest.id, &session.manifest.task, config.ui_mode)?;

    record_event(
        &session,
        &mut ui,
        RuntimeEvent::CommanderNote {
            message: format!("检测到 Codex CLI：`{codex_path}`"),
        },
    )?;
    record_event(
        &session,
        &mut ui,
        RuntimeEvent::PhaseChanged {
            phase: "规划中".to_string(),
        },
    )?;

    let reused_plan = find_reusable_plan_session(
        &config.target_dir,
        &config.task,
        config.workers,
        &config.role_set,
    )?;

    let graph = if let Some(plan_manifest) = reused_plan {
        if let Some(plan_todo) = plan_manifest.plan_todo.clone() {
            session.set_plan_todo(plan_todo)?;
        }
        session.set_reused_plan_session_id(plan_manifest.id.clone())?;
        record_event(
            &session,
            &mut ui,
            RuntimeEvent::CommanderNote {
                message: format!("复用 plan 会话：`{}`", plan_manifest.id),
            },
        )?;
        let graph = plan_manifest
            .execution_graph
            .context("复用的 plan 会话缺少执行图")?;
        let contract = plan_manifest
            .execution_contract
            .unwrap_or_else(|| derive_execution_contract(&config, &graph));
        session.set_execution_contract(contract)?;
        graph
    } else {
        let plan_todo = build_plan_todo(
            &config,
            &session.manifest.repo_snapshot,
            &session.commander_dir(),
        )
        .await?;
        session.set_plan_todo(plan_todo.clone())?;
        record_event(
            &session,
            &mut ui,
            RuntimeEvent::CommanderNote {
                message: format!("本次运行生成了 {} 项 todo 计划。", plan_todo.todos.len()),
            },
        )?;
        let graph = build_plan(
            &config,
            &session.manifest.repo_snapshot,
            &roles,
            &session.commander_dir(),
            Some(&plan_todo),
        )
        .await?;
        let contract = derive_execution_contract(&config, &graph);
        session.set_execution_contract(contract)?;
        graph
    };

    record_event(
        &session,
        &mut ui,
        RuntimeEvent::GraphReady {
            nodes: graph.nodes.len(),
            dependencies: graph.dependency_count(),
        },
    )?;
    for note in &graph.planning_notes {
        record_event(
            &session,
            &mut ui,
            RuntimeEvent::CommanderNote {
                message: note.clone(),
            },
        )?;
    }
    session.set_graph(graph.clone())?;
    session.set_status(SessionStatus::Running)?;

    let manager = WorktreeManager::new(
        &session.manifest.repo_snapshot.repo_root,
        &session.manifest.id,
    )?;

    record_event(
        &session,
        &mut ui,
        RuntimeEvent::PhaseChanged {
            phase: "依赖调度中".to_string(),
        },
    )?;

    let mut finished =
        schedule_graph(&config, &roles, &graph, &manager, &mut session, &mut ui).await?;

    if config.cleanup_success {
        for result in &finished {
            if result.status == WorkerStatus::Succeeded {
                let _ = manager.cleanup(&result.worktree_path).await;
            }
        }
        record_event(
            &session,
            &mut ui,
            RuntimeEvent::CommanderNote {
                message: "已清理成功节点的 worker worktree。".to_string(),
            },
        )?;
    }

    record_event(
        &session,
        &mut ui,
        RuntimeEvent::PhaseChanged {
            phase: "集成应用中".to_string(),
        },
    )?;
    session.set_status(SessionStatus::Integrating)?;

    let ordered_worker_ids = graph.topological_order()?;
    let apply_plan = build_apply_plan(
        config.apply_mode,
        &graph,
        &ordered_worker_ids,
        &finished,
        &session.manifest.apply_plan_path,
    )
    .await?;
    session.manifest.artifact_manifest.apply_plan_path =
        Some(session.manifest.apply_plan_path.clone());
    session.persist_artifact_manifest()?;
    record_event(
        &session,
        &mut ui,
        RuntimeEvent::ApplyPlanReady {
            mode: apply_plan.mode,
            operations: apply_plan.operations.len(),
        },
    )?;

    let (apply_result, verification_report, change_trust_report) = execute_apply_plan(
        apply_plan,
        ApplyExecutionContext {
            session_dir: &session.manifest.session_dir,
            repo_root: &session.manifest.repo_snapshot.repo_root,
            worker_results: &finished,
            manager: &manager,
            verification_commands: &config.verification_commands,
            apply_result_path: &session.manifest.apply_result_path,
            verification_report_path: &session.manifest.verification_report_path,
            change_trust_report_path: &session.manifest.change_trust_report_path,
            execution_contract: session
                .manifest
                .execution_contract
                .as_ref()
                .context("当前 session 缺少 execution contract")?,
        },
    )
    .await?;
    record_event(
        &session,
        &mut ui,
        RuntimeEvent::ApplyUpdate {
            message: format!("应用阶段完成：{}", apply_result.status.label()),
        },
    )?;
    record_event(
        &session,
        &mut ui,
        RuntimeEvent::VerificationReady {
            stage: "全链路".to_string(),
            success: matches!(
                verification_report.overall_status,
                crate::model::VerificationOverallStatus::Passed
                    | crate::model::VerificationOverallStatus::Partial
            ),
            message: format!("验证状态：{}", verification_report.overall_status.label()),
        },
    )?;
    session.set_apply_result(apply_result)?;
    session.set_verification_report(verification_report)?;
    session.set_change_trust_report(change_trust_report)?;

    record_event(
        &session,
        &mut ui,
        RuntimeEvent::PhaseChanged {
            phase: "总结中".to_string(),
        },
    )?;

    let summary =
        summarize_run(&config, &session.manifest, &roles, &session.commander_dir()).await?;
    session.set_summary(summary.clone())?;
    record_event(
        &session,
        &mut ui,
        RuntimeEvent::SummaryReady {
            summary: Box::new(summary),
        },
    )?;

    session.set_status(SessionStatus::Completed)?;
    ui.finish()?;

    finished.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
    Ok(session.manifest)
}

async fn schedule_graph(
    config: &SessionConfig,
    roles: &[RoleConfig],
    graph: &ExecutionGraph,
    manager: &WorktreeManager,
    session: &mut SessionContext,
    ui: &mut UiController,
) -> Result<Vec<WorkerResult>> {
    let ordered_ids = graph.topological_order()?;
    let node_map = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();
    let role_map = roles
        .iter()
        .map(|role| (role.key.as_str(), role))
        .collect::<HashMap<_, _>>();

    let (tx, mut rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut handles = HashMap::new();
    let mut pending = ordered_ids.iter().cloned().collect::<HashSet<_>>();
    let mut running = HashSet::new();
    let mut results = Vec::new();
    let mut result_by_id = HashMap::<String, WorkerResult>::new();
    let mut stop_dispatch = false;

    loop {
        let mut dispatched_any = false;
        while !stop_dispatch && running.len() < config.workers {
            let Some(node_id) = next_ready_node(
                &ordered_ids,
                &pending,
                &running,
                &result_by_id,
                &role_map,
                &node_map,
            ) else {
                break;
            };

            let node = node_map
                .get(node_id.as_str())
                .copied()
                .context("ready 节点不存在")?;

            if has_failed_dependency(node, &result_by_id) {
                let skipped = skipped_result(node, "上游依赖失败，当前节点已跳过", session)?;
                session.add_worker_result(skipped.clone())?;
                result_by_id.insert(node.id.clone(), skipped.clone());
                results.push(skipped.clone());
                pending.remove(&node.id);
                record_event(
                    session,
                    ui,
                    RuntimeEvent::WorkerFinished {
                        result: Box::new(skipped),
                    },
                )?;
                continue;
            }

            let worker_dir = session.worker_dir(&node.id);
            fs::create_dir_all(&worker_dir)
                .with_context(|| format!("创建 worker 输出目录失败：{}", worker_dir.display()))?;
            let worktree_path = manager.create(&node.id).await?;
            let role = find_role(roles, &node.role)
                .with_context(|| format!("未找到角色模板：{}", node.role))?;
            let upstream_handoffs = node
                .dependencies
                .iter()
                .filter_map(|dep| result_by_id.get(dep))
                .filter_map(|result| result.handoff.clone())
                .collect::<Vec<HandoffArtifact>>();
            let prompt = render_worker_prompt(
                &role,
                node,
                config,
                &session.manifest.repo_snapshot,
                &upstream_handoffs,
            );
            let launch_spec = WorkerLaunchSpec {
                agent_id: node.id.clone(),
                role: node.role.clone(),
                task_title: node.title.clone(),
                prompt,
                worktree_path: worktree_path.clone(),
                prompt_path: worker_dir.join("prompt.md"),
                stdout_path: worker_dir.join("stdout.log"),
                stderr_path: worker_dir.join("stderr.log"),
                events_path: worker_dir.join("events.jsonl"),
                final_output_path: worker_dir.join("final.md"),
                diff_path: worker_dir.join("changes.patch"),
                git_status_path: worker_dir.join("git-status.txt"),
                handoff_path: worker_dir.join("handoff.json"),
                max_retries: config.max_retries,
            };

            record_event(
                session,
                ui,
                RuntimeEvent::WorkerDispatched {
                    agent_id: node.id.clone(),
                    role: node.role.clone(),
                    title: node.title.clone(),
                    worktree_path: worktree_path.clone(),
                },
            )?;

            let model = config.model.clone();
            let tx_clone = tx.clone();
            let agent_id = node.id.clone();
            let handle =
                tokio::spawn(
                    async move { run_worker(launch_spec, model.as_deref(), tx_clone).await },
                );
            handles.insert(agent_id.clone(), handle);
            pending.remove(&agent_id);
            running.insert(agent_id);
            dispatched_any = true;
        }

        if running.is_empty() {
            if pending.is_empty() {
                break;
            }
            if !dispatched_any {
                for node_id in ordered_ids.iter().filter(|id| pending.contains(*id)) {
                    let node = node_map
                        .get(node_id.as_str())
                        .copied()
                        .context("pending 节点不存在")?;
                    let skipped = skipped_result(node, "依赖未满足或 fail-fast 已触发", session)?;
                    session.add_worker_result(skipped.clone())?;
                    result_by_id.insert(node.id.clone(), skipped.clone());
                    results.push(skipped.clone());
                    record_event(
                        session,
                        ui,
                        RuntimeEvent::WorkerFinished {
                            result: Box::new(skipped),
                        },
                    )?;
                }
                break;
            }
        }

        if let Some(event) = rx.recv().await {
            if let RuntimeEvent::WorkerFinished { result } = &event {
                let result = result.as_ref().clone();
                running.remove(&result.agent_id);
                pending.remove(&result.agent_id);
                result_by_id.insert(result.agent_id.clone(), result.clone());
                results.push(result.clone());
                session.add_worker_result(result.clone())?;
                handles.remove(&result.agent_id);
                if config.fail_fast && result.status == WorkerStatus::Failed {
                    stop_dispatch = true;
                }
            }
            record_event(session, ui, event)?;
        }
    }

    drop(tx);
    for (_id, handle) in handles {
        let _ = handle.await;
    }

    Ok(results)
}

fn next_ready_node(
    ordered_ids: &[String],
    pending: &HashSet<String>,
    running: &HashSet<String>,
    result_by_id: &HashMap<String, WorkerResult>,
    role_map: &HashMap<&str, &RoleConfig>,
    node_map: &HashMap<&str, &ExecutionNode>,
) -> Option<String> {
    ordered_ids.iter().find_map(|node_id| {
        if !pending.contains(node_id) {
            return None;
        }
        let node = node_map.get(node_id.as_str())?;
        if !dependencies_finished(node, result_by_id) {
            return None;
        }
        let role = role_map.get(node.role.as_str())?;
        if let Some(max_concurrency) = role.max_concurrency {
            let running_same_role = running
                .iter()
                .filter(|id| {
                    node_map
                        .get(id.as_str())
                        .map(|item| item.role == node.role)
                        .unwrap_or(false)
                })
                .count();
            if running_same_role >= max_concurrency {
                return None;
            }
        }
        Some(node.id.clone())
    })
}

fn dependencies_finished(
    node: &ExecutionNode,
    result_by_id: &HashMap<String, WorkerResult>,
) -> bool {
    node.dependencies
        .iter()
        .all(|dep| result_by_id.contains_key(dep))
}

fn has_failed_dependency(
    node: &ExecutionNode,
    result_by_id: &HashMap<String, WorkerResult>,
) -> bool {
    node.dependencies.iter().any(|dep| {
        result_by_id
            .get(dep)
            .map(|result| matches!(result.status, WorkerStatus::Failed | WorkerStatus::Skipped))
            .unwrap_or(false)
    })
}

fn skipped_result(
    node: &ExecutionNode,
    reason: &str,
    session: &SessionContext,
) -> Result<WorkerResult> {
    let worker_dir = session.worker_dir(&node.id);
    fs::create_dir_all(&worker_dir)
        .with_context(|| format!("创建 skipped worker 目录失败：{}", worker_dir.display()))?;
    Ok(WorkerResult {
        agent_id: node.id.clone(),
        role: node.role.clone(),
        task_title: node.title.clone(),
        status: WorkerStatus::Skipped,
        exit_code: None,
        attempts: 0,
        diagnostic_summary: Some(reason.to_string()),
        final_message: String::new(),
        summary: None,
        changed_files: Vec::new(),
        worktree_path: worker_dir.join("skipped"),
        prompt_path: worker_dir.join("prompt.md"),
        stdout_path: worker_dir.join("stdout.log"),
        stderr_path: worker_dir.join("stderr.log"),
        events_path: worker_dir.join("events.jsonl"),
        final_output_path: worker_dir.join("final.md"),
        diff_path: Some(worker_dir.join("changes.patch")),
        git_status_path: Some(worker_dir.join("git-status.txt")),
        handoff_path: None,
        handoff: None,
        error: Some(reason.to_string()),
    })
}

fn record_event(
    session: &SessionContext,
    ui: &mut UiController,
    event: RuntimeEvent,
) -> Result<()> {
    session.append_timeline(&event)?;
    ui.apply(&event)
}
