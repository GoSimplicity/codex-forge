use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

use crate::model::{
    ApplyDecision, ApplyMode, ApplyOperation, ApplyPlan, ApplyResult, ApplyStatus,
    ChangeTrustReport, ExecutionContract, ExecutionGraph, ReviewGateReport, ScopeDrift,
    TodoCommitRecord, TodoStateRecord, TodoStatus, TrustLevel, VerificationCommandResult,
    WorkerResult, WorkerStatus,
};
use crate::verify::{build_verification_report, run_stage_verification, verification_dir};
use crate::worktree::{
    WorktreeManager, apply_patch_file, apply_patch_file_for_paths, git_commit_paths,
    git_diff_binary, git_is_clean,
};

pub struct ApplyExecutionContext<'a> {
    pub session_dir: &'a Path,
    pub repo_root: &'a Path,
    pub worker_results: &'a [WorkerResult],
    pub manager: &'a WorktreeManager,
    pub verification_commands: &'a [String],
    pub apply_result_path: &'a Path,
    pub verification_report_path: &'a Path,
    pub change_trust_report_path: &'a Path,
    pub execution_contract: &'a ExecutionContract,
    pub todo_states: &'a [TodoStateRecord],
}

#[derive(Debug)]
struct TodoBatch {
    todo_id: String,
    title: String,
    changed_files: Vec<String>,
    operation_indexes: Vec<usize>,
}

#[derive(Debug)]
struct TodoCommitExecution {
    records: Vec<TodoCommitRecord>,
    verification_results: Vec<VerificationCommandResult>,
    final_ok: bool,
    error: Option<String>,
}

pub async fn build_apply_plan(
    mode: ApplyMode,
    graph: &ExecutionGraph,
    ordered_worker_ids: &[String],
    worker_results: &[WorkerResult],
    apply_plan_path: &Path,
) -> Result<ApplyPlan> {
    if let Some(parent) = apply_plan_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建 apply 目录失败：{}", parent.display()))?;
    }
    let result_map = worker_results
        .iter()
        .map(|item| (item.agent_id.as_str(), item))
        .collect::<HashMap<_, _>>();
    let node_map = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();

    let mut operations = Vec::new();
    for (index, worker_id) in ordered_worker_ids.iter().enumerate() {
        if let Some(result) = result_map.get(worker_id.as_str())
            && let Some(node) = node_map.get(worker_id.as_str())
            && result.status == WorkerStatus::Succeeded
            && node.allow_code_changes
            && let Some(diff_path) = &result.diff_path
            && !result.changed_files.is_empty()
            && diff_path.exists()
        {
            operations.push(ApplyOperation {
                agent_id: result.agent_id.clone(),
                todo_id: node.todo_id.clone(),
                patch_path: diff_path.clone(),
                order: index + 1,
                touched_files: result.changed_files.clone(),
                applied: false,
                note: None,
            });
        }
    }

    let plan = ApplyPlan {
        mode,
        operations,
        degrade_to_bundle: matches!(mode, ApplyMode::Bundle),
    };
    fs::write(
        apply_plan_path,
        serde_json::to_vec_pretty(&plan).context("序列化 apply plan 失败")?,
    )
    .with_context(|| format!("写入 apply plan 失败：{}", apply_plan_path.display()))?;
    Ok(plan)
}

