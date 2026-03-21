use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::model::{
    ApplyMode, ApplyResult, ArtifactEntry, ArtifactIndexEntry, ArtifactManifest, ChangeTrustReport,
    ExecutionContract, ExecutionGraph, FinalSummary, PlanTodo, RepoSnapshot, RuntimeEvent,
    RuntimeEventRecord, SessionConfig, SessionLineageEntry, SessionManifest, SessionStatus,
    TimelineEventSummary, TodoStateRecord, TodoStatus, VerificationReport, WorkerResult,
};

#[derive(Debug, Clone)]
pub struct SessionContext {
    pub manifest: SessionManifest,
    manifest_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupScope {
    SessionCascade,
    AllArtifacts,
}

#[derive(Debug, Clone)]
pub struct CleanupReport {
    pub scope: CleanupScope,
    pub repo_root: PathBuf,
    pub forge_dir: PathBuf,
    pub removed_sessions: Vec<String>,
    pub had_artifacts: bool,
}

#[derive(Debug, Clone)]
pub struct ResetReport {
    pub removed_sessions: Vec<String>,
    pub reset_commits: Vec<String>,
    pub reset_to: Option<String>,
}

impl SessionContext {
    pub fn init(config: &SessionConfig, repo_snapshot: RepoSnapshot) -> Result<Self> {
        let session_id = format!(
            "{}-{}",
            Utc::now().format("%Y%m%d-%H%M%S"),
            std::process::id()
        );
        let base_dir = repo_snapshot
            .repo_root
            .join(".codex-forge")
            .join("sessions");
        let session_dir = base_dir.join(&session_id);
        fs::create_dir_all(&session_dir)
            .with_context(|| format!("创建 session 目录失败：{}", session_dir.display()))?;

        let created_at = Utc::now();
        let iteration_index = config
            .continuation
            .as_ref()
            .map(|item| item.iteration_index.max(1))
            .unwrap_or(1);
        let root_session_id = config
            .continuation
            .as_ref()
            .map(|item| item.root_session_id.clone())
            .unwrap_or_else(|| session_id.clone());
        let lineage = if let Some(continuation) = &config.continuation {
            let mut items = if continuation.parent_lineage.is_empty() {
                vec![SessionLineageEntry {
                    session_id: continuation.parent_session_id.clone(),
                    iteration_index: iteration_index.saturating_sub(1).max(1),
                    continuation_kind: None,
                    status: SessionStatus::Completed,
                    created_at,
                }]
            } else {
                continuation.parent_lineage.clone()
            };
            items.push(SessionLineageEntry {
                session_id: session_id.clone(),
                iteration_index,
                continuation_kind: Some(continuation.kind),
                status: SessionStatus::Planning,
                created_at,
            });
            items
        } else {
            vec![SessionLineageEntry {
                session_id: session_id.clone(),
                iteration_index,
                continuation_kind: None,
                status: SessionStatus::Planning,
                created_at,
            }]
        };

        let manifest = SessionManifest {
            id: session_id,
            task: config.task.clone(),
            repo_snapshot,
            created_at,
            status: SessionStatus::Planning,
            ui_mode: config.ui_mode,
            workers_requested: config.workers,
            role_set: config.role_set.clone(),
            model: config.model.clone(),
            thinking_mode: config.thinking_mode,
            cleanup_success: config.cleanup_success,
            apply_mode: config.apply_mode,
            max_retries: config.max_retries,
            fail_fast: config.fail_fast,
            verification_commands: config.verification_commands.clone(),
            config_path: config.config_path.clone(),
            preset: config.preset,
            iteration_index,
            root_session_id,
            parent_session_id: config
                .continuation
                .as_ref()
                .map(|item| item.parent_session_id.clone()),
            continuation_kind: config.continuation.as_ref().map(|item| item.kind),
            feedback_history: config
                .continuation
                .as_ref()
                .map(|item| item.feedback_history.clone())
                .unwrap_or_default(),
            supersedes_session_id: config
                .continuation
                .as_ref()
                .map(|item| item.parent_session_id.clone()),
            baseline_artifacts: config
                .continuation
                .as_ref()
                .map(|item| item.baseline_artifacts.clone())
                .unwrap_or_default(),
            plan_todo: None,
            todo_states: Vec::new(),
            execution_graph: None,
            execution_contract: None,
            worker_results: Vec::new(),
            artifact_manifest: ArtifactManifest::default(),
            apply_result: None,
            verification_report: None,
            change_trust_report: None,
            doctor_report: None,
            final_summary: None,
            reused_plan_session_id: None,
            resumed_from_session_id: None,
            artifact_index: Vec::new(),
            timeline_events: Vec::new(),
            demo_summary: Vec::new(),
            lineage,
            timeline_path: session_dir.join("timeline.jsonl"),
            graph_path: session_dir.join("commander").join("execution-graph.json"),
            execution_contract_path: session_dir
                .join("commander")
                .join("execution-contract.json"),
            summary_json_path: session_dir.join("summary.json"),
            summary_markdown_path: session_dir.join("summary.md"),
            artifact_manifest_path: session_dir.join("artifact-manifest.json"),
            apply_plan_path: session_dir.join("integration").join("apply-plan.json"),
            apply_result_path: session_dir.join("integration").join("apply-result.json"),
            verification_report_path: session_dir
                .join("integration")
                .join("verification-report.json"),
            change_trust_report_path: session_dir
                .join("integration")
                .join("change-trust-report.json"),
            session_dir: session_dir.clone(),
        };

        let manifest_path = session_dir.join("manifest.json");
        let mut ctx = Self {
            manifest,
            manifest_path,
        };
        ctx.persist()?;
        ctx.persist_artifact_manifest()?;
        Ok(ctx)
    }

    pub fn set_status(&mut self, status: SessionStatus) -> Result<()> {
        self.manifest.status = status;
        if let Some(last) = self.manifest.lineage.last_mut() {
            last.status = status;
        }
        self.persist()
    }

