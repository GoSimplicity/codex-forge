use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::memory;
use crate::model::{
    ApplyMode, ApplyResult, ArtifactEntry, ArtifactIndexEntry, ArtifactManifest, ChangeTrustReport,
    ExecutionContract, ExecutionGraph, FinalSummary, ManualDeliveryResult, ManualReviewState,
    PlanTodo, RepoSnapshot, RuntimeEvent, RuntimeEventRecord, SessionConfig, SessionKind,
    SessionLineageEntry, SessionManifest, SessionStatus, TimelineEventSummary, TodoStateRecord,
    TodoStatus, VerificationReport, WorkerResult,
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
            session_kind: if config.plan_only {
                SessionKind::Plan
            } else {
                SessionKind::Run
            },
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
            shared_context_version: 1,
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
            manual_delivery_result: None,
            manual_review_state: None,
            review_fix: config
                .continuation
                .as_ref()
                .and_then(|item| item.review_fix.clone()),
            memory_manifest: None,
            source_plan_session_id: config.source_plan_session_id.clone(),
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
        ctx.sync_memory_manifest()?;
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
        let deliverable_plan_path = self.manifest.deliverable_plan_path();
        fs::create_dir_all(self.manifest.deliverables_dir()).with_context(|| {
            format!(
                "创建过程工件目录失败：{}",
                self.manifest.deliverables_dir().display()
            )
        })?;
        fs::write(
            &deliverable_plan_path,
            render_user_plan_deliverable(&self.manifest, &plan_todo),
        )
        .with_context(|| format!("写入过程计划摘要失败：{}", deliverable_plan_path.display()))?;
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
        fs::create_dir_all(self.manifest.deliverables_dir()).with_context(|| {
            format!(
                "创建过程工件目录失败：{}",
                self.manifest.deliverables_dir().display()
            )
        })?;
        fs::write(
            self.manifest.deliverable_summary_path(),
            render_user_summary_deliverable(&self.manifest, &summary),
        )
        .with_context(|| {
            format!(
                "写入过程总结摘要失败：{}",
                self.manifest.deliverable_summary_path().display()
            )
        })?;
        fs::write(
            self.manifest.deliverable_changes_path(),
            render_user_changes_deliverable(&self.manifest, &summary),
        )
        .with_context(|| {
            format!(
                "写入过程变更摘要失败：{}",
                self.manifest.deliverable_changes_path().display()
            )
        })?;
        fs::write(
            self.manifest.deliverable_verify_path(),
            render_user_verify_deliverable(&self.manifest, &summary),
        )
        .with_context(|| {
            format!(
                "写入过程验证摘要失败：{}",
                self.manifest.deliverable_verify_path().display()
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

    #[allow(dead_code)]
    pub fn set_reused_plan_session_id(&mut self, session_id: impl Into<String>) -> Result<()> {
        self.manifest.reused_plan_session_id = Some(session_id.into());
        self.persist()
    }

    pub fn set_source_plan_session_id(&mut self, session_id: impl Into<String>) -> Result<()> {
        self.manifest.source_plan_session_id = Some(session_id.into());
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

    pub fn sync_memory_manifest(&mut self) -> Result<()> {
        memory::sync_memory_manifest(&mut self.manifest)?;
        self.persist_artifact_manifest()?;
        self.persist()
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

pub fn set_manual_delivery_result_for_loaded_session(
    manifest: &mut SessionManifest,
    result: ManualDeliveryResult,
) -> Result<()> {
    let path = manifest.manual_delivery_result_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建手动交付目录失败：{}", parent.display()))?;
    }
    fs::write(
        &path,
        serde_json::to_vec_pretty(&result).context("序列化手动交付结果失败")?,
    )
    .with_context(|| format!("写入手动交付结果失败：{}", path.display()))?;
    manifest.manual_delivery_result = Some(result);
    manifest.artifact_manifest.manual_delivery_result_path = Some(path);
    manifest.artifact_index = build_artifact_index(manifest);
    fs::write(
        manifest.session_dir.join("manifest.json"),
        serde_json::to_vec_pretty(manifest).context("序列化 session manifest 失败")?,
    )
    .with_context(|| {
        format!(
            "写入 session manifest 失败：{}",
            manifest.session_dir.join("manifest.json").display()
        )
    })?;
    fs::write(
        &manifest.artifact_manifest_path,
        serde_json::to_vec_pretty(&manifest.artifact_manifest)
            .context("序列化 artifact manifest 失败")?,
    )
    .with_context(|| {
        format!(
            "写入 artifact manifest 失败：{}",
            manifest.artifact_manifest_path.display()
        )
    })?;
    Ok(())
}

pub fn set_manual_review_state_for_loaded_session(
    manifest: &mut SessionManifest,
    state: ManualReviewState,
) -> Result<()> {
    let path = manifest.manual_review_state_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建人工审查目录失败：{}", parent.display()))?;
    }
    fs::write(
        &path,
        serde_json::to_vec_pretty(&state).context("序列化人工审查状态失败")?,
    )
    .with_context(|| format!("写入人工审查状态失败：{}", path.display()))?;
    manifest.manual_review_state = Some(state);
    manifest.artifact_manifest.manual_review_state_path = Some(path);
    manifest.artifact_index = build_artifact_index(manifest);
    fs::write(
        manifest.session_dir.join("manifest.json"),
        serde_json::to_vec_pretty(manifest).context("序列化 session manifest 失败")?,
    )
    .with_context(|| {
        format!(
            "写入 session manifest 失败：{}",
            manifest.session_dir.join("manifest.json").display()
        )
    })?;
    fs::write(
        &manifest.artifact_manifest_path,
        serde_json::to_vec_pretty(&manifest.artifact_manifest)
            .context("序列化 artifact manifest 失败")?,
    )
    .with_context(|| {
        format!(
            "写入 artifact manifest 失败：{}",
            manifest.artifact_manifest_path.display()
        )
    })?;
    Ok(())
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

#[allow(dead_code)]
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
    if manifest.shared_context_version == 0 {
        manifest.shared_context_version = 1;
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
    if manifest.source_plan_session_id.is_none() {
        manifest.source_plan_session_id = manifest.reused_plan_session_id.clone();
    }
    manifest.session_kind = infer_session_kind(&manifest);
    manifest
}

fn infer_session_kind(manifest: &SessionManifest) -> SessionKind {
    if !manifest.worker_results.is_empty()
        || manifest.apply_result.is_some()
        || manifest.final_summary.is_some()
        || manifest.resumed_from_session_id.is_some()
        || manifest.source_plan_session_id.is_some()
        || !matches!(manifest.apply_mode, ApplyMode::None)
    {
        SessionKind::Run
    } else {
        SessionKind::Plan
    }
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
- 目标目录交付：{}\n\
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
        if manifest.delivered_to_target() {
            "已交付"
        } else {
            "未交付"
        },
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

fn render_user_plan_deliverable(manifest: &SessionManifest, plan_todo: &PlanTodo) -> String {
    let todos = plan_todo
        .todos
        .iter()
        .map(|item| {
            let deps = if item.dependencies.is_empty() {
                "无依赖".to_string()
            } else {
                format!("依赖：{}", item.dependencies.join("、"))
            };
            let criteria = if item.completion_criteria.is_empty() {
                "完成标准：待补充".to_string()
            } else {
                format!("完成标准：{}", item.completion_criteria.join("；"))
            };
            format!(
                "## {} {}\n\n- 目标：{}\n- {}\n- {}\n",
                item.id, item.title, item.goal, deps, criteria
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let risks = render_bullets(&plan_todo.risks);

    format!(
        "# Codex Forge 计划\n\n\
- 目标仓库：`{}`\n\
- 会话：`{}`\n\
- 当前轮次：V{}\n\
- 系统记录目录：`{}`\n\
- 你直接看的交付物目录：`{}`\n\n\
## 任务\n\n{}\n\n\
## 方案摘要\n\n{}\n\n\
## 推进策略\n\n{}\n\n\
## 待办清单\n\n{}\n\
## 主要风险\n\n{}\n",
        manifest.repo_root().display(),
        manifest.id,
        plan_todo.iteration_index,
        manifest.session_dir.display(),
        manifest.repo_root().display(),
        manifest.task,
        plan_todo.summary,
        plan_todo.approach,
        todos,
        risks,
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

fn render_user_summary_deliverable(manifest: &SessionManifest, summary: &FinalSummary) -> String {
    let next_steps = render_bullets(&summary.recommended_next_action);
    let completed = render_bullets(&summary.completed_this_iteration);
    let risks = render_bullets(&summary.open_risks);

    format!(
        "# Codex Forge 最终交付\n\n\
- 目标仓库：`{}`\n\
- 会话：`{}`\n\
- 系统记录目录：`{}`\n\
- 你直接看的交付物目录：`{}`\n\n\
## 任务\n\n{}\n\n\
## 结果结论\n\n{}\n\n\
- 结果：{}\n\
- 审阅结论：{}\n\
- 应用状态：{}\n\
- 目标目录交付：{}\n\
- 可信度：{}\n\n\
## 本轮完成内容\n\n{}\n\n\
## 下一步建议\n\n{}\n\n\
## 风险提示\n\n{}\n",
        manifest.repo_root().display(),
        manifest.id,
        manifest.session_dir.display(),
        manifest.repo_root().display(),
        manifest.task,
        summary.overview,
        summary.result_status.label(),
        summary
            .review_gate
            .map(|item| item.label().to_string())
            .unwrap_or_else(|| "无".to_string()),
        summary.apply_status.label(),
        if manifest.delivered_to_target() {
            "已交付"
        } else {
            "未交付"
        },
        summary.trust_level.label(),
        completed,
        next_steps,
        risks,
    )
}

fn render_user_changes_deliverable(manifest: &SessionManifest, summary: &FinalSummary) -> String {
    let accepted = render_bullets(&summary.accepted_files);
    let manual_review = render_bullets(&summary.manual_review_files);
    let rejected = render_bullets(&summary.rejected_files);
    let todo_states = render_todo_states(&summary.todo_states);
    let apply_result = manifest
        .apply_result
        .as_ref()
        .map(|result| {
            let commits = if result.todo_commits.is_empty() {
                "- 无".to_string()
            } else {
                result
                    .todo_commits
                    .iter()
                    .map(|item| {
                        format!(
                            "- {} / {} / {}",
                            item.todo_id,
                            item.status.label(),
                            item.commit_hash.as_deref().unwrap_or("未记录 commit")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            format!(
                "## 应用结果\n\n- 模式：{}\n- 状态：{}\n- 已同步到目标仓库：{}\n\n## Todo 提交记录\n\n{}\n",
                result.mode.label(),
                result.status.label(),
                if result.synced_to_target { "是" } else { "否" },
                commits
            )
        })
        .unwrap_or_else(|| "## 应用结果\n\n- 当前没有 apply 记录。\n".to_string());

    format!(
        "# Codex Forge 变更摘要\n\n\
- 目标仓库：`{}`\n\
- 会话：`{}`\n\n\
## 已接收变更\n\n{}\n\n\
## 需要人工复核\n\n{}\n\n\
## 被拒绝或未接收\n\n{}\n\n\
## Todo 当前状态\n\n{}\n\n\
{}\n",
        manifest.repo_root().display(),
        manifest.id,
        accepted,
        manual_review,
        rejected,
        todo_states,
        apply_result,
    )
}

fn render_user_verify_deliverable(manifest: &SessionManifest, summary: &FinalSummary) -> String {
    let verified = render_bullets(&summary.verified_capabilities);
    let blocked = render_bullets(&summary.blocked_verifications);
    let verification_report = manifest
        .verification_report
        .as_ref()
        .map(|report| {
            let integration = if report.integration.is_empty() {
                "- 无".to_string()
            } else {
                report
                    .integration
                    .iter()
                    .map(|item| {
                        format!(
                            "- {} / {} / {}",
                            item.stage,
                            item.capability,
                            item.status.label()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let final_run = if report.final_run.is_empty() {
                "- 无".to_string()
            } else {
                report
                    .final_run
                    .iter()
                    .map(|item| {
                        format!(
                            "- {} / {} / {}",
                            item.stage,
                            item.capability,
                            item.status.label()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            format!(
                "## 验证明细\n\n- 总体结论：{}\n\n### 集成验证\n\n{}\n\n### 最终验证\n\n{}\n",
                report.overall_status.label(),
                integration,
                final_run
            )
        })
        .unwrap_or_else(|| "## 验证明细\n\n- 当前没有验证报告。\n".to_string());

    format!(
        "# Codex Forge 验证摘要\n\n\
- 目标仓库：`{}`\n\
- 会话：`{}`\n\n\
## 已通过能力\n\n{}\n\n\
## 受阻项目\n\n{}\n\n\
{}\n",
        manifest.repo_root().display(),
        manifest.id,
        verified,
        blocked,
        verification_report,
    )
}

#[allow(dead_code)]
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
            kind: "plan".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    items.push(ArtifactIndexEntry {
        key: "timeline".to_string(),
        kind: "timeline".to_string(),
        scope: "session".to_string(),
        path: manifest.timeline_path.clone(),
    });
    items.push(ArtifactIndexEntry {
        key: "graph".to_string(),
        kind: "graph".to_string(),
        scope: "session".to_string(),
        path: manifest.graph_path.clone(),
    });
    items.push(ArtifactIndexEntry {
        key: "summary_json".to_string(),
        kind: "summary".to_string(),
        scope: "session".to_string(),
        path: manifest.summary_json_path.clone(),
    });
    items.push(ArtifactIndexEntry {
        key: "summary_markdown".to_string(),
        kind: "summary".to_string(),
        scope: "session".to_string(),
        path: manifest.summary_markdown_path.clone(),
    });
    if let Some(path) = &manifest.artifact_manifest.execution_contract_path {
        items.push(ArtifactIndexEntry {
            key: "execution_contract".to_string(),
            kind: "contract".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.apply_plan_path {
        items.push(ArtifactIndexEntry {
            key: "apply_plan".to_string(),
            kind: "apply".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.apply_result_path {
        items.push(ArtifactIndexEntry {
            key: "apply_result".to_string(),
            kind: "apply".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.verification_report_path {
        items.push(ArtifactIndexEntry {
            key: "verification_report".to_string(),
            kind: "verification".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.change_trust_report_path {
        items.push(ArtifactIndexEntry {
            key: "change_trust_report".to_string(),
            kind: "trust".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.manual_delivery_result_path {
        items.push(ArtifactIndexEntry {
            key: "manual_delivery_result".to_string(),
            kind: "delivery".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.manual_review_state_path {
        items.push(ArtifactIndexEntry {
            key: "manual_review_state".to_string(),
            kind: "review".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.feedback_json_path {
        items.push(ArtifactIndexEntry {
            key: "feedback_json".to_string(),
            kind: "feedback".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.feedback_markdown_path {
        items.push(ArtifactIndexEntry {
            key: "feedback_markdown".to_string(),
            kind: "feedback".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.iteration_summary_path {
        items.push(ArtifactIndexEntry {
            key: "iteration_summary".to_string(),
            kind: "summary".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.lineage_path {
        items.push(ArtifactIndexEntry {
            key: "lineage".to_string(),
            kind: "lineage".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.latest_pointer_path {
        items.push(ArtifactIndexEntry {
            key: "latest".to_string(),
            kind: "pointer".to_string(),
            scope: "shared".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.memory_manifest_path {
        items.push(ArtifactIndexEntry {
            key: "memory_manifest".to_string(),
            kind: "memory".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.shared_memory_index_path {
        items.push(ArtifactIndexEntry {
            key: "shared_memory_index".to_string(),
            kind: "memory".to_string(),
            scope: "shared".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.session_memory_entries_path {
        items.push(ArtifactIndexEntry {
            key: "session_memory_entries".to_string(),
            kind: "memory".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    if let Some(path) = &manifest.artifact_manifest.task_brief_path {
        items.push(ArtifactIndexEntry {
            key: "task_brief".to_string(),
            kind: "memory".to_string(),
            scope: "session".to_string(),
            path: path.clone(),
        });
    }
    for (key, path) in [
        ("deliverable_plan", manifest.deliverable_plan_path()),
        ("deliverable_summary", manifest.deliverable_summary_path()),
        ("deliverable_changes", manifest.deliverable_changes_path()),
        ("deliverable_verify", manifest.deliverable_verify_path()),
    ] {
        if path.exists() {
            items.push(ArtifactIndexEntry {
                key: key.to_string(),
                kind: "deliverable".to_string(),
                scope: "session".to_string(),
                path,
            });
        }
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
        RuntimeEvent::BrainStarted { .. } => "Brain 已接管".to_string(),
        RuntimeEvent::BrainThought { .. } => "Brain 思考".to_string(),
        RuntimeEvent::BrainDecisionMade { .. } => "Brain 决策".to_string(),
        RuntimeEvent::BrainEscalationRaised { .. } => "Brain 升级".to_string(),
        RuntimeEvent::SchedulerSnapshotUpdated { .. } => "调度快照".to_string(),
        RuntimeEvent::PhaseChanged { .. } => "阶段切换".to_string(),
        RuntimeEvent::CommanderNote { .. } => "过程说明".to_string(),
        RuntimeEvent::GraphReady { .. } => "方案完成".to_string(),
        RuntimeEvent::TodoStateChanged { title, .. } => format!("Todo 更新：{title}"),
        RuntimeEvent::WorkerQueued { title, .. } => format!("队列更新：{title}"),
        RuntimeEvent::WorkerBlocked { title, .. } => format!("阻塞：{title}"),
        RuntimeEvent::WorkerRequeued { agent_id, .. } => format!("重新入队：{agent_id}"),
        RuntimeEvent::WorkerDispatched { .. } => "子任务开始".to_string(),
        RuntimeEvent::WorkerUpdate { .. } => "子任务推进".to_string(),
        RuntimeEvent::WorkerOutput { stream, .. } => match stream.as_str() {
            "stderr" => "错误流输出".to_string(),
            _ => "标准流输出".to_string(),
        },
        RuntimeEvent::HandoffReady { .. } => "阶段产物已生成".to_string(),
        RuntimeEvent::MemoryViewReady { .. } => "上下文整理完成".to_string(),
        RuntimeEvent::WorkerFinished { result } => format!("子任务完成：{}", result.task_title),
        RuntimeEvent::ApplyPlanReady { .. } => "落地计划就绪".to_string(),
        RuntimeEvent::ReviewGateReady { .. } => "审阅结论".to_string(),
        RuntimeEvent::ApplyUpdate { .. } => "落地更新".to_string(),
        RuntimeEvent::VerificationReady { stage, .. } => format!("验证完成：{stage}"),
        RuntimeEvent::MemoryUpdated { reason, .. } => format!("记忆更新：{reason}"),
        RuntimeEvent::SummaryReady { .. } => "交付摘要完成".to_string(),
    }
}

fn timeline_detail(event: &RuntimeEvent) -> String {
    match event {
        RuntimeEvent::BrainStarted { state } => {
            format!("{} / {}", state.status, truncate(&state.objective, 96))
        }
        RuntimeEvent::BrainThought { thought } => truncate(thought, 120),
        RuntimeEvent::BrainDecisionMade { decision } => format!(
            "{} / {}",
            decision.action.label(),
            truncate(&decision.rationale, 96)
        ),
        RuntimeEvent::BrainEscalationRaised { message } => truncate(message, 120),
        RuntimeEvent::SchedulerSnapshotUpdated { snapshot } => format!(
            "ready {} / running {} / 阻塞(依赖{} 角色{} 上游{}) / 关键路径 {}",
            snapshot.ready_count,
            snapshot.running_count,
            snapshot.blocked_dependency_count,
            snapshot.blocked_role_limit_count,
            snapshot.blocked_upstream_failed_count,
            snapshot.critical_path_remaining
        ),
        RuntimeEvent::PhaseChanged { phase } => phase.clone(),
        RuntimeEvent::CommanderNote { message } => truncate(message, 120),
        RuntimeEvent::GraphReady {
            nodes,
            dependencies,
        } => format!("共 {nodes} 个执行节点 / {dependencies} 条依赖"),
        RuntimeEvent::TodoStateChanged {
            title,
            status,
            message,
            ..
        } => format!("{title} / {} / {}", status.label(), truncate(message, 96)),
        RuntimeEvent::WorkerQueued {
            role,
            lane,
            todo_id,
            ..
        } => format!(
            "{} / 队列位次 {} / {}",
            role,
            lane,
            todo_id.as_deref().unwrap_or("未映射 todo")
        ),
        RuntimeEvent::WorkerBlocked { reason, .. } => {
            format!("{} / {}", reason.label(), truncate(&reason.detail, 96))
        }
        RuntimeEvent::WorkerRequeued { reason, .. } => truncate(reason, 120),
        RuntimeEvent::WorkerDispatched { role, title, .. } => format!("{role} / {title}"),
        RuntimeEvent::WorkerUpdate { message, .. } => truncate(&message.replace('\n', " "), 120),
        RuntimeEvent::WorkerOutput {
            agent_id,
            stream,
            message,
        } => format!(
            "{} / {} / {}",
            agent_id,
            stream,
            truncate(&message.replace('\n', " "), 120)
        ),
        RuntimeEvent::HandoffReady { .. } => "已生成交接稿，等待整合。".to_string(),
        RuntimeEvent::MemoryViewReady { entries, .. } => {
            format!("已整理 {} 条共享上下文。", entries)
        }
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
        RuntimeEvent::ApplyUpdate { message } => truncate(message, 120),
        RuntimeEvent::VerificationReady {
            success, message, ..
        } => format!(
            "{} / {}",
            if *success { "成功" } else { "失败" },
            truncate(message, 120)
        ),
        RuntimeEvent::MemoryUpdated { scope, entries, .. } => format!("{scope} / {} 条", entries),
        RuntimeEvent::SummaryReady { summary } => summary.overview.clone(),
    }
}

fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.replace('\n', " ");
    if trimmed.chars().count() <= max {
        trimmed
    } else {
        format!("{}…", trimmed.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::SessionContext;
    use crate::model::{
        ApplyMode, ApplyStatus, FinalSummary, PlanTodo, PlanTodoItem, RepoSnapshot, ResultStatus,
        SessionConfig, ThinkingMode, TodoStateRecord, TrustLevel, UiMode,
    };
    use tempfile::TempDir;

    fn sample_config(root: &std::path::Path) -> SessionConfig {
        SessionConfig {
            task: "优化 TUI 体验".to_string(),
            workers: 2,
            role_set: "default".to_string(),
            model: None,
            thinking_mode: ThinkingMode::Balanced,
            ui_mode: UiMode::Minimal,
            target_dir: root.to_path_buf(),
            cleanup_success: false,
            apply_mode: ApplyMode::Bundle,
            max_retries: 1,
            fail_fast: false,
            verification_commands: Vec::new(),
            config_path: None,
            global_rule_prompt: String::new(),
            reviewer_rule_prompt: None,
            plan_only: false,
            preset: None,
            source_plan_session_id: None,
            resume_session_id: None,
            continuation: None,
        }
    }

    fn sample_repo_snapshot(root: &std::path::Path) -> RepoSnapshot {
        RepoSnapshot {
            repo_root: root.to_path_buf(),
            display_name: "demo".to_string(),
            top_level_entries: vec!["src".to_string()],
            detected_stacks: vec!["rust".to_string()],
            readme_excerpt: None,
        }
    }

    fn sample_plan() -> PlanTodo {
        PlanTodo {
            summary: "先梳理执行方案，再决定是否真正运行。".to_string(),
            approach: "优先展示方案和交付物目录。".to_string(),
            todos: vec![PlanTodoItem {
                id: "todo-1".to_string(),
                title: "重做信息架构".to_string(),
                goal: "让用户先看到方案，再决定执行".to_string(),
                details: vec!["优化 Start 页".to_string()],
                dependencies: Vec::new(),
                completion_criteria: vec!["主路径清晰".to_string()],
            }],
            risks: vec!["旧断言需要同步调整".to_string()],
            used_fallback: false,
            planning_notes: Vec::new(),
            iteration_index: 1,
            source_session_id: None,
            feedback_summary: Vec::new(),
            delta_summary: Vec::new(),
        }
    }

    fn sample_summary() -> FinalSummary {
        FinalSummary {
            overview: "TUI 已改成先看方案、再看执行与交付。".to_string(),
            result_status: ResultStatus::Completed,
            review_gate: None,
            apply_status: ApplyStatus::Bundled,
            trust_level: TrustLevel::High,
            accepted_files: vec!["src/app_shell.rs".to_string()],
            manual_review_files: vec!["src/ui.rs".to_string()],
            rejected_files: Vec::new(),
            verified_capabilities: vec!["cargo test".to_string()],
            blocked_verifications: Vec::new(),
            open_risks: vec!["真实 PTY 文案断言需要同步".to_string()],
            recommended_next_action: vec!["进入目标仓库根目录查看交付物".to_string()],
            todo_states: vec![TodoStateRecord {
                todo_id: "todo-1".to_string(),
                title: "重做信息架构".to_string(),
                status: crate::model::TodoStatus::Verified,
                node_ids: Vec::new(),
                completed_node_ids: Vec::new(),
                commit_hash: None,
                last_message: Some("验证通过".to_string()),
            }],
            used_fallback: false,
            review_report: None,
            evidence_summary: Vec::new(),
            iteration_index: 1,
            based_on_session_id: None,
            feedback_summary: Vec::new(),
            delta_summary: Vec::new(),
            completed_this_iteration: vec!["完成 TUI 主路径重构".to_string()],
            unaccepted_feedback: Vec::new(),
        }
    }

    #[test]
    fn writes_process_deliverables_into_session_dir() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        let mut ctx =
            SessionContext::init(&sample_config(root), sample_repo_snapshot(root)).expect("init");

        ctx.set_plan_todo(sample_plan()).expect("plan export");
        assert!(ctx.manifest.deliverable_plan_path().exists());
        assert!(
            ctx.manifest
                .deliverable_plan_path()
                .starts_with(ctx.manifest.session_dir.join("deliverables"))
        );
        assert!(!root.join("codex-forge-plan.md").exists());

        ctx.set_summary(sample_summary()).expect("summary export");
        assert!(ctx.manifest.deliverable_summary_path().exists());
        assert!(ctx.manifest.deliverable_changes_path().exists());
        assert!(ctx.manifest.deliverable_verify_path().exists());
        assert!(!root.join("codex-forge-summary.md").exists());
        assert!(!root.join("codex-forge-changes.md").exists());
        assert!(!root.join("codex-forge-verify.md").exists());
    }
}