pub async fn execute_apply_plan(
    plan: ApplyPlan,
    context: ApplyExecutionContext<'_>,
) -> Result<(
    ApplyResult,
    crate::model::VerificationReport,
    crate::model::ChangeTrustReport,
)> {
    let integration_dir = context.session_dir.join("integration");
    fs::create_dir_all(&integration_dir)
        .with_context(|| format!("创建 integration 目录失败：{}", integration_dir.display()))?;
    let log_path = integration_dir.join("apply.log");
    let bundle_dir = integration_dir.join("bundle");
    let final_patch_path = integration_dir.join("final.patch");
    let integration_worktree = context.manager.create_named("integration", "HEAD").await?;
    let reviewer_gate = latest_reviewer_gate(context.worker_results);
    let review_report = latest_reviewer_report(context.worker_results);
    let trust_report = build_change_trust_report(
        context.execution_contract,
        &plan,
        context.worker_results,
        reviewer_gate,
    );

    fs::write(
        context.change_trust_report_path,
        serde_json::to_vec_pretty(&trust_report).context("序列化 change trust report 失败")?,
    )
    .with_context(|| {
        format!(
            "写入 change trust report 失败：{}",
            context.change_trust_report_path.display()
        )
    })?;

    let mut apply_result = ApplyResult {
        mode: plan.mode,
        status: ApplyStatus::Skipped,
        integration_worktree: Some(integration_worktree.clone()),
        applied_workers: Vec::new(),
        rejected_workers: Vec::new(),
        conflicts: trust_report.blocking_reasons.clone(),
        synced_to_target: false,
        bundle_dir: None,
        final_patch_path: None,
        log_path: log_path.clone(),
        review_gate: reviewer_gate,
        trust_level: trust_report.trust_level,
        scope_drift: trust_report.scope_drift,
        accepted_files: trust_report.accepted_files.clone(),
        manual_review_files: trust_report.manual_review_files.clone(),
        rejected_files: trust_report.rejected_files.clone(),
        out_of_scope_files: trust_report.out_of_scope_files.clone(),
        todo_commits: Vec::new(),
        review_report,
    };

    match plan.mode {
        ApplyMode::None => {
            append_log(
                &log_path,
                "apply_mode=none，只生成决策与审阅清单，不落地 patch。",
            )?;
        }
        ApplyMode::Bundle => {
            materialize_bundle(&bundle_dir, context.worker_results).await?;
            append_log(&log_path, "apply_mode=bundle，直接输出 bundle。")?;
            apply_result.status = ApplyStatus::Bundled;
            apply_result.bundle_dir = Some(bundle_dir);
        }
        ApplyMode::AutoSafe => {
            if let Some(ApplyDecision::Block) = reviewer_gate {
                append_log(&log_path, "reviewer 明确阻止自动应用，降级为 bundle。")?;
                materialize_bundle(&bundle_dir, context.worker_results).await?;
                apply_result.status = ApplyStatus::Bundled;
                apply_result.bundle_dir = Some(bundle_dir.clone());
            } else if !trust_report.safe_to_auto_apply {
                append_log(&log_path, "可信度评估认为不适合自动应用，降级为 bundle。")?;
                materialize_bundle(&bundle_dir, context.worker_results).await?;
                apply_result.status = ApplyStatus::Bundled;
                apply_result.bundle_dir = Some(bundle_dir.clone());
            }

            for operation in &plan.operations {
                if apply_result.status == ApplyStatus::Bundled {
                    break;
                }

                let accepted_for_operation = operation
                    .touched_files
                    .iter()
                    .filter(|file| trust_report.accepted_files.contains(*file))
                    .cloned()
                    .collect::<Vec<_>>();

                if accepted_for_operation.is_empty() {
                    apply_result
                        .rejected_workers
                        .push(operation.agent_id.clone());
                    append_log(
                        &log_path,
                        &format!("{} 没有可自动接收的文件，跳过应用。", operation.agent_id),
                    )?;
                    continue;
                }

                let apply_res = if accepted_for_operation.len() == operation.touched_files.len() {
                    apply_patch_file(&integration_worktree, &operation.patch_path).await
                } else {
                    append_log(
                        &log_path,
                        &format!(
                            "{} 进入部分接收，仅应用：{}",
                            operation.agent_id,
                            accepted_for_operation.join("、")
                        ),
                    )?;
                    apply_patch_file_for_paths(
                        &integration_worktree,
                        &operation.patch_path,
                        &accepted_for_operation,
                    )
                    .await
                };

                match apply_res {
                    Ok(()) => {
                        apply_result
                            .applied_workers
                            .push(operation.agent_id.clone());
                        append_log(
                            &log_path,
                            &format!(
                                "已应用 {}，接收文件：{}",
                                operation.agent_id,
                                accepted_for_operation.join("、")
                            ),
                        )?;
                    }
                    Err(error) => {
                        apply_result
                            .rejected_workers
                            .push(operation.agent_id.clone());
                        apply_result
                            .conflicts
                            .push(format!("{} patch 应用失败：{}", operation.agent_id, error));
                        append_log(
                            &log_path,
                            &format!("{} 应用失败，降级为 bundle：{error}", operation.agent_id),
                        )?;
                        materialize_bundle(&bundle_dir, context.worker_results).await?;
                        apply_result.status = ApplyStatus::Bundled;
                        apply_result.bundle_dir = Some(bundle_dir.clone());
                        break;
                    }
                }
            }

            if apply_result.status != ApplyStatus::Bundled {
                let verification_root = verification_dir(context.session_dir);
                let integration_results = run_stage_verification(
                    "integration",
                    context.verification_commands,
                    &integration_worktree,
                    &verification_root,
                )
                .await?;
                let integration_has_failed = integration_results.iter().any(|item| !item.passed());
                let mut final_results = Vec::new();

                if !integration_has_failed {
                    git_diff_binary(&integration_worktree, &final_patch_path).await?;
                    apply_result.final_patch_path = Some(final_patch_path.clone());

                    if git_is_clean(context.repo_root).await? {
                        let commit_execution = apply_and_commit_todos(
                            context.repo_root,
                            &plan,
                            &trust_report.accepted_files,
                            context.todo_states,
                            context.verification_commands,
                            &verification_root,
                        )
                        .await?;
                        apply_result.todo_commits = commit_execution.records;
                        final_results = commit_execution.verification_results;
                        if let Some(error) = commit_execution.error {
                            apply_result.status = ApplyStatus::SyncFailed;
                            apply_result.conflicts.push(error);
                        } else {
                            apply_result.synced_to_target = commit_execution.final_ok;
                            apply_result.status = if commit_execution.final_ok {
                                ApplyStatus::Applied
                            } else {
                                ApplyStatus::VerificationFailed
                            };
                        }
                    } else {
                        apply_result.status = ApplyStatus::SyncFailed;
                        apply_result
                            .conflicts
                            .push("目标工作区不干净，拒绝自动同步".to_string());
                    }
                } else {
                    apply_result.status = ApplyStatus::VerificationFailed;
                }

                let report = build_verification_report(
                    context.worker_results,
                    integration_results,
                    final_results,
                );
                persist_apply_result(&apply_result, context.apply_result_path).await?;
                persist_verification_report(&report, context.verification_report_path).await?;
                let _ = context.manager.cleanup(&integration_worktree).await;
                return Ok((apply_result, report, trust_report));
            }
        }
    }

    let report = build_verification_report(context.worker_results, Vec::new(), Vec::new());
    persist_apply_result(&apply_result, context.apply_result_path).await?;
    persist_verification_report(&report, context.verification_report_path).await?;
    let _ = context.manager.cleanup(&integration_worktree).await;
    Ok((apply_result, report, trust_report))
}

