use std::collections::{HashMap, HashSet};
use std::fs;

use anyhow::{Context, Result};
use tokio::sync::{
    mpsc::{self, UnboundedSender},
    watch,
};

use crate::apply::{ApplyExecutionContext, build_apply_plan, execute_apply_plan};
use crate::codex::{ensure_codex_available, run_worker};
use crate::commander::{build_plan, build_plan_todo, derive_execution_contract, summarize_run};
use crate::memory;
use crate::model::{
    BlockedReason, BlockedReasonKind, BrainActionKind, BrainDecision, BrainRiskLevel, BrainState,
    ExecutionGraph, ExecutionNode, HandoffArtifact, RoleConfig, RuntimeEvent, SchedulerSnapshot,
    SessionConfig, SessionManifest, SessionStatus, TodoStatus, WorkerLaunchSpec, WorkerQueueState,
    WorkerResult, WorkerStatus,
};
use crate::repo::discover_repo;
use crate::roles::{WorkerPromptContext, find_role, render_worker_prompt};
use crate::session::{SessionContext, load_session};
use crate::ui::UiController;
use crate::workspace::{cleanup_empty_dirs, prepare_target_dir};
use crate::worktree::{WorktreeManager, git_is_clean, materialize_dependency_patches};

pub async fn run_session(config: SessionConfig, roles: Vec<RoleConfig>) -> Result<SessionManifest> {
    Ok(run_session_inner(config, roles, None, None).await?.manifest)
}

/// 给内嵌 TUI 使用的执行入口。
/// 除了返回 manifest 外，还会：
/// - 向外推送实时 RuntimeEvent
/// - 接受停止信号，用于运行中安全取消
pub async fn run_session_embedded(
    config: SessionConfig,
    roles: Vec<RoleConfig>,
    event_tx: UnboundedSender<RuntimeEvent>,
    stop_rx: Option<watch::Receiver<bool>>,
) -> Result<EmbeddedRunOutcome> {
    run_session_inner(config, roles, Some(event_tx), stop_rx).await
}