    pub fn set_plan_todo(&mut self, plan_todo: PlanTodo) -> Result<()> {
        let json_path = self.plan_todo_json_path();
        let markdown_path = self.plan_todo_markdown_path();
        fs::create_dir_all(self.commander_dir()).with_context(|| {
            format!(
                "创建 commander 目录失败：{}",
                self.commander_dir().display()
            )
        })?;
        self.manifest.plan_todo = Some(plan_todo.clone());
        self.manifest.todo_states = plan_todo
            .todos
            .iter()
            .map(|item| TodoStateRecord {
                todo_id: item.id.clone(),
                title: item.title.clone(),
                status: TodoStatus::Pending,
                node_ids: Vec::new(),
                completed_node_ids: Vec::new(),
                commit_hash: None,
                last_message: Some("等待调度".to_string()),
            })
            .collect();
        fs::write(
            &json_path,
            serde_json::to_vec_pretty(&plan_todo).context("序列化 plan todo 失败")?,
        )
        .with_context(|| format!("写入 plan todo JSON 失败：{}", json_path.display()))?;
        fs::write(&markdown_path, render_plan_todo_markdown(&plan_todo)).with_context(|| {
            format!("写入 plan todo Markdown 失败：{}", markdown_path.display())
        })?;
        self.manifest.artifact_manifest.plan_todo_path = Some(json_path);
        self.persist_todo_states()?;
        self.persist_artifact_manifest()?;
        self.persist()
    }

    pub fn set_graph(&mut self, graph: ExecutionGraph) -> Result<()> {
        fs::create_dir_all(self.commander_dir()).with_context(|| {
            format!(
                "创建 commander 目录失败：{}",
                self.commander_dir().display()
            )
        })?;
        self.manifest.execution_graph = Some(graph.clone());
        let node_ids_by_todo = graph
            .nodes
            .iter()
            .filter_map(|node| {
                node.todo_id
                    .as_ref()
                    .map(|todo_id| (todo_id, node.id.clone()))
            })
            .fold(
                std::collections::HashMap::<String, Vec<String>>::new(),
                |mut acc, (todo_id, node_id)| {
                    acc.entry(todo_id.clone()).or_default().push(node_id);
                    acc
                },
            );
        for todo in &mut self.manifest.todo_states {
            todo.node_ids = node_ids_by_todo
                .get(&todo.todo_id)
                .cloned()
                .unwrap_or_default();
        }
        fs::write(
            &self.manifest.graph_path,
            serde_json::to_vec_pretty(&graph).context("序列化执行图失败")?,
        )
        .with_context(|| format!("写入执行图失败：{}", self.manifest.graph_path.display()))?;
        self.persist_todo_states()?;
        self.persist()
    }

    pub fn set_execution_contract(&mut self, contract: ExecutionContract) -> Result<()> {
        fs::create_dir_all(self.commander_dir()).with_context(|| {
            format!(
                "创建 commander 目录失败：{}",
                self.commander_dir().display()
            )
        })?;
        self.manifest.execution_contract = Some(contract.clone());
        fs::write(
            &self.manifest.execution_contract_path,
            serde_json::to_vec_pretty(&contract).context("序列化执行契约失败")?,
        )
        .with_context(|| {
            format!(
                "写入执行契约失败：{}",
                self.manifest.execution_contract_path.display()
            )
        })?;
        self.manifest.artifact_manifest.execution_contract_path =
            Some(self.manifest.execution_contract_path.clone());
        self.persist_artifact_manifest()?;
        self.persist()
    }

    pub fn add_worker_result(&mut self, result: WorkerResult) -> Result<()> {
        self.manifest.worker_results.push(result.clone());
        self.manifest.artifact_manifest.entries.push(ArtifactEntry {
            agent_id: result.agent_id,
            handoff_path: result.handoff_path,
            diff_path: result.diff_path,
            final_output_path: result.final_output_path,
            changed_files: result.changed_files,
        });
        self.persist_artifact_manifest()?;
        self.persist()
    }

    pub fn set_apply_result(&mut self, apply_result: ApplyResult) -> Result<()> {
        self.manifest.apply_result = Some(apply_result);
        self.manifest.artifact_manifest.apply_result_path =
            Some(self.manifest.apply_result_path.clone());
        self.persist_artifact_manifest()?;
        self.persist()
    }

    pub fn set_verification_report(&mut self, report: VerificationReport) -> Result<()> {
        self.manifest.verification_report = Some(report);
        self.manifest.artifact_manifest.verification_report_path =
            Some(self.manifest.verification_report_path.clone());
        self.persist_artifact_manifest()?;
        self.persist()
    }

    pub fn set_change_trust_report(&mut self, report: ChangeTrustReport) -> Result<()> {
        self.manifest.change_trust_report = Some(report.clone());
        fs::write(
            &self.manifest.change_trust_report_path,
            serde_json::to_vec_pretty(&report).context("序列化可信度报告失败")?,
        )
        .with_context(|| {
            format!(
                "写入可信度报告失败：{}",
                self.manifest.change_trust_report_path.display()
            )
        })?;
        self.manifest.artifact_manifest.change_trust_report_path =
            Some(self.manifest.change_trust_report_path.clone());
        self.persist_artifact_manifest()?;
        self.persist()
    }

    pub fn set_summary(&mut self, summary: FinalSummary) -> Result<()> {
        self.manifest.final_summary = Some(summary.clone());
        self.manifest.demo_summary = build_demo_summary(&self.manifest, &summary);
        fs::write(
            &self.manifest.summary_json_path,
            serde_json::to_vec_pretty(&summary).context("序列化 summary 失败")?,
        )
        .with_context(|| {
            format!(
                "写入 JSON summary 失败：{}",
                self.manifest.summary_json_path.display()
            )
        })?;
        fs::write(
            &self.manifest.summary_markdown_path,
            render_summary_markdown(&self.manifest, &summary),
        )
        .with_context(|| {
            format!(
                "写入 Markdown summary 失败：{}",
                self.manifest.summary_markdown_path.display()
            )
        })?;
        self.persist()
    }