pub async fn deliver_accepted_files(
    plan: &ApplyPlan,
    apply_result: &ApplyResult,
    target_dir: &Path,
) -> Result<Vec<String>> {
    if apply_result.accepted_files.is_empty() {
        anyhow::bail!("当前会话没有可安全交付的 accepted_files");
    }
    if !git_is_clean(target_dir).await? {
        anyhow::bail!("目标工作区存在未提交改动，拒绝执行安全交付");
    }

    let delivered =
        deliver_selected_files_from_plan(plan, target_dir, &apply_result.accepted_files).await?;
    if delivered.is_empty() {
        anyhow::bail!("accepted_files 为空或没有对应可应用 patch");
    }
    Ok(delivered)
}

pub async fn deliver_selected_files_from_plan(
    plan: &ApplyPlan,
    target_dir: &Path,
    selected_files: &[String],
) -> Result<Vec<String>> {
    let mut delivered = Vec::new();
    for operation in &plan.operations {
        let files_for_operation = operation
            .touched_files
            .iter()
            .filter(|file| selected_files.contains(*file))
            .cloned()
            .collect::<Vec<_>>();
        if files_for_operation.is_empty() {
            continue;
        }

        if files_for_operation.len() == operation.touched_files.len() {
            apply_patch_file(target_dir, &operation.patch_path).await?;
        } else {
            apply_patch_file_for_paths(target_dir, &operation.patch_path, &files_for_operation)
                .await?;
        }
        delivered.extend(files_for_operation);
    }

    delivered.sort();
    delivered.dedup();
    Ok(delivered)
}