async fn run_session_inner(
    config: SessionConfig,
    roles: Vec<RoleConfig>,
    event_tx: Option<UnboundedSender<RuntimeEvent>>,
    stop_rx: Option<watch::Receiver<bool>>,
) -> Result<EmbeddedRunOutcome> {
    let prep = prepare_target_dir(&config.target_dir).await?;
    let codex_path = ensure_codex_available()?;
    if !config.plan_only
        && matches!(
            config.apply_mode,
            crate::model::ApplyMode::AutoSafe | crate::model::ApplyMode::InPlace
        )
    {
        let repo_snapshot = discover_repo(&config.target_dir)?;
        if !git_is_clean(&repo_snapshot.repo_root).await? {
            anyhow::bail!(
                "目标工作区存在未提交改动，{} 模式拒绝运行",
                if matches!(config.apply_mode, crate::model::ApplyMode::InPlace) {
                    "in-place"
                } else {
                    "auto-safe"
                }
            );
        }
    }

    let repo_snapshot = discover_repo(&config.target_dir)?;
    let mut session = SessionContext::init(&config, repo_snapshot)?;
    let mut ui = if event_tx.is_some() {
        UiController::silent(&session.manifest.id, &session.manifest.task)
    } else {
        UiController::new(&session.manifest.id, &session.manifest.task, config.ui_mode)?
    };
    session.set_status(SessionStatus::Preflight)?;

    record_event(
        &mut session,
        &mut ui,
        event_tx.as_ref(),
        RuntimeEvent::CommanderNote {
            message: format!(
                "检测到 Codex CLI：`{codex_path}`；工作目录：{}；git 初始化：{}；本地身份补齐：{}",
                prep.target_dir.display(),
                if prep.git_initialized { "是" } else { "否" },
                if prep.local_identity_configured {
                    "是"
                } else {
                    "否"
                }
            ),
        },
    )?;
    if let Some(continuation) = &config.continuation {
        record_event(
            &mut session,
            &mut ui,
            event_tx.as_ref(),
            RuntimeEvent::CommanderNote {
                message: format!(
                    "基于 session `{}` 进入 V{} 迭代，反馈摘要：{}",
                    continuation.parent_session_id,
                    continuation.iteration_index,
                    continuation.latest_feedback_summary()
                ),
            },
        )?;
    }
    session.set_status(SessionStatus::Planning)?;
    record_event(
        &mut session,
        &mut ui,
        event_tx.as_ref(),
        RuntimeEvent::PhaseChanged {
            phase: "规划中".to_string(),
        },
    )?;

    let resumed_session = if let Some(session_id) = config.resume_session_id.as_deref() {
        Some(load_session(&config.target_dir, Some(session_id))?)
    } else {
        None
    };
    let source_plan = if let Some(session_id) = config.source_plan_session_id.as_deref() {
        Some(load_session(&config.target_dir, Some(session_id))?)
    } else {
        None
    };
    let commander_memory = memory::build_commander_memory_context(
        &session.manifest.repo_snapshot.repo_root,
        Some(&session.manifest.id),
        config.continuation.as_ref(),
    )?;
    record_event(
        &mut session,
        &mut ui,
        event_tx.as_ref(),
        RuntimeEvent::CommanderNote {
            message: format!(
                "已加载共享记忆摘要：历史 session {} 个，记忆条目 {} 条。",
                commander_memory.sessions, commander_memory.entries
            ),
        },
    )?;

    let (graph, seed_results) = if let Some(resume_manifest) = resumed_session {
        if let Some(plan_todo) = resume_manifest.plan_todo.clone() {
            session.set_plan_todo(plan_todo)?;
        }
        session.set_resumed_from_session_id(resume_manifest.id.clone())?;
        record_event(
            &mut session,
            &mut ui,
            event_tx.as_ref(),
            RuntimeEvent::CommanderNote {
                message: format!(
                    "恢复 session：`{}`，将复用其执行图与已成功节点。",
                    resume_manifest.id
                ),
            },
        )?;
        let graph = resume_manifest
            .execution_graph
            .clone()
            .context("恢复 session 缺少执行图")?;
        let contract = resume_manifest
            .execution_contract
            .clone()
            .unwrap_or_else(|| derive_execution_contract(&config, &graph));
        session.set_execution_contract(contract)?;
        let seed_results = resume_manifest
            .worker_results
            .into_iter()
            .filter(|result| result.status == WorkerStatus::Succeeded)
            .collect::<Vec<_>>();
        (graph, seed_results)
    } else if let Some(plan_manifest) = source_plan {
        if !plan_manifest.is_plan_session() {
            anyhow::bail!(
                "`{}` 不是 plan session，不能作为 run --from-plan 来源",
                plan_manifest.id
            );
        }
        if plan_manifest.status != SessionStatus::Completed {
            anyhow::bail!("plan session `{}` 尚未完成，不能直接执行", plan_manifest.id);
        }
        if plan_manifest.task != config.task {
            anyhow::bail!(
                "run 的任务描述与 plan session 不一致：`{}` != `{}`",
                config.task,
                plan_manifest.task
            );
        }
        if let Some(plan_todo) = plan_manifest.plan_todo.clone() {
            session.set_plan_todo(plan_todo)?;
        }
        session.set_source_plan_session_id(plan_manifest.id.clone())?;
        record_event(
            &mut session,
            &mut ui,
            event_tx.as_ref(),
            RuntimeEvent::CommanderNote {
                message: format!("基于 plan 会话执行：`{}`", plan_manifest.id),
            },
        )?;
        let graph = plan_manifest
            .execution_graph
            .context("指定的 plan 会话缺少执行图")?;
        let contract = plan_manifest
            .execution_contract
            .unwrap_or_else(|| derive_execution_contract(&config, &graph));
        session.set_execution_contract(contract)?;
        (graph, Vec::new())
    } else {
        let plan_todo = build_plan_todo(
            &config,
            &session.manifest.repo_snapshot,
            &session.commander_dir(),
            Some(&commander_memory.prompt_block),
        )
        .await?;
        session.set_plan_todo(plan_todo.clone())?;
        record_event(
            &mut session,
            &mut ui,
            event_tx.as_ref(),
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
            Some(&commander_memory.prompt_block),
        )
        .await?;
        let contract = derive_execution_contract(&config, &graph);
        session.set_execution_contract(contract)?;
        (graph, Vec::new())
    };

    session.set_status(SessionStatus::Ready)?;
    record_event(
        &mut session,
        &mut ui,
        event_tx.as_ref(),
        RuntimeEvent::GraphReady {
            nodes: graph.nodes.len(),
            dependencies: graph.dependency_count(),
        },
    )?;
    for note in &graph.planning_notes {
        record_event(
            &mut session,
            &mut ui,
            event_tx.as_ref(),
            RuntimeEvent::CommanderNote {
                message: note.clone(),
            },
        )?;
    }
    session.set_graph(graph.clone())?;

    if config.plan_only {
        record_event(
            &mut session,
            &mut ui,
            event_tx.as_ref(),
            RuntimeEvent::PhaseChanged {
                phase: "方案已完成".to_string(),
            },
        )?;
        record_event(
            &mut session,
            &mut ui,
            event_tx.as_ref(),
            RuntimeEvent::CommanderNote {
                message: "方案会话已完成；可在历史页查看详情，或直接执行此方案。".to_string(),
            },
        )?;
        session.set_status(SessionStatus::Completed)?;
        ui.finish()?;
        let _ = cleanup_empty_dirs(
            &session
                .manifest
                .repo_snapshot
                .repo_root
                .join(".codex-forge"),
        );
        if event_tx.is_none() {
            println!("方案已生成：`{}`", session.manifest.id);
            println!(
                "计划清单文件：`{}`",
                session.plan_todo_json_path().display()
            );
            println!("执行图文件：`{}`", session.manifest.graph_path.display());
            println!(
                "执行此方案：`codex-forge run --from-plan {} \"{}\"`",
                session.manifest.id, session.manifest.task
            );
            println!(
                "继续反馈：`codex-forge continue --session {} --feedback \"...\"`",
                session.manifest.id
            );
        }
        return Ok(EmbeddedRunOutcome {
            manifest: session.manifest,
            stopped: false,
        });
    }

    session.set_status(SessionStatus::Running)?;

    let manager = WorktreeManager::new(
        &session.manifest.repo_snapshot.repo_root,
        &session.manifest.id,
    )?;

    record_event(
        &mut session,
        &mut ui,
        event_tx.as_ref(),
        RuntimeEvent::PhaseChanged {
            phase: "依赖调度中".to_string(),
        },
    )?;

    // 调度阶段负责并发 worker、依赖推进和停止收敛。
    let ScheduleOutcome {
        results: mut finished,
        stopped,
    } = schedule_graph(
        ScheduleInputs {
            config: &config,
            roles: &roles,
            graph: &graph,
            manager: &manager,
            seed_results: &seed_results,
            stop_rx: stop_rx.clone(),
        },
        &mut session,
        &mut ui,
        event_tx.as_ref(),
    )
    .await?;

    if config.cleanup_success {
        for result in &finished {
            if result.status == WorkerStatus::Succeeded {
                let _ = manager.cleanup(&result.worktree_path).await;
            }
        }
        record_event(
            &mut session,
            &mut ui,
            event_tx.as_ref(),
            RuntimeEvent::CommanderNote {
                message: "已清理成功节点的 worker worktree。".to_string(),
            },
        )?;
    }

    if stopped {
        // 用户主动停止时，不再继续进入 apply / verify / summary 正常收敛链路；
        // 但仍尽量把现有中间状态落盘，保证历史查看和 replay 不丢信息。
        record_event(
            &mut session,
            &mut ui,
            event_tx.as_ref(),
            RuntimeEvent::PhaseChanged {
                phase: "已停止".to_string(),
            },
        )?;
        record_event(
            &mut session,
            &mut ui,
            event_tx.as_ref(),
            RuntimeEvent::CommanderNote {
                message: "收到停止信号，已跳过 apply / verify / summary 收敛阶段。".to_string(),
            },
        )?;
        for todo in session.manifest.todo_states.clone() {
            let next_status = match todo.status {
                TodoStatus::Pending | TodoStatus::Ready | TodoStatus::Running => {
                    Some(TodoStatus::Blocked)
                }
                TodoStatus::InReview | TodoStatus::Verifying => {
                    Some(TodoStatus::NeedsManualFollowup)
                }
                _ => None,
            };
            if let Some(status) = next_status {
                update_todo_status(
                    &mut session,
                    &mut ui,
                    event_tx.as_ref(),
                    &todo.todo_id,
                    status,
                    "用户停止运行，保留当前产物供后续继续处理",
                    None,
                )?;
            }
        }
        session.set_status(SessionStatus::Blocked)?;
        ui.finish()?;
        let _ = cleanup_empty_dirs(
            &session
                .manifest
                .repo_snapshot
                .repo_root
                .join(".codex-forge"),
        );

        finished.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
        return Ok(EmbeddedRunOutcome {
            manifest: session.manifest,
            stopped: true,
        });
    }

    session.set_status(SessionStatus::Reviewing)?;
    record_event(
        &mut session,
        &mut ui,
        event_tx.as_ref(),
        RuntimeEvent::PhaseChanged {
            phase: "集成应用中".to_string(),
        },
    )?;
    let review_focus_todos = session
        .manifest
        .todo_states
        .iter()
        .filter(|todo| {
            matches!(
                todo.status,
                TodoStatus::Pending | TodoStatus::Ready | TodoStatus::Running
            )
        })
        .map(|todo| todo.todo_id.clone())
        .collect::<Vec<_>>();
    record_event(
        &mut session,
        &mut ui,
        event_tx.as_ref(),
        RuntimeEvent::BrainDecisionMade {
            decision: Box::new(BrainDecision {
                action: BrainActionKind::AdvancePhase,
                summary: "Brain 推进到审阅与集成阶段".to_string(),
                rationale: "所有可执行节点已收敛，下一步由 reviewer gate、apply 和 verify 接管。"
                    .to_string(),
                target_agents: Vec::new(),
                focus_todos: review_focus_todos,
                risk_level: BrainRiskLevel::Medium,
            }),
        },
    )?;
    session.set_status(SessionStatus::Applying)?;
    for todo in session.manifest.todo_states.clone() {
        if matches!(
            todo.status,
            TodoStatus::Pending | TodoStatus::Ready | TodoStatus::Running
        ) {
            update_todo_status(
                &mut session,
                &mut ui,
                event_tx.as_ref(),
                &todo.todo_id,
                TodoStatus::InReview,
                "进入 reviewer gate 与集成应用阶段",
                None,
            )?;
        }
    }

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
        &mut session,
        &mut ui,
        event_tx.as_ref(),
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
            todo_states: &session.manifest.todo_states,
        },
    )
    .await?;
    if let Some(review_report) = apply_result.review_report.clone() {
        let all_focus_todos = session
            .manifest
            .todo_states
            .iter()
            .map(|todo| todo.todo_id.clone())
            .collect::<Vec<_>>();
        record_event(
            &mut session,
            &mut ui,
            event_tx.as_ref(),
            RuntimeEvent::ReviewGateReady {
                report: Box::new(review_report),
            },
        )?;
        record_event(
            &mut session,
            &mut ui,
            event_tx.as_ref(),
            RuntimeEvent::BrainDecisionMade {
                decision: Box::new(BrainDecision {
                    action: if matches!(
                        apply_result
                            .review_report
                            .as_ref()
                            .map(|report| report.decision),
                        Some(crate::model::ApplyDecision::Block)
                    ) {
                        BrainActionKind::EscalateToUser
                    } else {
                        BrainActionKind::AdvancePhase
                    },
                    summary: format!(
                        "Brain 收到 review gate 结论：{}",
                        apply_result
                            .review_report
                            .as_ref()
                            .map(|report| report.decision.label())
                            .unwrap_or("未知")
                    ),
                    rationale: apply_result
                        .review_report
                        .as_ref()
                        .and_then(|report| report.confidence_reasoning.clone())
                        .unwrap_or_else(|| "继续根据 gate 结果推进 apply / verify。".to_string()),
                    target_agents: Vec::new(),
                    focus_todos: all_focus_todos,
                    risk_level: match apply_result
                        .review_report
                        .as_ref()
                        .map(|report| report.decision)
                    {
                        Some(crate::model::ApplyDecision::Block) => BrainRiskLevel::High,
                        Some(crate::model::ApplyDecision::AllowPartial) => BrainRiskLevel::Medium,
                        _ => BrainRiskLevel::Low,
                    },
                }),
            },
        )?;
    }
    record_event(
        &mut session,
        &mut ui,
        event_tx.as_ref(),
        RuntimeEvent::ApplyUpdate {
            message: format!("应用阶段完成：{}", apply_result.status.label()),
        },
    )?;
    session.set_status(SessionStatus::Verifying)?;
    record_event(
        &mut session,
        &mut ui,
        event_tx.as_ref(),
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
    if let Some(report) = session.manifest.verification_report.as_ref() {
        let entries = memory::append_verification_memory(
            &session.manifest.repo_snapshot.repo_root,
            &session.manifest.id,
            report,
        )?;
        session.sync_memory_manifest()?;
        if let Some(path) = session
            .manifest
            .artifact_manifest
            .session_memory_entries_path
            .clone()
        {
            record_event(
                &mut session,
                &mut ui,
                event_tx.as_ref(),
                RuntimeEvent::MemoryUpdated {
                    scope: "session".to_string(),
                    reason: "verification".to_string(),
                    path,
                    entries,
                },
            )?;
        }
    }
    if session
        .manifest
        .apply_result
        .as_ref()
        .is_some_and(|item| item.todo_commits.is_empty())
    {
        let fallback_status = match session
            .manifest
            .apply_result
            .as_ref()
            .map(|item| item.status)
        {
            Some(crate::model::ApplyStatus::Applied) => TodoStatus::Applied,
            Some(crate::model::ApplyStatus::VerificationFailed)
            | Some(crate::model::ApplyStatus::WrittenNeedsFix) => TodoStatus::Failed,
            Some(crate::model::ApplyStatus::Bundled)
            | Some(crate::model::ApplyStatus::Skipped)
            | Some(crate::model::ApplyStatus::SyncFailed)
            | None => TodoStatus::NeedsManualFollowup,
        };
        for todo in session.manifest.todo_states.clone() {
            if !matches!(
                todo.status,
                TodoStatus::Committed | TodoStatus::Failed | TodoStatus::Blocked
            ) {
                update_todo_status(
                    &mut session,
                    &mut ui,
                    event_tx.as_ref(),
                    &todo.todo_id,
                    fallback_status,
                    "当前 todo 尚未形成自动提交结果，请按 summary 做后续处理",
                    None,
                )?;
            }
        }
    }
    for record in session
        .manifest
        .apply_result
        .as_ref()
        .map(|item| item.todo_commits.clone())
        .unwrap_or_default()
    {
        update_todo_status(
            &mut session,
            &mut ui,
            event_tx.as_ref(),
            &record.todo_id,
            record.status,
            record.message,
            record.commit_hash,
        )?;
    }

    session.set_status(SessionStatus::Summarizing)?;
    record_event(
        &mut session,
        &mut ui,
        event_tx.as_ref(),
        RuntimeEvent::PhaseChanged {
            phase: "总结中".to_string(),
        },
    )?;
    let summary_focus_todos = session
        .manifest
        .todo_states
        .iter()
        .map(|todo| todo.todo_id.clone())
        .collect::<Vec<_>>();
    record_event(
        &mut session,
        &mut ui,
        event_tx.as_ref(),
        RuntimeEvent::BrainDecisionMade {
            decision: Box::new(BrainDecision {
                action: BrainActionKind::AdvancePhase,
                summary: "Brain 开始汇总最终交付".to_string(),
                rationale: "审阅、应用和验证已经完成，接下来生成统一摘要与导出物。".to_string(),
                target_agents: Vec::new(),
                focus_todos: summary_focus_todos,
                risk_level: BrainRiskLevel::Low,
            }),
        },
    )?;

    let summary =
        summarize_run(&config, &session.manifest, &roles, &session.commander_dir()).await?;
    session.set_summary(summary.clone())?;
    let summary_entries = memory::append_summary_memory(
        &session.manifest.repo_snapshot.repo_root,
        &session.manifest,
        &summary,
    )?;
    session.sync_memory_manifest()?;
    if let Some(path) = session.manifest.artifact_manifest.task_brief_path.clone() {
        record_event(
            &mut session,
            &mut ui,
            event_tx.as_ref(),
            RuntimeEvent::MemoryUpdated {
                scope: "session".to_string(),
                reason: "summary".to_string(),
                path,
                entries: summary_entries,
            },
        )?;
    }
    record_event(
        &mut session,
        &mut ui,
        event_tx.as_ref(),
        RuntimeEvent::SummaryReady {
            summary: Box::new(summary),
        },
    )?;

    session.set_status(SessionStatus::Completed)?;
    ui.finish()?;
    let _ = cleanup_empty_dirs(
        &session
            .manifest
            .repo_snapshot
            .repo_root
            .join(".codex-forge"),
    );

    finished.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
    if event_tx.is_none() {
        if session.manifest.wrote_to_target() {
            println!("运行完成，代码已写入目标目录：`{}`", session.manifest.id);
        } else {
            println!(
                "运行结束，但本轮尚未写入目标目录：`{}`",
                session.manifest.id
            );
            if let Some(apply_result) = session.manifest.apply_result.as_ref() {
                if let Some(bundle_dir) = &apply_result.bundle_dir {
                    println!("bundle 目录：`{}`", bundle_dir.display());
                }
                println!(
                    "review gate：{}；apply：{}；accepted_files：{}",
                    apply_result
                        .review_gate
                        .map(|gate| gate.label().to_string())
                        .unwrap_or_else(|| "无".to_string()),
                    apply_result.status.label(),
                    apply_result.accepted_files.len()
                );
            }
        }
        println!(
            "摘要文件：`{}`",
            session.manifest.summary_markdown_path.display()
        );
        println!(
            "继续反馈：`codex-forge continue --session {} --feedback \"...\"`",
            session.manifest.id
        );
    }
    Ok(EmbeddedRunOutcome {
        manifest: session.manifest,
        stopped: false,
    })
}