    pub fn worker_dir(&self, agent_id: &str) -> PathBuf {
        self.manifest.session_dir.join("workers").join(agent_id)
    }

    pub fn commander_dir(&self) -> PathBuf {
        self.manifest.session_dir.join("commander")
    }

    pub fn todo_state_path(&self) -> PathBuf {
        self.commander_dir().join("todo-state.json")
    }

    pub fn plan_todo_json_path(&self) -> PathBuf {
        self.commander_dir().join("plan-todo.json")
    }

    pub fn plan_todo_markdown_path(&self) -> PathBuf {
        self.commander_dir().join("plan-todo.md")
    }

    pub fn feedback_json_path(&self) -> PathBuf {
        self.commander_dir().join("feedback.json")
    }

    pub fn feedback_markdown_path(&self) -> PathBuf {
        self.commander_dir().join("feedback.md")
    }

    pub fn iteration_summary_path(&self) -> PathBuf {
        self.commander_dir().join("iteration-summary.md")
    }

    pub fn lineage_path(&self) -> PathBuf {
        self.commander_dir().join("session-lineage.json")
    }

    pub fn latest_pointer_path(&self) -> PathBuf {
        self.sessions_root()
            .join(self.manifest.root_session_id_ref())
            .join("latest.md")
    }

    pub fn set_reused_plan_session_id(&mut self, session_id: impl Into<String>) -> Result<()> {
        self.manifest.reused_plan_session_id = Some(session_id.into());
        self.persist()
    }

    pub fn set_resumed_from_session_id(&mut self, session_id: impl Into<String>) -> Result<()> {
        self.manifest.resumed_from_session_id = Some(session_id.into());
        self.persist()
    }

    pub fn update_todo_status(
        &mut self,
        todo_id: &str,
        status: TodoStatus,
        message: impl Into<String>,
        commit_hash: Option<String>,
    ) -> Result<Option<TodoStateRecord>> {
        let message = message.into();
        let mut updated = None;
        for todo in &mut self.manifest.todo_states {
            if todo.todo_id == todo_id {
                todo.status = status;
                todo.last_message = Some(message.clone());
                if let Some(hash) = commit_hash.clone() {
                    todo.commit_hash = Some(hash);
                }
                updated = Some(todo.clone());
                break;
            }
        }
        if updated.is_some() {
            self.persist_todo_states()?;
            self.persist()?;
        }
        Ok(updated)
    }

    pub fn mark_todo_node_completed(&mut self, todo_id: &str, node_id: &str) -> Result<()> {
        for todo in &mut self.manifest.todo_states {
            if todo.todo_id == todo_id
                && !todo.completed_node_ids.iter().any(|item| item == node_id)
            {
                todo.completed_node_ids.push(node_id.to_string());
            }
        }
        self.persist_todo_states()?;
        self.persist()
    }

    pub fn persist_todo_states(&mut self) -> Result<()> {
        fs::create_dir_all(self.commander_dir()).with_context(|| {
            format!(
                "创建 commander 目录失败：{}",
                self.commander_dir().display()
            )
        })?;
        let path = self.todo_state_path();
        fs::write(
            &path,
            serde_json::to_vec_pretty(&self.manifest.todo_states)
                .context("序列化 todo 状态失败")?,
        )
        .with_context(|| format!("写入 todo 状态失败：{}", path.display()))?;
        self.manifest.artifact_manifest.todo_state_path = Some(path);
        self.persist_artifact_manifest()
    }

    pub fn persist(&mut self) -> Result<()> {
        self.sync_auxiliary_artifacts()?;
        self.refresh_indexes();
        fs::write(
            &self.manifest_path,
            serde_json::to_vec_pretty(&self.manifest).context("序列化 manifest 失败")?,
        )
        .with_context(|| format!("写入 manifest 失败：{}", self.manifest_path.display()))
    }

    pub fn persist_artifact_manifest(&mut self) -> Result<()> {
        self.sync_auxiliary_artifacts()?;
        self.refresh_indexes();
        fs::write(
            &self.manifest.artifact_manifest_path,
            serde_json::to_vec_pretty(&self.manifest.artifact_manifest)
                .context("序列化 artifact manifest 失败")?,
        )
        .with_context(|| {
            format!(
                "写入 artifact manifest 失败：{}",
                self.manifest.artifact_manifest_path.display()
            )
        })
    }

    pub fn append_timeline(&mut self, event: &RuntimeEvent) -> Result<()> {
        let record = RuntimeEventRecord {
            ts: Utc::now(),
            payload: event.clone(),
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.manifest.timeline_path)
            .with_context(|| {
                format!(
                    "打开 timeline 文件失败：{}",
                    self.manifest.timeline_path.display()
                )
            })?;
        let line = serde_json::to_string(&record).context("序列化 timeline 事件失败")?;
        writeln!(file, "{line}").context("写入 timeline 失败")?;

        self.manifest.timeline_events.push(TimelineEventSummary {
            ts: record.ts,
            title: timeline_title(event),
            detail: timeline_detail(event),
        });
        if self.manifest.timeline_events.len() > 256 {
            let overflow = self.manifest.timeline_events.len() - 256;
            self.manifest.timeline_events.drain(0..overflow);
        }
        self.persist()
    }

    fn refresh_indexes(&mut self) {
        self.manifest.artifact_index = build_artifact_index(&self.manifest);
    }

    fn sessions_root(&self) -> PathBuf {
        self.manifest
            .session_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.manifest.session_dir.clone())
    }