async fn apply_and_commit_todos(
    repo_root: &Path,
    plan: &ApplyPlan,
    accepted_files: &[String],
    todo_states: &[TodoStateRecord],
    verification_commands: &[String],
    verification_root: &Path,
) -> Result<TodoCommitExecution> {
    let batches = build_todo_batches(plan, accepted_files, todo_states);
    let mut records = Vec::new();
    let mut verification_results = Vec::new();

    for batch in batches {
        for index in &batch.operation_indexes {
            let operation = &plan.operations[*index];
            let accepted_for_operation = operation
                .touched_files
                .iter()
                .filter(|file| accepted_files.contains(*file))
                .cloned()
                .collect::<Vec<_>>();
            let apply_res = if accepted_for_operation.len() == operation.touched_files.len() {
                apply_patch_file(repo_root, &operation.patch_path).await
            } else {
                apply_patch_file_for_paths(
                    repo_root,
                    &operation.patch_path,
                    &accepted_for_operation,
                )
                .await
            };
            if let Err(error) = apply_res {
                records.push(TodoCommitRecord {
                    todo_id: batch.todo_id.clone(),
                    title: batch.title.clone(),
                    status: TodoStatus::Blocked,
                    changed_files: batch.changed_files.clone(),
                    commit_hash: None,
                    message: format!("应用 todo patch 失败：{error}"),
                });
                return Ok(TodoCommitExecution {
                    records,
                    verification_results,
                    final_ok: false,
                    error: Some(format!("todo `{}` 应用失败：{error}", batch.todo_id)),
                });
            }
        }

        let stage = format!("todo-{}", batch.todo_id);
        let todo_verify =
            run_stage_verification(&stage, verification_commands, repo_root, verification_root)
                .await?;
        let verified = todo_verify.iter().all(|item| item.passed());
        verification_results.extend(todo_verify);
        if !verified {
            records.push(TodoCommitRecord {
                todo_id: batch.todo_id.clone(),
                title: batch.title.clone(),
                status: TodoStatus::Failed,
                changed_files: batch.changed_files.clone(),
                commit_hash: None,
                message: "todo 验证失败，已停止后续提交".to_string(),
            });
            return Ok(TodoCommitExecution {
                records,
                verification_results,
                final_ok: false,
                error: None,
            });
        }

        let commit_message = build_todo_commit_message(&batch);
        match git_commit_paths(
            repo_root,
            &commit_message,
            &batch.changed_files,
            batch.changed_files.is_empty(),
        )
        .await
        {
            Ok(commit_hash) => records.push(TodoCommitRecord {
                todo_id: batch.todo_id.clone(),
                title: batch.title.clone(),
                status: TodoStatus::Committed,
                changed_files: batch.changed_files.clone(),
                commit_hash: Some(commit_hash),
                message: commit_message,
            }),
            Err(error) => {
                records.push(TodoCommitRecord {
                    todo_id: batch.todo_id.clone(),
                    title: batch.title.clone(),
                    status: TodoStatus::Blocked,
                    changed_files: batch.changed_files.clone(),
                    commit_hash: None,
                    message: format!("git commit 失败：{error}"),
                });
                return Ok(TodoCommitExecution {
                    records,
                    verification_results,
                    final_ok: false,
                    error: Some(format!("todo `{}` 提交失败：{error}", batch.todo_id)),
                });
            }
        }
    }

    let final_results =
        run_stage_verification("final", verification_commands, repo_root, verification_root)
            .await?;
    let final_ok = final_results.iter().all(|item| item.passed());
    verification_results.extend(final_results);
    Ok(TodoCommitExecution {
        records,
        verification_results,
        final_ok,
        error: None,
    })
}

fn build_todo_batches(
    plan: &ApplyPlan,
    accepted_files: &[String],
    todo_states: &[TodoStateRecord],
) -> Vec<TodoBatch> {
    let mut by_todo = HashMap::<String, TodoBatch>::new();
    for todo in todo_states {
        by_todo.insert(
            todo.todo_id.clone(),
            TodoBatch {
                todo_id: todo.todo_id.clone(),
                title: todo.title.clone(),
                changed_files: Vec::new(),
                operation_indexes: Vec::new(),
            },
        );
    }

    for (index, operation) in plan.operations.iter().enumerate() {
        let Some(todo_id) = operation.todo_id.as_ref() else {
            continue;
        };
        let accepted_for_operation = operation
            .touched_files
            .iter()
            .filter(|file| accepted_files.contains(*file))
            .cloned()
            .collect::<Vec<_>>();
        let batch = by_todo.entry(todo_id.clone()).or_insert_with(|| TodoBatch {
            todo_id: todo_id.clone(),
            title: todo_id.clone(),
            changed_files: Vec::new(),
            operation_indexes: Vec::new(),
        });
        if !accepted_for_operation.is_empty() {
            batch.operation_indexes.push(index);
            batch.changed_files.extend(accepted_for_operation);
        }
    }

    let mut ordered = todo_states
        .iter()
        .map(|todo| {
            let mut batch = by_todo.remove(&todo.todo_id).unwrap_or(TodoBatch {
                todo_id: todo.todo_id.clone(),
                title: todo.title.clone(),
                changed_files: Vec::new(),
                operation_indexes: Vec::new(),
            });
            batch.changed_files.sort();
            batch.changed_files.dedup();
            batch
        })
        .collect::<Vec<_>>();

    if ordered.is_empty() {
        for (todo_id, mut batch) in by_todo {
            batch.changed_files.sort();
            batch.changed_files.dedup();
            ordered.push(TodoBatch { todo_id, ..batch });
        }
        ordered.sort_by(|left, right| left.todo_id.cmp(&right.todo_id));
    }
    ordered
}