#[derive(Debug)]
pub struct EmbeddedRunOutcome {
    /// 无论成功、失败还是停止，都返回当前时刻的 manifest。
    pub manifest: SessionManifest,
    /// `true` 表示用户主动停止，而不是执行失败。
    pub stopped: bool,
}

#[derive(Debug)]
struct ScheduleOutcome {
    results: Vec<WorkerResult>,
    stopped: bool,
}

struct ScheduleInputs<'a> {
    config: &'a SessionConfig,
    roles: &'a [RoleConfig],
    graph: &'a ExecutionGraph,
    manager: &'a WorktreeManager,
    seed_results: &'a [WorkerResult],
    stop_rx: Option<watch::Receiver<bool>>,
}

#[derive(Debug, Clone)]
struct NodeSchedulingMeta {
    topo_index: usize,
    critical_depth: usize,
    downstream_fanout: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NodeRuntimeState {
    queue_state: WorkerQueueState,
    blocked_reason: Option<BlockedReason>,
    lane: Option<usize>,
}

#[derive(Debug, Clone)]
struct SchedulerAssessment {
    ready_ids: Vec<String>,
    node_states: HashMap<String, NodeRuntimeState>,
    snapshot: SchedulerSnapshot,
}

#[derive(Debug, Clone)]
struct BrainRuntime {
    state: BrainState,
    last_snapshot: Option<SchedulerSnapshot>,
    last_node_states: HashMap<String, NodeRuntimeState>,
}

struct SchedulerContext<'a> {
    pending: &'a HashSet<String>,
    running: &'a HashSet<String>,
    result_by_id: &'a HashMap<String, WorkerResult>,
    role_map: &'a HashMap<&'a str, &'a RoleConfig>,
    node_map: &'a HashMap<&'a str, &'a ExecutionNode>,
    scheduling_meta: &'a HashMap<String, NodeSchedulingMeta>,
    max_workers: usize,
    stop_dispatch: bool,
}

