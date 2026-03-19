use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::model::{
    ApplyMode, ApplyResult, ArtifactEntry, ArtifactIndexEntry, ArtifactManifest,
    ChangeTrustReport, ExecutionContract, ExecutionGraph, FinalSummary, PlanTodo, RepoSnapshot,
    RuntimeEvent, RuntimeEventRecord, SessionConfig, SessionManifest, SessionStatus,
    TimelineEventSummary, TodoStateRecord, TodoStatus, VerificationReport, WorkerResult,
};

#[derive(Debug, Clone)]
pub struct SessionContext {
    pub manifest: SessionManifest,
    manifest_path: PathBuf,
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

        let manifest = SessionManifest {
            id: session_id,
            task: config.task.clone(),
            repo_snapshot,
            created_at: Utc::now(),
            status: SessionStatus::Planning,
            ui_mode: config.ui_mode,
            workers_requested: config.workers,
            role_set: config.role_set.clone(),
            model: config.model.clone(),
            cleanup_success: config.cleanup_success,
            apply_mode: config.apply_mode,
            max_retries: config.max_retries,
            fail_fast: config.fail_fast,
            verification_commands: config.verification_commands.clone(),
            config_path: config.config_path.clone(),
            preset: config.preset,
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
        self.refresh_indexes();
        fs::write(
            &self.manifest_path,
            serde_json::to_vec_pretty(&self.manifest).context("序列化 manifest 失败")?,
        )
        .with_context(|| format!("写入 manifest 失败：{}", self.manifest_path.display()))
    }

    pub fn persist_artifact_manifest(&mut self) -> Result<()> {
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
    serde_json::from_str(&content).context("解析 manifest 失败")
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
            format!("**放行范围**\n\n{}\n\n**拦截范围**\n\n{}", accepted, rejected)
        })
        .unwrap_or_else(|| "**放行范围**\n\n- 无\n\n**拦截范围**\n\n- 无".to_string());

    format!(
        "# Session {}\n\n\
## 任务\n\n{}\n\n\
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
## 审阅关卡\n\n{}\n\n{}\n\n\
## 证据摘要\n\n{}\n\n\
## Demo 摘要\n\n{}\n\n\
## 未关闭风险\n\n{}\n\n\
## 下一步\n\n{}\n",
        manifest.id,
        manifest.task,
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
        review_findings,
        review_scopes,
        evidence_summary,
        demo_summary,
        open_risks,
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

    format!(
        "# 计划清单\n\n\
## 摘要\n\n{}\n\n\
## 推进策略\n\n{}\n\n\
## Todo\n\n{}\n\
## 风险\n\n{}\n\n\
## 规划备注\n\n{}\n",
        plan_todo.summary, plan_todo.approach, todo_lines, risks, notes
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
    items
}

fn build_demo_summary(manifest: &SessionManifest, summary: &FinalSummary) -> Vec<String> {
    let mut lines = Vec::new();
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
        RuntimeEvent::HandoffReady {
            handoff_path, ..
        } => handoff_path.display().to_string(),
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