    fn sync_auxiliary_artifacts(&mut self) -> Result<()> {
        fs::create_dir_all(self.commander_dir()).with_context(|| {
            format!(
                "创建 commander 目录失败：{}",
                self.commander_dir().display()
            )
        })?;

        let lineage_path = self.lineage_path();
        fs::write(
            &lineage_path,
            serde_json::to_vec_pretty(&self.manifest.lineage).context("序列化 lineage 失败")?,
        )
        .with_context(|| format!("写入 lineage 失败：{}", lineage_path.display()))?;
        self.manifest.artifact_manifest.lineage_path = Some(lineage_path);

        let iteration_summary_path = self.iteration_summary_path();
        fs::write(
            &iteration_summary_path,
            render_iteration_summary_markdown(&self.manifest),
        )
        .with_context(|| format!("写入迭代摘要失败：{}", iteration_summary_path.display()))?;
        self.manifest.artifact_manifest.iteration_summary_path = Some(iteration_summary_path);

        let latest_pointer_path = self.latest_pointer_path();
        fs::write(&latest_pointer_path, render_latest_pointer(&self.manifest))
            .with_context(|| format!("写入 latest 指针失败：{}", latest_pointer_path.display()))?;
        self.manifest.artifact_manifest.latest_pointer_path = Some(latest_pointer_path);

        if !self.manifest.feedback_history.is_empty() {
            let feedback_json_path = self.feedback_json_path();
            fs::write(
                &feedback_json_path,
                serde_json::to_vec_pretty(&self.manifest.feedback_history)
                    .context("序列化反馈记录失败")?,
            )
            .with_context(|| format!("写入反馈 JSON 失败：{}", feedback_json_path.display()))?;
            self.manifest.artifact_manifest.feedback_json_path = Some(feedback_json_path);

            let feedback_markdown_path = self.feedback_markdown_path();
            fs::write(
                &feedback_markdown_path,
                render_feedback_markdown(&self.manifest.feedback_history),
            )
            .with_context(|| {
                format!(
                    "写入反馈 Markdown 失败：{}",
                    feedback_markdown_path.display()
                )
            })?;
            self.manifest.artifact_manifest.feedback_markdown_path = Some(feedback_markdown_path);
        }

        Ok(())
    }
}

pub fn load_session(target_dir: &Path, session_id: Option<&str>) -> Result<SessionManifest> {
    let sessions_root = resolve_sessions_root(target_dir)?;
    if !sessions_root.exists() {
        bail!("未找到 .codex-forge/sessions：{}", sessions_root.display());
    }

    let session_path = if let Some(id) = session_id {
        sessions_root.join(id)
    } else {
        latest_session_dir(&sessions_root)?
    };

    let manifest_path = session_path.join("manifest.json");
    let content = fs::read_to_string(&manifest_path)
        .with_context(|| format!("读取 manifest 失败：{}", manifest_path.display()))?;
    let manifest = serde_json::from_str(&content).context("解析 manifest 失败")?;
    Ok(normalize_loaded_manifest(manifest))
}

pub fn cleanup_session_lineage(target_dir: &Path, session_id: &str) -> Result<CleanupReport> {
    let sessions_root = resolve_sessions_root(target_dir)?;
    if !sessions_root.exists() {
        bail!("未找到 .codex-forge/sessions：{}", sessions_root.display());
    }

    let entries = load_all_session_entries(&sessions_root)?;
    let mut removed_ids = entries
        .iter()
        .filter(|entry| session_matches_cleanup_target(&entry.manifest, session_id))
        .map(|entry| entry.manifest.id.clone())
        .collect::<Vec<_>>();
    removed_ids.sort();
    removed_ids.dedup();

    if removed_ids.is_empty() {
        bail!("未找到 session `{session_id}`，无法清理");
    }

    let removed_id_set = removed_ids.iter().cloned().collect::<HashSet<_>>();
    let affected_roots = entries
        .iter()
        .filter(|entry| removed_id_set.contains(&entry.manifest.id))
        .map(|entry| entry.manifest.root_session_id_ref().to_string())
        .collect::<HashSet<_>>();

    for entry in entries
        .iter()
        .filter(|entry| removed_id_set.contains(&entry.manifest.id))
    {
        if entry.path.exists() {
            fs::remove_dir_all(&entry.path)
                .with_context(|| format!("删除 session 目录失败：{}", entry.path.display()))?;
        }
    }

    let remaining = entries
        .into_iter()
        .filter(|entry| !removed_id_set.contains(&entry.manifest.id))
        .map(|entry| entry.manifest)
        .collect::<Vec<_>>();

    for root_session_id in affected_roots {
        rewrite_latest_pointer(&sessions_root, &remaining, &root_session_id)?;
    }

    let forge_dir = sessions_root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| sessions_root.clone());
    crate::workspace::cleanup_empty_dirs(&forge_dir)?;

    Ok(CleanupReport {
        scope: CleanupScope::SessionCascade,
        repo_root: forge_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| target_dir.to_path_buf()),
        forge_dir,
        removed_sessions: removed_ids,
        had_artifacts: true,
    })
}

pub fn cleanup_all_forge_artifacts(target_dir: &Path) -> Result<CleanupReport> {
    let sessions_root = resolve_sessions_root(target_dir)?;
    let forge_dir = sessions_root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| sessions_root.clone());
    let removed_sessions = if sessions_root.exists() {
        load_all_session_entries(&sessions_root)?
            .into_iter()
            .map(|entry| entry.manifest.id)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let had_artifacts = forge_dir.exists();
    if had_artifacts {
        fs::remove_dir_all(&forge_dir)
            .with_context(|| format!("删除 .codex-forge 目录失败：{}", forge_dir.display()))?;
    }

    Ok(CleanupReport {
        scope: CleanupScope::AllArtifacts,
        repo_root: forge_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| target_dir.to_path_buf()),
        forge_dir,
        removed_sessions,
        had_artifacts,
    })
}