struct ReadyNode<'a> {
    id: &'a str,
    node: &'a ExecutionNode,
    meta: &'a NodeSchedulingMeta,
}

async fn schedule_graph(
    inputs: ScheduleInputs<'_>,
    session: &mut SessionContext,
    ui: &mut UiController,
    event_tx: Option<&UnboundedSender<RuntimeEvent>>,
) -> Result<ScheduleOutcome> {
    let ordered_ids = inputs.graph.topological_order()?;
    let node_map = inputs
        .graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();
    let role_map = inputs
        .roles
        .iter()
        .map(|role| (role.key.as_str(), role))
        .collect::<HashMap<_, _>>();
    let scheduling_meta = build_node_scheduling_meta(inputs.graph, &ordered_ids)?;

    let (tx, mut rx) = mpsc::channel::<RuntimeEvent>(256);
    let mut handles = HashMap::new();
    let mut pending = ordered_ids.iter().cloned().collect::<HashSet<_>>();
    let mut running = HashSet::new();
    let mut results = Vec::new();
    let mut result_by_id = HashMap::<String, WorkerResult>::new();
    let mut stop_dispatch = false;
    let mut stopped = false;
    let mut brain = BrainRuntime {
        state: BrainState {
            status: "在线".to_string(),
            objective: format!(
                "统一安排“{}”的多 agent 执行、审批、检查、优化与测试。",
                inputs.config.task
            ),
            current_focus: "接管执行图并准备首轮派发".to_string(),
            latest_thought: "先识别关键路径和可立即并行的节点。".to_string(),
            latest_decision: "等待首批可派发节点。".to_string(),
            risk_level: BrainRiskLevel::Low,
            needs_user_attention: false,
        },
        last_snapshot: None,
        last_node_states: HashMap::new(),
    };
    record_event(
        session,
        ui,
        event_tx,
        RuntimeEvent::BrainStarted {
            state: Box::new(brain.state.clone()),
        },
    )?;
    record_event(
        session,
        ui,
        event_tx,
        RuntimeEvent::BrainThought {
            thought: brain.state.latest_thought.clone(),
        },
    )?;

    for result in inputs.seed_results {
        pending.remove(&result.agent_id);
        result_by_id.insert(result.agent_id.clone(), result.clone());
        results.push(result.clone());
        session.add_worker_result(result.clone())?;
        if let Some(node) = node_map.get(result.agent_id.as_str()).copied()
            && let Some(todo_id) = node.todo_id.as_deref()
        {
            session.mark_todo_node_completed(todo_id, &result.agent_id)?;
            update_todo_status(
                session,
                ui,
                event_tx,
                todo_id,
                TodoStatus::Ready,
                format!("复用历史成功节点 {}", result.agent_id),
                None,
            )?;
        }
        record_event(
            session,
            ui,
            event_tx,
            RuntimeEvent::WorkerFinished {
                result: Box::new(result.clone()),
            },
        )?;
    }
    sync_brain_runtime(
        &mut brain,
        inputs.graph,
        &ordered_ids,
        &SchedulerContext {
            pending: &pending,
            running: &running,
            result_by_id: &result_by_id,
            role_map: &role_map,
            node_map: &node_map,
            scheduling_meta: &scheduling_meta,
            max_workers: inputs.config.workers,
            stop_dispatch,
        },
        session,
        ui,
        event_tx,
    )?;

    loop {
        if !stop_dispatch && stop_requested(inputs.stop_rx.as_ref()) {
            stop_dispatch = true;
            stopped = true;
            brain.state.current_focus = "停止继续派发新节点".to_string();
            brain.state.latest_decision = "停止接收新派发，等待在跑节点安全退出。".to_string();
            brain.state.latest_thought = "用户要求停止运行，Brain 已进入收敛保护模式。".to_string();
            brain.state.risk_level = BrainRiskLevel::High;
            brain.state.needs_user_attention = true;
            record_event(
                session,
                ui,
                event_tx,
                RuntimeEvent::BrainEscalationRaised {
                    message: "用户触发停止，Brain 将停止派发新节点，并保留当前产物。".to_string(),
                },
            )?;
            // 收到 stop 后先停止派发新节点，已在跑的 worker 则依赖同一个 stop signal 自行退出。
            record_event(
                session,
                ui,
                event_tx,
                RuntimeEvent::CommanderNote {
                    message: "收到停止信号，停止继续派发新节点，并等待在跑 worker 安全退出。"
                        .to_string(),
                },
            )?;
            sync_brain_runtime(
                &mut brain,
                inputs.graph,
                &ordered_ids,
                &SchedulerContext {
                    pending: &pending,
                    running: &running,
                    result_by_id: &result_by_id,
                    role_map: &role_map,
                    node_map: &node_map,
                    scheduling_meta: &scheduling_meta,
                    max_workers: inputs.config.workers,
                    stop_dispatch,
                },
                session,
                ui,
                event_tx,
            )?;
        }

        let mut dispatched_any = false;
        while !stop_dispatch && running.len() < inputs.config.workers {
            let scheduler = SchedulerContext {
                pending: &pending,
                running: &running,
                result_by_id: &result_by_id,
                role_map: &role_map,
                node_map: &node_map,
                scheduling_meta: &scheduling_meta,
                max_workers: inputs.config.workers,
                stop_dispatch,
            };
            let assessment = assess_scheduler(&ordered_ids, &scheduler);
            sync_brain_runtime(
                &mut brain,
                inputs.graph,
                &ordered_ids,
                &scheduler,
                session,
                ui,
                event_tx,
            )?;
            let Some(node_id) = assessment.ready_ids.first().cloned() else {
                update_brain_thought(
                    &mut brain,
                    if assessment.snapshot.blocked_role_limit_count > 0 {
                        "当前有节点已就绪，但受角色并发上限约束，先让其他分支继续推进。"
                    } else if assessment.snapshot.blocked_dependency_count > 0 {
                        "当前没有可立即派发的节点，等待运行中的 agent 解锁更多依赖。"
                    } else {
                        "Brain 正在等待下一批可调度事件。"
                    },
                    session,
                    ui,
                    event_tx,
                )?;
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
                    event_tx,
                    RuntimeEvent::WorkerFinished {
                        result: Box::new(skipped),
                    },
                )?;
                continue;
            }
            record_brain_decision(
                &mut brain,
                BrainDecision {
                    action: BrainActionKind::DispatchNode,
                    summary: format!("派发 {}", node.id),
                    rationale: build_dispatch_rationale(
                        node,
                        &scheduling_meta,
                        &pending,
                        &node_map,
                    ),
                    target_agents: vec![node.id.clone()],
                    focus_todos: node.todo_id.clone().into_iter().collect(),
                    risk_level: BrainRiskLevel::Low,
                },
                session,
                ui,
                event_tx,
            )?;

            let worker_dir = session.worker_dir(&node.id);
            fs::create_dir_all(&worker_dir)
                .with_context(|| format!("创建 worker 输出目录失败：{}", worker_dir.display()))?;
            let worktree_path = inputs.manager.create(&node.id).await?;
            let role = find_role(inputs.roles, &node.role)
                .with_context(|| format!("未找到角色模板：{}", node.role))?;
            let dependency_results = node
                .dependencies
                .iter()
                .filter_map(|dep| result_by_id.get(dep))
                .collect::<Vec<_>>();
            if !node.allow_code_changes {
                let (materialized, materialize_failures) =
                    materialize_dependency_patches(&worktree_path, &dependency_results).await?;
                if !materialized.is_empty() {
                    record_event(
                        session,
                        ui,
                        event_tx,
                        RuntimeEvent::CommanderNote {
                            message: format!(
                                "已为 `{}` 预铺依赖 patch：{}",
                                node.id,
                                materialized.join("、")
                            ),
                        },
                    )?;
                }
                if !materialize_failures.is_empty() {
                    record_event(
                        session,
                        ui,
                        event_tx,
                        RuntimeEvent::CommanderNote {
                            message: format!(
                                "为 `{}` 预铺依赖 patch 时有冲突，交由下游节点审阅：{}",
                                node.id,
                                materialize_failures.join("；")
                            ),
                        },
                    )?;
                }
            }
            let upstream_handoffs = node
                .dependencies
                .iter()
                .filter_map(|dep| result_by_id.get(dep))
                .filter_map(|result| result.handoff.clone())
                .collect::<Vec<HandoffArtifact>>();
            let materialized_memory = memory::build_worker_memory_view(
                &session.manifest,
                &node.id,
                &node.role,
                &dependency_results,
            )?;
            session.sync_memory_manifest()?;
            let node_contract = session
                .manifest
                .execution_contract
                .as_ref()
                .and_then(|contract| contract.node_contract(&node.id));
            let prompt = render_worker_prompt(
                &role,
                node,
                inputs.config,
                &session.manifest.repo_snapshot,
                WorkerPromptContext {
                    upstream_handoffs: &upstream_handoffs,
                    memory_prompt: &memory::render_memory_prompt_block(&materialized_memory.view),
                    allowed_paths: node_contract
                        .map(|contract| contract.allowed_paths.as_slice())
                        .unwrap_or(&[]),
                    forbidden_paths: node_contract
                        .map(|contract| contract.forbidden_paths.as_slice())
                        .unwrap_or(&[]),
                    review_fix: inputs
                        .config
                        .continuation
                        .as_ref()
                        .and_then(|item| item.review_fix.as_ref()),
                },
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
                max_retries: inputs.config.max_retries,
            };
            record_event(
                session,
                ui,
                event_tx,
                RuntimeEvent::MemoryViewReady {
                    agent_id: node.id.clone(),
                    memory_view_path: materialized_memory.markdown_path.clone(),
                    entries: materialized_memory.view.entries.len(),
                },
            )?;

            record_event(
                session,
                ui,
                event_tx,
                RuntimeEvent::WorkerDispatched {
                    agent_id: node.id.clone(),
                    role: node.role.clone(),
                    title: node.title.clone(),
                    worktree_path: worktree_path.clone(),
                },
            )?;
            if let Some(todo_id) = node.todo_id.as_deref() {
                update_todo_status(
                    session,
                    ui,
                    event_tx,
                    todo_id,
                    TodoStatus::Running,
                    format!("开始执行节点 {}", node.id),
                    None,
                )?;
            }

            let model = inputs.config.model.clone();
            let thinking_mode = inputs.config.thinking_mode;
            let tx_clone = tx.clone();
            let agent_id = node.id.clone();
            let worker_stop_rx = inputs.stop_rx.clone();
            let handle = tokio::spawn(async move {
                run_worker(
                    launch_spec,
                    model.as_deref(),
                    thinking_mode,
                    tx_clone,
                    worker_stop_rx,
                )
                .await
            });
            handles.insert(agent_id.clone(), handle);
            pending.remove(&agent_id);
            running.insert(agent_id);
            dispatched_any = true;
            sync_brain_runtime(
                &mut brain,
                inputs.graph,
                &ordered_ids,
                &SchedulerContext {
                    pending: &pending,
                    running: &running,
                    result_by_id: &result_by_id,
                    role_map: &role_map,
                    node_map: &node_map,
                    scheduling_meta: &scheduling_meta,
                    max_workers: inputs.config.workers,
                    stop_dispatch,
                },
                session,
                ui,
                event_tx,
            )?;
        }

        if running.is_empty() {
            if pending.is_empty() {
                break;
            }
            if stop_dispatch {
                break;
            }
            if !dispatched_any {
                let assessment = assess_scheduler(
                    &ordered_ids,
                    &SchedulerContext {
                        pending: &pending,
                        running: &running,
                        result_by_id: &result_by_id,
                        role_map: &role_map,
                        node_map: &node_map,
                        scheduling_meta: &scheduling_meta,
                        max_workers: inputs.config.workers,
                        stop_dispatch,
                    },
                );
                if assessment.snapshot.blocked_upstream_failed_count > 0 {
                    record_event(
                        session,
                        ui,
                        event_tx,
                        RuntimeEvent::BrainEscalationRaised {
                            message: "存在上游失败导致的阻塞子树，Brain 将把剩余节点标记为跳过。"
                                .to_string(),
                        },
                    )?;
                } else {
                    record_brain_decision(
                        &mut brain,
                        BrainDecision {
                            action: BrainActionKind::Hold,
                            summary: "当前没有可继续推进的 ready 节点".to_string(),
                            rationale: "所有剩余节点都受依赖或策略约束，Brain 将结束本轮调度。"
                                .to_string(),
                            target_agents: Vec::new(),
                            focus_todos: Vec::new(),
                            risk_level: BrainRiskLevel::Medium,
                        },
                        session,
                        ui,
                        event_tx,
                    )?;
                }
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
                        event_tx,
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
                if let Some(node) = node_map.get(result.agent_id.as_str()).copied()
                    && let Some(todo_id) = node.todo_id.as_deref()
                {
                    match result.status {
                        WorkerStatus::Succeeded => {
                            session.mark_todo_node_completed(todo_id, &result.agent_id)?;
                            update_todo_status(
                                session,
                                ui,
                                event_tx,
                                todo_id,
                                TodoStatus::Running,
                                format!("节点 {} 已完成，等待 todo 汇总验证", result.agent_id),
                                None,
                            )?;
                        }
                        WorkerStatus::Failed => {
                            update_todo_status(
                                session,
                                ui,
                                event_tx,
                                todo_id,
                                TodoStatus::Failed,
                                format!("节点 {} 执行失败", result.agent_id),
                                None,
                            )?;
                        }
                        WorkerStatus::Skipped => {
                            update_todo_status(
                                session,
                                ui,
                                event_tx,
                                todo_id,
                                TodoStatus::Blocked,
                                format!("节点 {} 被跳过", result.agent_id),
                                None,
                            )?;
                        }
                        WorkerStatus::Pending | WorkerStatus::Running => {}
                    }
                }
                running.remove(&result.agent_id);
                pending.remove(&result.agent_id);
                result_by_id.insert(result.agent_id.clone(), result.clone());
                results.push(result.clone());
                session.add_worker_result(result.clone())?;
                let entries = memory::append_worker_memory(
                    &session.manifest.repo_snapshot.repo_root,
                    &session.manifest.id,
                    &result,
                )?;
                if entries > 0 {
                    session.sync_memory_manifest()?;
                    if let Some(path) = session
                        .manifest
                        .artifact_manifest
                        .session_memory_entries_path
                        .clone()
                    {
                        record_event(
                            session,
                            ui,
                            event_tx,
                            RuntimeEvent::MemoryUpdated {
                                scope: "session".to_string(),
                                reason: format!("worker {}", result.agent_id),
                                path,
                                entries,
                            },
                        )?;
                    }
                }
                handles.remove(&result.agent_id);
                if inputs.config.fail_fast && result.status == WorkerStatus::Failed {
                    stop_dispatch = true;
                    let focus_todos = node_map
                        .get(result.agent_id.as_str())
                        .and_then(|node| node.todo_id.clone())
                        .into_iter()
                        .collect();
                    record_brain_decision(
                        &mut brain,
                        BrainDecision {
                            action: BrainActionKind::Hold,
                            summary: format!("{} 失败，触发 fail-fast", result.agent_id),
                            rationale: "当前配置要求在失败后停止继续派发，优先保护集成质量。"
                                .to_string(),
                            target_agents: vec![result.agent_id.clone()],
                            focus_todos,
                            risk_level: BrainRiskLevel::High,
                        },
                        session,
                        ui,
                        event_tx,
                    )?;
                }
            }
            record_event(session, ui, event_tx, event)?;
            sync_brain_runtime(
                &mut brain,
                inputs.graph,
                &ordered_ids,
                &SchedulerContext {
                    pending: &pending,
                    running: &running,
                    result_by_id: &result_by_id,
                    role_map: &role_map,
                    node_map: &node_map,
                    scheduling_meta: &scheduling_meta,
                    max_workers: inputs.config.workers,
                    stop_dispatch,
                },
                session,
                ui,
                event_tx,
            )?;
        }
    }

    drop(tx);
    for (_id, handle) in handles {
        let _ = handle.await;
    }

    Ok(ScheduleOutcome { results, stopped })
}