fn build_todo_commit_message(batch: &TodoBatch) -> String {
    let lowercase = batch.title.to_lowercase();
    let prefix = if lowercase.contains("fix")
        || lowercase.contains("bug")
        || batch.title.contains("修复")
        || batch.title.contains("修正")
    {
        "fix"
    } else if batch.changed_files.is_empty() {
        "chore"
    } else {
        "feat"
    };
    format!("{prefix}({}): {}", batch.todo_id, batch.title)
}

fn build_change_trust_report(
    contract: &ExecutionContract,
    plan: &ApplyPlan,
    worker_results: &[WorkerResult],
    reviewer_gate: Option<ApplyDecision>,
) -> ChangeTrustReport {
    let mut file_workers = HashMap::<String, Vec<String>>::new();
    let result_map = worker_results
        .iter()
        .map(|item| (item.agent_id.as_str(), item))
        .collect::<HashMap<_, _>>();

    for operation in &plan.operations {
        for file in &operation.touched_files {
            file_workers
                .entry(file.clone())
                .or_default()
                .push(operation.agent_id.clone());
        }
    }

    let mut rejected_files = Vec::new();
    let mut manual_review_files = Vec::new();
    let mut accepted_files = Vec::new();
    let mut out_of_scope_files = Vec::new();
    let mut blocking_reasons = Vec::new();

    for operation in &plan.operations {
        let worker = result_map.get(operation.agent_id.as_str());
        let claimed_files = worker
            .and_then(|item| item.handoff.as_ref())
            .map(|handoff| {
                if handoff.contract_scope_claim.is_empty() {
                    handoff.touched_files.clone()
                } else {
                    handoff.contract_scope_claim.clone()
                }
            })
            .unwrap_or_default();
        let node_allowed = contract
            .node_contract(&operation.agent_id)
            .map(|item| item.allowed_paths.clone())
            .filter(|items| !items.is_empty())
            .unwrap_or_else(|| contract.allowed_paths.clone());
        let node_forbidden = contract
            .node_contract(&operation.agent_id)
            .map(|item| item.forbidden_paths.clone())
            .filter(|items| !items.is_empty())
            .unwrap_or_else(|| contract.forbidden_paths.clone());

        for file in &operation.touched_files {
            let forbidden = matches_any_pattern(file, &node_forbidden);
            let allowed = node_allowed.is_empty() || matches_any_pattern(file, &node_allowed);
            let is_claimed = claimed_files.is_empty() || claimed_files.contains(file);
            let is_conflicted = file_workers
                .get(file)
                .is_some_and(|workers| workers.len() > 1);

            if forbidden || !allowed {
                rejected_files.push(file.clone());
                out_of_scope_files.push(file.clone());
                continue;
            }

            if is_conflicted || !is_claimed {
                manual_review_files.push(file.clone());
                continue;
            }

            accepted_files.push(file.clone());
        }
    }

    accepted_files.sort();
    manual_review_files.sort();
    rejected_files.sort();
    out_of_scope_files.sort();
    accepted_files.dedup();
    manual_review_files.dedup();
    rejected_files.dedup();
    out_of_scope_files.dedup();

    if plan.operations.is_empty() {
        blocking_reasons.push("没有可自动应用的成功 patch，自动应用直接降级。".to_string());
    }
    if !out_of_scope_files.is_empty() {
        blocking_reasons.push(format!(
            "发现超出契约范围的文件：{}",
            out_of_scope_files.join("、")
        ));
    }
    if !manual_review_files.is_empty() {
        blocking_reasons.push(format!(
            "以下文件存在冲突或声明不一致，需要人工复核：{}",
            manual_review_files.join("、")
        ));
    }
    if matches!(reviewer_gate, Some(ApplyDecision::Block)) {
        blocking_reasons.push("reviewer 明确阻止自动应用。".to_string());
    }

    let scope_drift = if !out_of_scope_files.is_empty() {
        ScopeDrift::Major
    } else if !manual_review_files.is_empty() {
        ScopeDrift::Minor
    } else {
        ScopeDrift::None
    };
    let trust_level = if plan.operations.is_empty()
        || accepted_files.is_empty()
            && (!manual_review_files.is_empty() || !rejected_files.is_empty())
        || matches!(reviewer_gate, Some(ApplyDecision::Block))
        || scope_drift == ScopeDrift::Major
    {
        TrustLevel::Low
    } else if scope_drift == ScopeDrift::Minor
        || matches!(reviewer_gate, Some(ApplyDecision::AllowPartial))
    {
        TrustLevel::Medium
    } else {
        TrustLevel::High
    };
    let safe_to_auto_apply = match trust_level {
        TrustLevel::High => true,
        TrustLevel::Medium => contract.drift_policy.allow_partial_apply,
        TrustLevel::Low => false,
    } && !matches!(reviewer_gate, Some(ApplyDecision::Block));

    ChangeTrustReport {
        trust_level,
        scope_drift,
        safe_to_auto_apply,
        accepted_files,
        manual_review_files,
        rejected_files,
        out_of_scope_files,
        blocking_reasons,
    }
}