pub fn reset_session_lineage(target_dir: &Path, session_id: &str) -> Result<ResetReport> {
    let sessions_root = resolve_sessions_root(target_dir)?;
    if !sessions_root.exists() {
        bail!("未找到 .codex-forge/sessions：{}", sessions_root.display());
    }

    let entries = load_all_session_entries(&sessions_root)?;
    let matched_entries = entries
        .iter()
        .filter(|entry| session_matches_cleanup_target(&entry.manifest, session_id))
        .collect::<Vec<_>>();
    if matched_entries.is_empty() {
        bail!("未找到 session `{session_id}`，无法重置");
    }

    let reset_commits = collect_reset_commit_hashes(&matched_entries);
    let repo_root = sessions_root
        .parent()
        .and_then(|path| path.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| target_dir.to_path_buf());
    let reset_to = if reset_commits.is_empty() {
        None
    } else {
        Some(reset_repo_before_commits(&repo_root, &reset_commits)?)
    };

    let cleanup = cleanup_session_lineage(target_dir, session_id)?;
    Ok(ResetReport {
        removed_sessions: cleanup.removed_sessions,
        reset_commits,
        reset_to,
    })
}

pub fn find_reusable_plan_session(
    target_dir: &Path,
    task: &str,
    workers: usize,
    role_set: &str,
) -> Result<Option<SessionManifest>> {
    let sessions_root = resolve_sessions_root(target_dir)?;
    if !sessions_root.exists() {
        return Ok(None);
    }

    let mut dirs = fs::read_dir(&sessions_root)
        .with_context(|| format!("读取 sessions 根目录失败：{}", sessions_root.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    dirs.sort();

    for dir in dirs.into_iter().rev() {
        let manifest_path = dir.join("manifest.json");
        let Ok(content) = fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<SessionManifest>(&content) else {
            continue;
        };
        let manifest = normalize_loaded_manifest(manifest);
        if is_reusable_plan_manifest(&manifest, task, workers, role_set) {
            return Ok(Some(manifest));
        }
    }

    Ok(None)
}

fn resolve_sessions_root(target_dir: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(target_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .with_context(|| format!("执行 git rev-parse 失败：{}", target_dir.display()))?;

    let repo_root = if output.status.success() {
        PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        target_dir.to_path_buf()
    };

    Ok(repo_root.join(".codex-forge").join("sessions"))
}

#[derive(Debug)]
struct SessionCleanupEntry {
    manifest: SessionManifest,
    path: PathBuf,
}

fn load_all_session_entries(sessions_root: &Path) -> Result<Vec<SessionCleanupEntry>> {
    let mut entries = fs::read_dir(sessions_root)
        .with_context(|| format!("读取 sessions 根目录失败：{}", sessions_root.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|path| {
            let manifest_path = path.join("manifest.json");
            let raw = fs::read_to_string(&manifest_path).ok()?;
            let manifest = serde_json::from_str::<SessionManifest>(&raw).ok()?;
            Some(SessionCleanupEntry {
                manifest: normalize_loaded_manifest(manifest),
                path,
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.manifest.id.cmp(&right.manifest.id));
    Ok(entries)
}

fn collect_reset_commit_hashes(entries: &[&SessionCleanupEntry]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut hashes = Vec::new();

    for entry in entries {
        let Some(apply_result) = &entry.manifest.apply_result else {
            continue;
        };
        for record in &apply_result.todo_commits {
            let Some(hash) = &record.commit_hash else {
                continue;
            };
            if seen.insert(hash.clone()) {
                hashes.push(hash.clone());
            }
        }
    }

    hashes
}

fn session_matches_cleanup_target(manifest: &SessionManifest, session_id: &str) -> bool {
    manifest.id == session_id
        || manifest
            .lineage
            .iter()
            .any(|entry| entry.session_id == session_id)
        || manifest.parent_session_id.as_deref() == Some(session_id)
}

fn reset_repo_before_commits(repo_root: &Path, reset_commits: &[String]) -> Result<String> {
    if reset_commits.is_empty() {
        bail!("没有可回滚的 commit");
    }
    if !git_is_clean_sync(repo_root)? {
        bail!("目标工作区不干净，拒绝一键重置；请先处理未提交改动");
    }

    let expected_tail = reset_commits.iter().rev().cloned().collect::<Vec<_>>();
    let actual_tail = git_lines(
        repo_root,
        &[
            "rev-list",
            "--first-parent",
            "--max-count",
            &expected_tail.len().to_string(),
            "HEAD",
        ],
        "读取当前 HEAD 提交链失败",
    )?;
    if actual_tail != expected_tail {
        bail!("目标 session 的提交已不在当前 HEAD 尾部，拒绝自动重置，避免误删后续人工提交");
    }

    let oldest = reset_commits
        .first()
        .context("缺少最早 commit，无法计算重置基线")?;
    let reset_to = git_stdout_trimmed(
        repo_root,
        &[&format!("{oldest}^")],
        "定位重置基线失败",
        "rev-parse",
    )?;

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["reset", "--hard"])
        .arg(&reset_to)
        .output()
        .with_context(|| format!("执行 git reset --hard 失败：{}", repo_root.display()))?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }

    Ok(reset_to)
}

fn git_is_clean_sync(repo_root: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--short", "--untracked-files=all"])
        .output()
        .with_context(|| format!("执行 git status 失败：{}", repo_root.display()))?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }

    let status_text = String::from_utf8_lossy(&output.stdout).to_string();
    let dirty_lines = status_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.ends_with(".codex-forge") && !line.contains(".codex-forge/"))
        .collect::<Vec<_>>();
    Ok(dirty_lines.is_empty())
}

fn git_lines(repo_root: &Path, args: &[&str], context: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("{context}：{}", repo_root.display()))?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}

fn git_stdout_trimmed(
    repo_root: &Path,
    extra_args: &[&str],
    context: &str,
    command: &str,
) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg(command)
        .args(extra_args)
        .output()
        .with_context(|| format!("{context}：{}", repo_root.display()))?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn rewrite_latest_pointer(
    sessions_root: &Path,
    remaining: &[SessionManifest],
    root_session_id: &str,
) -> Result<()> {
    let latest_pointer_path = sessions_root.join(root_session_id).join("latest.md");
    let latest = remaining
        .iter()
        .filter(|manifest| manifest.root_session_id_ref() == root_session_id)
        .max_by(|left, right| {
            left.iteration_index_value()
                .cmp(&right.iteration_index_value())
                .then_with(|| left.id.cmp(&right.id))
        });

    if let Some(manifest) = latest {
        if let Some(parent) = latest_pointer_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建 latest 指针目录失败：{}", parent.display()))?;
        }
        fs::write(&latest_pointer_path, render_latest_pointer(manifest))
            .with_context(|| format!("回写 latest 指针失败：{}", latest_pointer_path.display()))?;
    } else if latest_pointer_path.exists() {
        fs::remove_file(&latest_pointer_path).with_context(|| {
            format!(
                "删除失效 latest 指针失败：{}",
                latest_pointer_path.display()
            )
        })?;
        if let Some(parent) = latest_pointer_path.parent() {
            crate::workspace::cleanup_empty_dirs(parent)?;
        }
    }

    Ok(())
}

fn latest_session_dir(sessions_root: &Path) -> Result<PathBuf> {
    let mut dirs = fs::read_dir(sessions_root)
        .with_context(|| format!("读取 sessions 根目录失败：{}", sessions_root.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();

    dirs.sort();
    dirs.pop()
        .ok_or_else(|| anyhow::anyhow!("没有可回放的 session"))
}

fn normalize_loaded_manifest(mut manifest: SessionManifest) -> SessionManifest {
    if manifest.iteration_index == 0 {
        manifest.iteration_index = 1;
    }
    if manifest.root_session_id.is_empty() {
        manifest.root_session_id = manifest.id.clone();
    }
    if manifest.lineage.is_empty() {
        manifest.lineage.push(SessionLineageEntry {
            session_id: manifest.id.clone(),
            iteration_index: manifest.iteration_index,
            continuation_kind: manifest.continuation_kind,
            status: manifest.status,
            created_at: manifest.created_at,
        });
    }
    manifest
}

fn render_summary_markdown(manifest: &SessionManifest, summary: &FinalSummary) -> String {
    let worker_lines = manifest
        .worker_results
        .iter()
        .map(|result| {
            format!(
                "- `{}` / `{}`：{}（handoff：`{}`）",
                result.agent_id,
                result.role,
                result.status.label(),
                result
                    .handoff_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "无".to_string())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let accepted_files = render_bullets(&summary.accepted_files);
    let manual_review_files = render_bullets(&summary.manual_review_files);
    let rejected_files = render_bullets(&summary.rejected_files);
    let verified_capabilities = render_bullets(&summary.verified_capabilities);
    let blocked_verifications = render_bullets(&summary.blocked_verifications);
    let open_risks = render_bullets(&summary.open_risks);
    let recommended_next_action = render_bullets(&summary.recommended_next_action);
    let todo_states = render_todo_states(&summary.todo_states);
    let evidence_summary = render_bullets(&summary.evidence_summary);
    let demo_summary = render_bullets(&manifest.demo_summary);
    let feedback_summary = render_bullets(&summary.feedback_summary);
    let delta_summary = render_bullets(&summary.delta_summary);
    let completed_this_iteration = render_bullets(&summary.completed_this_iteration);
    let unaccepted_feedback = render_bullets(&summary.unaccepted_feedback);
    let review_findings = summary
        .review_report
        .as_ref()
        .map(|report| render_bullets(&report.blocking_findings))
        .unwrap_or_else(|| "- 无".to_string());
    let review_scopes = summary
        .review_report
        .as_ref()
        .map(|report| {
            let accepted = render_bullets(&report.accepted_scopes);
            let rejected = render_bullets(&report.rejected_scopes);
            format!(
                "**放行范围**\n\n{}\n\n**拦截范围**\n\n{}",
                accepted, rejected
            )
        })
        .unwrap_or_else(|| "**放行范围**\n\n- 无\n\n**拦截范围**\n\n- 无".to_string());

    format!(
        "# Session {}\n\n\
## 任务\n\n{}\n\n\
## 迭代信息\n\n\
- 当前轮次：V{}\n\
- 根会话：`{}`\n\
- 来源会话：{}\n\n\
## 总览\n\n{}\n\n\
## 决策状态\n\n\
- 结果：{}\n\
- reviewer gate：{}\n\
- apply：{}\n\
- 可信度：{}\n\n\
## 执行图\n\n`{}`\n\n\
## Worker 结果\n\n{}\n\n\
## 执行契约\n\n`{}`\n\n\
## 应用报告\n\n`{}`\n\n\
## 验证报告\n\n`{}`\n\n\
## 接收文件\n\n{}\n\n\
## 人工复核文件\n\n{}\n\n\
## 拒绝文件\n\n{}\n\n\
## 已验证能力\n\n{}\n\n\
## 因环境受阻\n\n{}\n\n\
## Todo 状态\n\n{}\n\n\
## 本轮反馈\n\n{}\n\n\
## 相对上一轮变化\n\n{}\n\n\
## 本轮完成内容\n\n{}\n\n\
## 审阅关卡\n\n{}\n\n{}\n\n\
## 证据摘要\n\n{}\n\n\
## Demo 摘要\n\n{}\n\n\
## 未关闭风险\n\n{}\n\n\
## 未采纳反馈\n\n{}\n\n\
## 下一步\n\n{}\n",
        manifest.id,
        manifest.task,
        summary.iteration_index,
        manifest.root_session_id_ref(),
        summary
            .based_on_session_id
            .as_deref()
            .map(|item| format!("`{item}`"))
            .unwrap_or_else(|| "无".to_string()),
        summary.overview,
        summary.result_status.label(),
        summary
            .review_gate
            .map(|item| item.label().to_string())
            .unwrap_or_else(|| "无".to_string()),
        summary.apply_status.label(),
        summary.trust_level.label(),
        manifest.graph_path.display(),
        worker_lines,
        manifest.execution_contract_path.display(),
        manifest.apply_result_path.display(),
        manifest.verification_report_path.display(),
        accepted_files,
        manual_review_files,
        rejected_files,
        verified_capabilities,
        blocked_verifications,
        todo_states,
        feedback_summary,
        delta_summary,
        completed_this_iteration,
        review_findings,
        review_scopes,
        evidence_summary,
        demo_summary,
        open_risks,
        unaccepted_feedback,
        recommended_next_action,
    )
}

fn render_plan_todo_markdown(plan_todo: &PlanTodo) -> String {
    let todo_lines = plan_todo
        .todos
        .iter()
        .map(|item| {
            let details = render_bullets(&item.details);
            let deps = if item.dependencies.is_empty() {
                "- 无".to_string()
            } else {
                render_bullets(&item.dependencies)
            };
            let done = render_bullets(&item.completion_criteria);
            format!(
                "## {} {}\n\n**目标**\n\n{}\n\n**细节**\n\n{}\n\n**依赖**\n\n{}\n\n**完成标准**\n\n{}\n",
                item.id, item.title, item.goal, details, deps, done
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let risks = render_bullets(&plan_todo.risks);
    let notes = render_bullets(&plan_todo.planning_notes);
    let feedback_summary = render_bullets(&plan_todo.feedback_summary);
    let delta_summary = render_bullets(&plan_todo.delta_summary);

    format!(
        "# 计划清单\n\n\
## 迭代信息\n\n\
- 当前轮次：V{}\n\
- 来源会话：{}\n\n\
## 摘要\n\n{}\n\n\
## 推进策略\n\n{}\n\n\
## 本轮反馈\n\n{}\n\n\
## 相对上一轮变化\n\n{}\n\n\
## Todo\n\n{}\n\
## 风险\n\n{}\n\n\
## 规划备注\n\n{}\n",
        plan_todo.iteration_index,
        plan_todo
            .source_session_id
            .as_deref()
            .map(|item| format!("`{item}`"))
            .unwrap_or_else(|| "无".to_string()),
        plan_todo.summary,
        plan_todo.approach,
        feedback_summary,
        delta_summary,
        todo_lines,
        risks,
        notes
    )
}

fn is_reusable_plan_manifest(
    manifest: &SessionManifest,
    task: &str,
    workers: usize,
    role_set: &str,
) -> bool {
    manifest.task == task
        && manifest.workers_requested == workers
        && manifest.role_set == role_set
        && manifest.status == SessionStatus::Completed
        && manifest.apply_mode == ApplyMode::None
        && manifest.worker_results.is_empty()
        && manifest.execution_graph.is_some()
}

fn render_bullets(items: &[String]) -> String {
    if items.is_empty() {
        "- 无".to_string()
    } else {
        items
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn render_todo_states(items: &[TodoStateRecord]) -> String {
    if items.is_empty() {
        return "- 无".to_string();
    }
    items
        .iter()
        .map(|item| {
            let commit = item
                .commit_hash
                .as_ref()
                .map(|hash| format!(" / commit {}", hash))
                .unwrap_or_default();
            let message = item
                .last_message
                .as_ref()
                .map(|text| format!(" / {}", text))
                .unwrap_or_default();
            format!(
                "- {} {}：{}{}{}",
                item.todo_id,
                item.title,
                item.status.label(),
                commit,
                message
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_feedback_markdown(items: &[crate::model::FeedbackRecord]) -> String {
    let body = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            format!(
                "## 反馈 {}\n\n- 作者：{}\n- 标题：{}\n- 意图摘要：{}\n- 原始反馈：{}\n- 默认假设：{}\n",
                index + 1,
                item.author,
                item.title.clone().unwrap_or_else(|| "无".to_string()),
                item.intent_summary,
                item.raw_feedback,
                if item.accepted_assumptions.is_empty() {
                    "无".to_string()
                } else {
                    item.accepted_assumptions.join("；")
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("# 反馈记录\n\n{body}\n")
}

fn render_iteration_summary_markdown(manifest: &SessionManifest) -> String {
    let latest_feedback = manifest
        .feedback_history
        .last()
        .map(|item| item.intent_summary.clone())
        .unwrap_or_else(|| "无新增反馈".to_string());
    let plan_delta = manifest
        .plan_todo
        .as_ref()
        .map(|item| render_bullets(&item.delta_summary))
        .unwrap_or_else(|| "- 无".to_string());
    let summary_delta = manifest
        .final_summary
        .as_ref()
        .map(|item| render_bullets(&item.delta_summary))
        .unwrap_or_else(|| "- 无".to_string());
    format!(
        "# 迭代摘要\n\n\
- 当前 session：`{}`\n\
- 当前轮次：V{}\n\
- 根会话：`{}`\n\
- 来源会话：{}\n\
- 延续类型：{}\n\
- 最新反馈：{}\n\n\
## 规划变化\n\n{}\n\n\
## 执行变化\n\n{}\n",
        manifest.id,
        manifest.iteration_index_value(),
        manifest.root_session_id_ref(),
        manifest
            .parent_session_id
            .as_deref()
            .map(|item| format!("`{item}`"))
            .unwrap_or_else(|| "无".to_string()),
        manifest
            .continuation_kind
            .map(|item| item.label().to_string())
            .unwrap_or_else(|| "root".to_string()),
        latest_feedback,
        plan_delta,
        summary_delta,
    )
}

fn render_latest_pointer(manifest: &SessionManifest) -> String {
    format!(
        "# Latest Iteration\n\n\
- root_session: `{}`\n\
- latest_session: `{}`\n\
- version: `V{}`\n\
- status: {}\n\
- continue: `codex-forge continue --session {} --feedback \"...\"`\n",
        manifest.root_session_id_ref(),
        manifest.id,
        manifest.iteration_index_value(),
        manifest.status.label(),
        manifest.id
    )
}

fn build_artifact_index(manifest: &SessionManifest) -> Vec<ArtifactIndexEntry> {
    let mut items = Vec::new();
    if let Some(path) = &manifest.artifact_manifest.plan_todo_path {
        items.push(ArtifactIndexEntry {
            key: "plan_todo".to_string(),
            path: path.clone(),
        });
    }
    items.push(ArtifactIndexEntry {
        key: "timeline".to_string(),
        path: manifest.timeline_path.clone(),
    });
    items.push(ArtifactIndexEntry {
        key: "graph".to_string(),
        path: manifest.graph_path.clone(),
    });
    items.push(ArtifactIndexEntry {
        key: "summary_json".to_string(),
        path: manifest.summary_json_path.clone(),
    });
    items.push(ArtifactIndexEntry {
        key: "summary_markdown".to_string(),
        path: manifest.summary_markdown_path.clone(),
    });
    if let Some(path) = &manifest.artifact_manifest.execution_contract_path {
        items.push(ArtifactIndexEntry {
            key: "execution_contract".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.apply_plan_path {
        items.push(ArtifactIndexEntry {
            key: "apply_plan".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.apply_result_path {
        items.push(ArtifactIndexEntry {
            key: "apply_result".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.verification_report_path {
        items.push(ArtifactIndexEntry {
            key: "verification_report".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.change_trust_report_path {
        items.push(ArtifactIndexEntry {
            key: "change_trust_report".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.feedback_json_path {
        items.push(ArtifactIndexEntry {
            key: "feedback_json".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.feedback_markdown_path {
        items.push(ArtifactIndexEntry {
            key: "feedback_markdown".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.iteration_summary_path {
        items.push(ArtifactIndexEntry {
            key: "iteration_summary".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.lineage_path {
        items.push(ArtifactIndexEntry {
            key: "lineage".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.latest_pointer_path {
        items.push(ArtifactIndexEntry {
            key: "latest".to_string(),
            path: path.clone(),
        });
    }
    items
}

fn build_demo_summary(manifest: &SessionManifest, summary: &FinalSummary) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("会话状态：{}", manifest.status.label()));
    if let Some(preset) = manifest.preset {
        lines.push(format!("运行预设：{}", preset.label()));
    }
    lines.push(format!("结果：{}", summary.result_status.label()));
    if let Some(report) = &summary.review_report {
        lines.push(format!("review gate：{}", report.decision.label()));
    }
    lines.push(format!("自动接收文件：{}", summary.accepted_files.len()));
    lines.push(format!("验证通过：{}", summary.verified_capabilities.len()));
    if !summary.open_risks.is_empty() {
        lines.push(format!("开放风险：{}", summary.open_risks.len()));
    }
    lines
}

fn timeline_title(event: &RuntimeEvent) -> String {
    match event {
        RuntimeEvent::PhaseChanged { .. } => "阶段切换".to_string(),
        RuntimeEvent::CommanderNote { .. } => "指挥备注".to_string(),
        RuntimeEvent::GraphReady { .. } => "执行图就绪".to_string(),
        RuntimeEvent::TodoStateChanged { title, .. } => format!("Todo 更新：{title}"),
        RuntimeEvent::WorkerDispatched { agent_id, .. } => format!("启动 {agent_id}"),
        RuntimeEvent::WorkerUpdate { agent_id, .. } => format!("Worker 更新：{agent_id}"),
        RuntimeEvent::HandoffReady { agent_id, .. } => format!("交接就绪：{agent_id}"),
        RuntimeEvent::WorkerFinished { result } => format!("Worker 完成：{}", result.agent_id),
        RuntimeEvent::ApplyPlanReady { .. } => "应用计划就绪".to_string(),
        RuntimeEvent::ReviewGateReady { .. } => "审阅关卡结论".to_string(),
        RuntimeEvent::ApplyUpdate { .. } => "应用更新".to_string(),
        RuntimeEvent::VerificationReady { stage, .. } => format!("验证完成：{stage}"),
        RuntimeEvent::SummaryReady { .. } => "总结完成".to_string(),
    }
}

fn timeline_detail(event: &RuntimeEvent) -> String {
    match event {
        RuntimeEvent::PhaseChanged { phase } => phase.clone(),
        RuntimeEvent::CommanderNote { message } => message.clone(),
        RuntimeEvent::GraphReady {
            nodes,
            dependencies,
        } => format!("节点 {nodes} / 依赖 {dependencies}"),
        RuntimeEvent::TodoStateChanged {
            todo_id,
            status,
            message,
            ..
        } => format!("{todo_id} -> {} / {}", status.label(), message),
        RuntimeEvent::WorkerDispatched { role, title, .. } => format!("{role} / {title}"),
        RuntimeEvent::WorkerUpdate { kind, message, .. } => {
            format!("{kind} / {}", message.replace('\n', " "))
        }
        RuntimeEvent::HandoffReady { handoff_path, .. } => handoff_path.display().to_string(),
        RuntimeEvent::WorkerFinished { result } => {
            format!("{} / {}", result.task_title, result.status.label())
        }
        RuntimeEvent::ApplyPlanReady { mode, operations } => {
            format!("{} / {} 个 patch", mode, operations)
        }
        RuntimeEvent::ReviewGateReady { report } => format!(
            "{} / {}",
            report.decision.label(),
            report
                .confidence_reasoning
                .clone()
                .unwrap_or_else(|| "无补充说明".to_string())
        ),
        RuntimeEvent::ApplyUpdate { message } => message.clone(),
        RuntimeEvent::VerificationReady {
            success, message, ..
        } => format!("{} / {}", if *success { "成功" } else { "失败" }, message),
        RuntimeEvent::SummaryReady { summary } => summary.overview.clone(),
    }
}