fn stop_requested(stop_rx: Option<&watch::Receiver<bool>>) -> bool {
    stop_rx.map(|receiver| *receiver.borrow()).unwrap_or(false)
}

fn build_node_scheduling_meta(
    graph: &ExecutionGraph,
    ordered_ids: &[String],
) -> Result<HashMap<String, NodeSchedulingMeta>> {
    let node_map = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();
    let mut downstream = HashMap::<String, Vec<String>>::new();
    for node in &graph.nodes {
        for dep in &node.dependencies {
            downstream
                .entry(dep.clone())
                .or_default()
                .push(node.id.clone());
        }
    }

    let mut meta = HashMap::<String, NodeSchedulingMeta>::new();
    for (topo_index, node_id) in ordered_ids.iter().enumerate().rev() {
        let child_depth = downstream
            .get(node_id)
            .into_iter()
            .flatten()
            .filter_map(|child| meta.get(child))
            .map(|child| child.critical_depth)
            .max()
            .unwrap_or(0);
        let node = node_map
            .get(node_id.as_str())
            .copied()
            .context("构建调度元信息时节点缺失")?;
        let hint_bonus = match node.scheduler_hint {
            Some(crate::model::SchedulerHint::CriticalPath) => 2,
            Some(crate::model::SchedulerHint::Unlock) => 1,
            _ => 0,
        };
        meta.insert(
            node_id.clone(),
            NodeSchedulingMeta {
                topo_index,
                critical_depth: 1 + child_depth + hint_bonus,
                downstream_fanout: downstream
                    .get(node_id)
                    .map(|items| items.len())
                    .unwrap_or(0),
            },
        );
    }
    Ok(meta)
}