async fn materialize_bundle(bundle_dir: &Path, worker_results: &[WorkerResult]) -> Result<()> {
    fs::create_dir_all(bundle_dir)
        .with_context(|| format!("创建 bundle 目录失败：{}", bundle_dir.display()))?;
    for result in worker_results {
        if let Some(diff_path) = &result.diff_path
            && diff_path.exists()
        {
            let target = bundle_dir.join(format!("{}-changes.patch", result.agent_id));
            fs::copy(diff_path, &target).with_context(|| {
                format!(
                    "复制 patch 到 bundle 失败：{} -> {}",
                    diff_path.display(),
                    target.display()
                )
            })?;
        }
    }
    Ok(())
}

fn latest_reviewer_gate(worker_results: &[WorkerResult]) -> Option<ApplyDecision> {
    worker_results
        .iter()
        .filter(|result| result.role == "reviewer")
        .filter_map(|result| {
            result
                .handoff
                .as_ref()
                .and_then(|handoff| handoff.apply_decision)
        })
        .next_back()
}

fn latest_reviewer_report(worker_results: &[WorkerResult]) -> Option<ReviewGateReport> {
    worker_results
        .iter()
        .filter(|result| result.role == "reviewer")
        .filter_map(|result| {
            result.handoff.as_ref().and_then(|handoff| {
                handoff.apply_decision.map(|decision| ReviewGateReport {
                    decision,
                    blocking_findings: handoff.blocking_findings.clone(),
                    accepted_scopes: handoff.accepted_scopes.clone(),
                    rejected_scopes: handoff.rejected_scopes.clone(),
                    confidence_reasoning: handoff.confidence_reasoning.clone(),
                })
            })
        })
        .next_back()
}

fn matches_any_pattern(path: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|pattern| path_matches_pattern(path, pattern))
}

fn path_matches_pattern(path: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }
    path == pattern
}

fn append_log(log_path: &Path, message: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("打开 apply 日志失败：{}", log_path.display()))?;
    writeln!(file, "{message}").context("写入 apply 日志失败")
}

async fn persist_apply_result(result: &ApplyResult, path: &Path) -> Result<()> {
    fs::write(
        path,
        serde_json::to_vec_pretty(result).context("序列化 apply result 失败")?,
    )
    .with_context(|| format!("写入 apply result 失败：{}", path.display()))
}

async fn persist_verification_report(
    report: &crate::model::VerificationReport,
    path: &Path,
) -> Result<()> {
    fs::write(
        path,
        serde_json::to_vec_pretty(report).context("序列化 verification report 失败")?,
    )
    .with_context(|| format!("写入 verification report 失败：{}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{matches_any_pattern, path_matches_pattern};

    #[test]
    fn matches_recursive_pattern() {
        assert!(path_matches_pattern("src/a.rs", "src/**"));
        assert!(!path_matches_pattern("tests/a.rs", "src/**"));
    }

    #[test]
    fn matches_wildcard() {
        assert!(matches_any_pattern("any/file", &["*".to_string()]));
    }
}