fn assess_scheduler(
    ordered_ids: &[String],
    scheduler: &SchedulerContext<'_>,
) -> SchedulerAssessment {
    let mut node_states = HashMap::new();
    let mut ready_ids = Vec::new();
    let mut blocked_dependency_count = 0usize;
    let mut blocked_role_limit_count = 0usize;
    let mut blocked_upstream_failed_count = 0usize;
    let finished_count = scheduler.result_by_id.len();

    for node_id in ordered_ids {
        let Some(node) = scheduler.node_map.get(node_id.as_str()).copied() else {
            continue;
        };
        if scheduler.result_by_id.contains_key(node_id) {
            node_states.insert(
                node_id.clone(),
                NodeRuntimeState {
                    queue_state: WorkerQueueState::Finished,
                    blocked_reason: None,
                    lane: None,
                },
            );
            continue;
        }
        if scheduler.running.contains(node_id) {
            node_states.insert(
                node_id.clone(),
                NodeRuntimeState {
                    queue_state: WorkerQueueState::Running,
                    blocked_reason: None,
                    lane: None,
                },
            );
            continue;
        }
        if !scheduler.pending.contains(node_id) {
            continue;
        }
        if let Some(reason) = blocked_reason_for_node(node, scheduler) {
            match reason.kind {
                BlockedReasonKind::WaitingDependencies => blocked_dependency_count += 1,
                BlockedReasonKind::RoleConcurrencyLimit => blocked_role_limit_count += 1,
                BlockedReasonKind::UpstreamFailed => blocked_upstream_failed_count += 1,
                BlockedReasonKind::WaitingReview
                | BlockedReasonKind::WaitingVerification
                | BlockedReasonKind::UserStop => {}
            }
            node_states.insert(
                node_id.clone(),
                NodeRuntimeState {
                    queue_state: WorkerQueueState::Blocked,
                    blocked_reason: Some(reason),
                    lane: None,
                },
            );
            continue;
        }
        ready_ids.push(node_id.clone());
        node_states.insert(
            node_id.clone(),
            NodeRuntimeState {
                queue_state: WorkerQueueState::Queued,
                blocked_reason: None,
                lane: None,
            },
        );
    }

    ready_ids.sort_by(|left, right| {
        let left = ReadyNode {
            id: left,
            node: scheduler
                .node_map
                .get(left.as_str())
                .copied()
                .expect("ready 节点缺少 node"),
            meta: scheduler
                .scheduling_meta
                .get(left)
                .expect("ready 节点缺少调度元信息"),
        };
        let right = ReadyNode {
            id: right,
            node: scheduler
                .node_map
                .get(right.as_str())
                .copied()
                .expect("ready 节点缺少 node"),
            meta: scheduler
                .scheduling_meta
                .get(right)
                .expect("ready 节点缺少调度元信息"),
        };
        compare_ready_nodes(left, right, scheduler)
    });

    for (index, node_id) in ready_ids.iter().enumerate() {
        if let Some(state) = node_states.get_mut(node_id) {
            state.lane = Some(index + 1);
        }
    }

    let critical_path_remaining = ordered_ids
        .iter()
        .filter(|node_id| {
            scheduler.pending.contains(*node_id) || scheduler.running.contains(*node_id)
        })
        .filter_map(|node_id| scheduler.scheduling_meta.get(node_id))
        .map(|item| item.critical_depth)
        .max()
        .unwrap_or(0);
    let ready_count = ready_ids.len();

    SchedulerAssessment {
        ready_ids,
        node_states,
        snapshot: SchedulerSnapshot {
            total_nodes: ordered_ids.len(),
            queued_count: ready_count,
            ready_count,
            running_count: scheduler.running.len(),
            blocked_dependency_count,
            blocked_role_limit_count,
            blocked_upstream_failed_count,
            finished_count,
            idle_slots: scheduler
                .max_workers
                .saturating_sub(scheduler.running.len()),
            critical_path_remaining,
        },
    }
}

fn compare_ready_nodes(
    left: ReadyNode<'_>,
    right: ReadyNode<'_>,
    scheduler: &SchedulerContext<'_>,
) -> std::cmp::Ordering {
    let left_todo_pressure = todo_pressure(
        left.node.todo_id.as_deref(),
        scheduler.pending,
        scheduler.node_map,
    );
    let right_todo_pressure = todo_pressure(
        right.node.todo_id.as_deref(),
        scheduler.pending,
        scheduler.node_map,
    );
    right
        .meta
        .critical_depth
        .cmp(&left.meta.critical_depth)
        .then_with(|| {
            right
                .meta
                .downstream_fanout
                .cmp(&left.meta.downstream_fanout)
        })
        .then_with(|| {
            usize::from(right.node.allow_code_changes)
                .cmp(&usize::from(left.node.allow_code_changes))
        })
        .then_with(|| right_todo_pressure.cmp(&left_todo_pressure))
        .then_with(|| left.meta.topo_index.cmp(&right.meta.topo_index))
        .then_with(|| left.id.cmp(right.id))
}

fn todo_pressure(
    todo_id: Option<&str>,
    pending: &HashSet<String>,
    node_map: &HashMap<&str, &ExecutionNode>,
) -> usize {
    let Some(todo_id) = todo_id else {
        return 0;
    };
    pending
        .iter()
        .filter(|candidate| {
            node_map
                .get(candidate.as_str())
                .and_then(|node| node.todo_id.as_deref())
                == Some(todo_id)
        })
        .count()
}

fn blocked_reason_for_node(
    node: &ExecutionNode,
    scheduler: &SchedulerContext<'_>,
) -> Option<BlockedReason> {
    if scheduler.stop_dispatch {
        return Some(BlockedReason {
            kind: BlockedReasonKind::UserStop,
            detail: "已收到停止信号，不再派发新节点。".to_string(),
        });
    }
    if has_failed_dependency(node, scheduler.result_by_id) {
        let failed_count = node
            .dependencies
            .iter()
            .filter(|dep| {
                scheduler
                    .result_by_id
                    .get(dep.as_str())
                    .map(|result| {
                        matches!(result.status, WorkerStatus::Failed | WorkerStatus::Skipped)
                    })
                    .unwrap_or(false)
            })
            .count();
        return Some(BlockedReason {
            kind: BlockedReasonKind::UpstreamFailed,
            detail: format!("{failed_count} 个上游节点失败，需先修复后再继续。"),
        });
    }
    if !dependencies_finished(node, scheduler.result_by_id) {
        let missing = node
            .dependencies
            .iter()
            .filter(|dep| !scheduler.result_by_id.contains_key(dep.as_str()))
            .count();
        return Some(BlockedReason {
            kind: BlockedReasonKind::WaitingDependencies,
            detail: format!("仍在等待 {missing} 个上游节点完成。"),
        });
    }
    let role = scheduler.role_map.get(node.role.as_str())?;
    if let Some(max_concurrency) = role.max_concurrency {
        let running_same_role = scheduler
            .running
            .iter()
            .filter(|id| {
                scheduler
                    .node_map
                    .get(id.as_str())
                    .map(|item| item.role == node.role)
                    .unwrap_or(false)
            })
            .count();
        if running_same_role >= max_concurrency {
            return Some(BlockedReason {
                kind: BlockedReasonKind::RoleConcurrencyLimit,
                detail: format!(
                    "角色 {} 已达到并发上限 {max_concurrency}，当前等待空闲槽位。",
                    role.title
                ),
            });
        }
    }
    if !scheduler.pending.contains(&node.id) {
        return Some(BlockedReason {
            kind: BlockedReasonKind::WaitingReview,
            detail: "当前节点暂未处于可调度队列。".to_string(),
        });
    }
    None
}

fn sync_brain_runtime(
    brain: &mut BrainRuntime,
    graph: &ExecutionGraph,
    ordered_ids: &[String],
    scheduler: &SchedulerContext<'_>,
    session: &mut SessionContext,
    ui: &mut UiController,
    event_tx: Option<&UnboundedSender<RuntimeEvent>>,
) -> Result<()> {
    let assessment = assess_scheduler(ordered_ids, scheduler);
    if brain.last_snapshot.as_ref() != Some(&assessment.snapshot) {
        record_event(
            session,
            ui,
            event_tx,
            RuntimeEvent::SchedulerSnapshotUpdated {
                snapshot: Box::new(assessment.snapshot.clone()),
            },
        )?;
        brain.last_snapshot = Some(assessment.snapshot.clone());
    }

    for node in &graph.nodes {
        let Some(state) = assessment.node_states.get(&node.id).cloned() else {
            continue;
        };
        let previous = brain.last_node_states.get(&node.id).cloned();
        if previous.as_ref() == Some(&state) {
            continue;
        }
        match state.queue_state {
            WorkerQueueState::Queued | WorkerQueueState::Ready => {
                let event = if previous
                    .as_ref()
                    .is_some_and(|item| item.queue_state == WorkerQueueState::Blocked)
                {
                    RuntimeEvent::WorkerRequeued {
                        agent_id: node.id.clone(),
                        reason: "Brain 重新评估后已回到可派发队列。".to_string(),
                    }
                } else {
                    RuntimeEvent::WorkerQueued {
                        agent_id: node.id.clone(),
                        role: node.role.clone(),
                        title: node.title.clone(),
                        todo_id: node.todo_id.clone(),
                        lane: state.lane.unwrap_or(0),
                    }
                };
                record_event(session, ui, event_tx, event)?;
            }
            WorkerQueueState::Blocked => {
                if let Some(reason) = state.blocked_reason.clone() {
                    record_event(
                        session,
                        ui,
                        event_tx,
                        RuntimeEvent::WorkerBlocked {
                            agent_id: node.id.clone(),
                            role: node.role.clone(),
                            title: node.title.clone(),
                            todo_id: node.todo_id.clone(),
                            reason,
                        },
                    )?;
                }
            }
            WorkerQueueState::Running | WorkerQueueState::Finished => {}
        }
    }

    brain.last_node_states = assessment.node_states;
    Ok(())
}

fn update_brain_thought(
    brain: &mut BrainRuntime,
    thought: &str,
    session: &mut SessionContext,
    ui: &mut UiController,
    event_tx: Option<&UnboundedSender<RuntimeEvent>>,
) -> Result<()> {
    if brain.state.latest_thought == thought {
        return Ok(());
    }
    brain.state.latest_thought = thought.to_string();
    record_event(
        session,
        ui,
        event_tx,
        RuntimeEvent::BrainThought {
            thought: thought.to_string(),
        },
    )
}

fn record_brain_decision(
    brain: &mut BrainRuntime,
    decision: BrainDecision,
    session: &mut SessionContext,
    ui: &mut UiController,
    event_tx: Option<&UnboundedSender<RuntimeEvent>>,
) -> Result<()> {
    brain.state.current_focus = decision.summary.clone();
    brain.state.latest_decision = decision.summary.clone();
    brain.state.latest_thought = decision.rationale.clone();
    brain.state.risk_level = decision.risk_level;
    brain.state.needs_user_attention = decision.action == BrainActionKind::EscalateToUser
        || decision.risk_level == BrainRiskLevel::High;
    record_event(
        session,
        ui,
        event_tx,
        RuntimeEvent::BrainDecisionMade {
            decision: Box::new(decision),
        },
    )
}

fn build_dispatch_rationale(
    node: &ExecutionNode,
    scheduling_meta: &HashMap<String, NodeSchedulingMeta>,
    pending: &HashSet<String>,
    node_map: &HashMap<&str, &ExecutionNode>,
) -> String {
    let meta = scheduling_meta.get(&node.id);
    format!(
        "关键路径剩余 {}，可解锁下游 {}，当前 todo 压力 {}，调度 hint 为 {}，因此优先派发。",
        meta.map(|item| item.critical_depth).unwrap_or(0),
        meta.map(|item| item.downstream_fanout).unwrap_or(0),
        todo_pressure(node.todo_id.as_deref(), pending, node_map),
        node.scheduler_hint
            .map(|hint| hint.label())
            .unwrap_or("未显式指定")
    )
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
    session: &mut SessionContext,
    ui: &mut UiController,
    event_tx: Option<&UnboundedSender<RuntimeEvent>>,
    event: RuntimeEvent,
) -> Result<()> {
    session.append_timeline(&event)?;
    if let Some(tx) = event_tx {
        let _ = tx.send(event.clone());
    }
    ui.apply(&event)
}

fn update_todo_status(
    session: &mut SessionContext,
    ui: &mut UiController,
    event_tx: Option<&UnboundedSender<RuntimeEvent>>,
    todo_id: &str,
    status: TodoStatus,
    message: impl Into<String>,
    commit_hash: Option<String>,
) -> Result<()> {
    if let Some(updated) =
        session.update_todo_status(todo_id, status, message.into(), commit_hash)?
    {
        record_event(
            session,
            ui,
            event_tx,
            RuntimeEvent::TodoStateChanged {
                todo_id: updated.todo_id,
                title: updated.title,
                status: updated.status,
                message: updated
                    .last_message
                    .unwrap_or_else(|| "状态已更新".to_string()),
                commit_hash: updated.commit_hash,
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SchedulerContext, assess_scheduler, build_node_scheduling_meta};
    use crate::model::{
        ExecutionGraph, ExecutionNode, RoleConfig, SchedulerHint, ScopeDrift, WorkerResult,
        WorkerStatus,
    };
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    fn role(key: &str, title: &str, can_edit: bool, max_concurrency: Option<usize>) -> RoleConfig {
        RoleConfig {
            key: key.to_string(),
            title: title.to_string(),
            mission: "推进任务".to_string(),
            skills: Vec::new(),
            working_style: "直接推进".to_string(),
            can_edit,
            max_concurrency,
            dependency_policy: None,
            prompt_preamble: None,
        }
    }

    fn node(id: &str, role: &str, dependencies: &[&str]) -> ExecutionNode {
        ExecutionNode {
            id: id.to_string(),
            title: id.to_string(),
            todo_id: Some(format!("todo-{id}")),
            role: role.to_string(),
            objective: "推进".to_string(),
            deliverables: Vec::new(),
            dependencies: dependencies.iter().map(|item| item.to_string()).collect(),
            prompt_focus: "推进".to_string(),
            input_artifacts: Vec::new(),
            output_artifacts: Vec::new(),
            completion_criteria: Vec::new(),
            allow_code_changes: true,
            expected_artifacts: Vec::new(),
            required_verifications: Vec::new(),
            scope_guard_ref: None,
            scheduler_hint: Some(SchedulerHint::CriticalPath),
            acceptable_drift: ScopeDrift::Minor,
        }
    }

    fn succeeded_result(agent_id: &str) -> WorkerResult {
        WorkerResult {
            agent_id: agent_id.to_string(),
            role: "implementer".to_string(),
            task_title: agent_id.to_string(),
            status: WorkerStatus::Succeeded,
            exit_code: Some(0),
            attempts: 1,
            diagnostic_summary: None,
            final_message: String::new(),
            summary: None,
            changed_files: Vec::new(),
            worktree_path: PathBuf::from("/tmp"),
            prompt_path: PathBuf::from("/tmp/prompt.md"),
            stdout_path: PathBuf::from("/tmp/stdout.log"),
            stderr_path: PathBuf::from("/tmp/stderr.log"),
            events_path: PathBuf::from("/tmp/events.jsonl"),
            final_output_path: PathBuf::from("/tmp/final.md"),
            diff_path: None,
            git_status_path: None,
            handoff_path: None,
            handoff: None,
            error: None,
        }
    }

    fn failed_result(agent_id: &str) -> WorkerResult {
        WorkerResult {
            status: WorkerStatus::Failed,
            error: Some("boom".to_string()),
            ..succeeded_result(agent_id)
        }
    }

    #[test]
    fn scheduler_prefers_longer_critical_path() {
        let graph = ExecutionGraph {
            summary: "x".to_string(),
            strategy: "x".to_string(),
            nodes: vec![
                node("a", "implementer", &[]),
                node("b", "implementer", &["a"]),
                node("c", "implementer", &["b"]),
                node("d", "implementer", &[]),
            ],
            used_fallback: false,
            planning_notes: Vec::new(),
        };
        let ordered = graph.topological_order().unwrap();
        let meta = build_node_scheduling_meta(&graph, &ordered).unwrap();
        let node_map = graph
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node))
            .collect::<HashMap<_, _>>();
        let roles = [role("implementer", "实现", true, None)];
        let role_map = roles
            .iter()
            .map(|role| (role.key.as_str(), role))
            .collect::<HashMap<_, _>>();
        let pending = ordered.iter().cloned().collect::<HashSet<_>>();

        let assessment = assess_scheduler(
            &ordered,
            &SchedulerContext {
                pending: &pending,
                running: &HashSet::new(),
                result_by_id: &HashMap::new(),
                role_map: &role_map,
                node_map: &node_map,
                scheduling_meta: &meta,
                max_workers: 4,
                stop_dispatch: false,
            },
        );
        assert_eq!(assessment.ready_ids.first().map(String::as_str), Some("a"));
    }

    #[test]
    fn scheduler_backfills_other_ready_nodes_when_role_is_limited() {
        let graph = ExecutionGraph {
            summary: "x".to_string(),
            strategy: "x".to_string(),
            nodes: vec![
                node("impl-1", "implementer", &[]),
                node("impl-2", "implementer", &[]),
                node("test-1", "tester", &[]),
            ],
            used_fallback: false,
            planning_notes: Vec::new(),
        };
        let ordered = graph.topological_order().unwrap();
        let meta = build_node_scheduling_meta(&graph, &ordered).unwrap();
        let node_map = graph
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node))
            .collect::<HashMap<_, _>>();
        let roles = [
            role("implementer", "实现", true, Some(1)),
            role("tester", "测试", false, Some(1)),
        ];
        let role_map = roles
            .iter()
            .map(|role| (role.key.as_str(), role))
            .collect::<HashMap<_, _>>();
        let pending = ["impl-2".to_string(), "test-1".to_string()]
            .into_iter()
            .collect::<HashSet<_>>();
        let running = ["impl-1".to_string()].into_iter().collect::<HashSet<_>>();

        let empty_results = HashMap::new();
        let assessment = assess_scheduler(
            &ordered,
            &SchedulerContext {
                pending: &pending,
                running: &running,
                result_by_id: &empty_results,
                role_map: &role_map,
                node_map: &node_map,
                scheduling_meta: &meta,
                max_workers: 3,
                stop_dispatch: false,
            },
        );
        assert_eq!(
            assessment.ready_ids.first().map(String::as_str),
            Some("test-1")
        );
        assert_eq!(
            assessment
                .node_states
                .get("impl-2")
                .and_then(|state| state.blocked_reason.as_ref())
                .map(|reason| reason.kind.label()),
            Some("角色并发上限")
        );
    }

    #[test]
    fn scheduler_does_not_block_independent_branch_after_local_failure() {
        let graph = ExecutionGraph {
            summary: "x".to_string(),
            strategy: "x".to_string(),
            nodes: vec![
                node("a", "implementer", &[]),
                node("repair-a", "implementer", &["a"]),
                node("b", "implementer", &[]),
            ],
            used_fallback: false,
            planning_notes: Vec::new(),
        };
        let ordered = graph.topological_order().unwrap();
        let meta = build_node_scheduling_meta(&graph, &ordered).unwrap();
        let node_map = graph
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node))
            .collect::<HashMap<_, _>>();
        let roles = [role("implementer", "实现", true, None)];
        let role_map = roles
            .iter()
            .map(|role| (role.key.as_str(), role))
            .collect::<HashMap<_, _>>();
        let pending = ["repair-a".to_string(), "b".to_string()]
            .into_iter()
            .collect::<HashSet<_>>();
        let result_by_id = [("a".to_string(), failed_result("a"))]
            .into_iter()
            .collect::<HashMap<_, _>>();

        let assessment = assess_scheduler(
            &ordered,
            &SchedulerContext {
                pending: &pending,
                running: &HashSet::new(),
                result_by_id: &result_by_id,
                role_map: &role_map,
                node_map: &node_map,
                scheduling_meta: &meta,
                max_workers: 2,
                stop_dispatch: false,
            },
        );
        assert_eq!(assessment.ready_ids, vec!["b".to_string()]);
        assert_eq!(
            assessment
                .node_states
                .get("repair-a")
                .and_then(|state| state.blocked_reason.as_ref())
                .map(|reason| reason.kind.label()),
            Some("上游失败")
        );
    }
}
