use std::fs;

use anyhow::{Context, Result};
use chrono::Utc;

use crate::harness::types::{
    AgentBackendKind, ApprovalRecord, ApprovalStatus, HarnessEvent, HarnessEventRecord,
    HarnessRunManifest, HarnessRunStatus, HarnessThreadManifest, SubagentKind, SubagentRecord,
    TaskGraphManifest, TaskGraphStrategy, TaskNodeKind, TaskNodeRecord, TaskNodeStatus,
    ToolCallRecord, ToolCallRequest, ToolCallStatus,
};
use crate::model::ThinkingMode;

use super::HarnessStore;
use super::ids::make_id;
use super::jsonl::{append_jsonl, overwrite_jsonl, read_jsonl, replace_by_id, rewrite_jsonl};

impl HarnessStore {
    pub fn create_run(
        &self,
        thread_id: &str,
        model: Option<String>,
        thinking_mode: ThinkingMode,
    ) -> Result<HarnessRunManifest> {
        let mut thread = self.load_thread(thread_id)?;
        let id = make_id("run");
        let run_dir = thread.runs_dir.join(&id);
        fs::create_dir_all(&run_dir)
            .with_context(|| format!("创建 run 目录失败：{}", run_dir.display()))?;
        let now = Utc::now();
        let run = HarnessRunManifest {
            id: id.clone(),
            thread_id: thread_id.to_string(),
            status: HarnessRunStatus::Pending,
            created_at: now,
            updated_at: now,
            model,
            thinking_mode,
            backend: AgentBackendKind::Codex,
            turn_count: 0,
            summary: None,
            last_error: None,
            blocked_reason: None,
            run_dir: run_dir.clone(),
            events_path: run_dir.join("events.jsonl"),
            output_path: run_dir.join("assistant.md"),
            log_path: run_dir.join("codex.log"),
            tool_calls_path: run_dir.join("tool-calls.jsonl"),
            approvals_path: run_dir.join("approvals.jsonl"),
            artifacts_path: run_dir.join("artifacts.jsonl"),
            subagents_path: run_dir.join("subagents.jsonl"),
            task_graph_path: run_dir.join("task-graph.json"),
            task_nodes_path: run_dir.join("task-nodes.jsonl"),
            evaluation_log_path: run_dir.join("evaluations.jsonl"),
            bootstrap_path: run_dir.join("session-bootstrap.md"),
            active_task_node_id: None,
            sandbox: None,
        };
        self.persist_run(thread_id, &run)?;
        thread.run_count += 1;
        thread.last_run_id = Some(id);
        thread.last_run_status = Some(run.status);
        thread.updated_at = Utc::now();
        self.persist_thread(&thread)?;
        Ok(run)
    }

    pub fn update_run(&self, thread_id: &str, run: &HarnessRunManifest) -> Result<()> {
        let mut thread = self.load_thread(thread_id)?;
        let mut updated = run.clone();
        updated.updated_at = Utc::now();
        self.persist_run(thread_id, &updated)?;
        thread.last_run_id = Some(updated.id.clone());
        thread.last_run_status = Some(updated.status);
        thread.updated_at = updated.updated_at;
        self.persist_thread(&thread)
    }

    pub fn load_run(&self, thread_id: &str, run_id: &str) -> Result<HarnessRunManifest> {
        let path = self.run_manifest_path(thread_id, run_id);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("读取 run manifest 失败：{}", path.display()))?;
        let mut run: HarnessRunManifest = serde_json::from_str(&raw)
            .with_context(|| format!("解析 run manifest 失败：{}", path.display()))?;
        if run.evaluation_log_path.as_os_str().is_empty() {
            run.evaluation_log_path = run.run_dir.join("evaluations.jsonl");
        }
        if run.bootstrap_path.as_os_str().is_empty() {
            run.bootstrap_path = run.run_dir.join("session-bootstrap.md");
        }
        Ok(run)
    }

    pub fn list_runs(&self, thread_id: &str) -> Result<Vec<HarnessRunManifest>> {
        let thread = self.load_thread(thread_id)?;
        if !thread.runs_dir.exists() {
            return Ok(Vec::new());
        }

        let mut runs = Vec::new();
        for entry in fs::read_dir(&thread.runs_dir)
            .with_context(|| format!("读取 run 目录失败：{}", thread.runs_dir.display()))?
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let manifest_path = entry.path().join("run.json");
            if !manifest_path.exists() {
                continue;
            }
            let raw = match fs::read_to_string(&manifest_path) {
                Ok(raw) => raw,
                Err(_) => continue,
            };
            let Ok(mut manifest) = serde_json::from_str::<HarnessRunManifest>(&raw) else {
                continue;
            };
            if manifest.evaluation_log_path.as_os_str().is_empty() {
                manifest.evaluation_log_path = manifest.run_dir.join("evaluations.jsonl");
            }
            if manifest.bootstrap_path.as_os_str().is_empty() {
                manifest.bootstrap_path = manifest.run_dir.join("session-bootstrap.md");
            }
            runs.push(manifest);
        }
        runs.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        Ok(runs)
    }

    pub fn append_run_event(
        &self,
        thread_id: &str,
        run_id: &str,
        event: HarnessEvent,
    ) -> Result<()> {
        let run = self.load_run(thread_id, run_id)?;
        let record = HarnessEventRecord {
            at: Utc::now(),
            payload: event,
        };
        append_jsonl(&run.events_path, &record)
    }

    pub fn list_run_events(
        &self,
        thread_id: &str,
        run_id: &str,
    ) -> Result<Vec<HarnessEventRecord>> {
        let run = self.load_run(thread_id, run_id)?;
        read_jsonl(&run.events_path)
    }

    pub fn append_tool_call(
        &self,
        run: &HarnessRunManifest,
        call: &ToolCallRequest,
        task_node_id: Option<String>,
        subagent_id: Option<String>,
    ) -> Result<ToolCallRecord> {
        let record = ToolCallRecord {
            id: make_id("tool"),
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            task_node_id,
            subagent_id,
            status: ToolCallStatus::Pending,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            approval_id: None,
            output_summary: None,
            error: None,
        };
        append_jsonl(&run.tool_calls_path, &record)?;
        Ok(record)
    }

    pub fn list_tool_calls(&self, run: &HarnessRunManifest) -> Result<Vec<ToolCallRecord>> {
        read_jsonl(&run.tool_calls_path)
    }

    pub fn update_tool_call(
        &self,
        run: &HarnessRunManifest,
        record: &ToolCallRecord,
    ) -> Result<()> {
        rewrite_jsonl(&run.tool_calls_path, |items: &mut Vec<ToolCallRecord>| {
            replace_by_id(items, &record.id, record.clone())
        })
    }

    pub fn append_approval(
        &self,
        thread: &HarnessThreadManifest,
        run: &HarnessRunManifest,
        tool: &ToolCallRecord,
        reason: String,
        tool_call: ToolCallRequest,
    ) -> Result<ApprovalRecord> {
        let approval = ApprovalRecord {
            id: make_id("approval"),
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            tool_call_id: tool.id.clone(),
            task_node_id: tool.task_node_id.clone(),
            tool_name: tool.name.clone(),
            reason,
            status: ApprovalStatus::Pending,
            created_at: Utc::now(),
            resolved_at: None,
            tool_call,
        };
        append_jsonl(&run.approvals_path, &approval)?;
        append_jsonl(&thread.approvals_dir.join("pending.jsonl"), &approval)?;
        Ok(approval)
    }

    pub fn list_pending_approvals(&self, thread_id: Option<&str>) -> Result<Vec<ApprovalRecord>> {
        if let Some(thread_id) = thread_id {
            let thread = self.load_thread(thread_id)?;
            let pending_path = thread.approvals_dir.join("pending.jsonl");
            let mut approvals: Vec<ApprovalRecord> = read_jsonl(&pending_path)?;
            approvals.retain(|item| item.status == ApprovalStatus::Pending);
            approvals.sort_by(|left, right| right.created_at.cmp(&left.created_at));
            return Ok(approvals);
        }

        let mut all = Vec::new();
        for thread in self.list_threads()? {
            all.extend(self.list_pending_approvals(Some(&thread.id))?);
        }
        all.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        Ok(all)
    }

    pub fn resolve_approval(
        &self,
        thread_id: &str,
        approval_id: &str,
        status: ApprovalStatus,
    ) -> Result<ApprovalRecord> {
        let thread = self.load_thread(thread_id)?;
        let pending_path = thread.approvals_dir.join("pending.jsonl");
        let mut approvals: Vec<ApprovalRecord> = read_jsonl(&pending_path)?;
        let index = approvals
            .iter()
            .position(|item| item.id == approval_id)
            .ok_or_else(|| anyhow::anyhow!("未找到待处理 approval：{approval_id}"))?;
        let mut approval = approvals.remove(index);
        approval.status = status;
        approval.resolved_at = Some(Utc::now());
        overwrite_jsonl(&pending_path, &approvals)?;
        append_jsonl(&thread.approvals_dir.join("resolved.jsonl"), &approval)?;
        let run = self.load_run(thread_id, &approval.run_id)?;
        rewrite_jsonl(&run.approvals_path, |items: &mut Vec<ApprovalRecord>| {
            replace_by_id(items, approval_id, approval.clone())
        })?;
        Ok(approval)
    }

    pub fn append_subagent(
        &self,
        run: &HarnessRunManifest,
        kind: SubagentKind,
        task: String,
        task_node_id: Option<String>,
        model: Option<String>,
        thinking_mode: ThinkingMode,
    ) -> Result<SubagentRecord> {
        let id = make_id("subagent");
        let subagent_dir = run.run_dir.join("subagents");
        fs::create_dir_all(&subagent_dir)
            .with_context(|| format!("创建 subagent 目录失败：{}", subagent_dir.display()))?;
        let record = SubagentRecord {
            id: id.clone(),
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            task_node_id,
            kind,
            task,
            model,
            thinking_mode,
            status: HarnessRunStatus::Running,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            output_path: subagent_dir.join(format!("{id}.md")),
            log_path: subagent_dir.join(format!("{id}.log")),
            summary: None,
            error: None,
        };
        append_jsonl(&run.subagents_path, &record)?;
        Ok(record)
    }

    pub fn update_subagent(&self, run: &HarnessRunManifest, record: &SubagentRecord) -> Result<()> {
        rewrite_jsonl(&run.subagents_path, |items: &mut Vec<SubagentRecord>| {
            replace_by_id(items, &record.id, record.clone())
        })
    }

    pub fn list_subagents(&self, run: &HarnessRunManifest) -> Result<Vec<SubagentRecord>> {
        read_jsonl(&run.subagents_path)
    }

    pub fn create_task_graph(
        &self,
        run: &HarnessRunManifest,
        goal: String,
        strategy: TaskGraphStrategy,
        success_criteria: Vec<String>,
    ) -> Result<TaskGraphManifest> {
        let graph = TaskGraphManifest {
            id: make_id("graph"),
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            goal,
            strategy,
            success_criteria,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        fs::write(
            &run.task_graph_path,
            serde_json::to_vec_pretty(&graph).context("序列化 task graph 失败")?,
        )
        .with_context(|| format!("写入 task graph 失败：{}", run.task_graph_path.display()))?;
        Ok(graph)
    }

    pub fn load_task_graph(&self, run: &HarnessRunManifest) -> Result<TaskGraphManifest> {
        let raw = fs::read_to_string(&run.task_graph_path)
            .with_context(|| format!("读取 task graph 失败：{}", run.task_graph_path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("解析 task graph 失败：{}", run.task_graph_path.display()))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_task_node(
        &self,
        run: &HarnessRunManifest,
        graph: &TaskGraphManifest,
        kind: TaskNodeKind,
        title: String,
        instructions: String,
        depends_on: Vec<String>,
        position: usize,
        status: TaskNodeStatus,
    ) -> Result<TaskNodeRecord> {
        let record = TaskNodeRecord {
            id: make_id("task"),
            graph_id: graph.id.clone(),
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            kind,
            title,
            instructions,
            depends_on,
            position,
            status,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            started_at: None,
            completed_at: None,
            output_summary: None,
            error: None,
            last_subagent_id: None,
            attempt_count: 0,
            feature_id: None,
        };
        append_jsonl(&run.task_nodes_path, &record)?;
        Ok(record)
    }

    pub fn list_task_nodes(&self, run: &HarnessRunManifest) -> Result<Vec<TaskNodeRecord>> {
        let mut nodes: Vec<TaskNodeRecord> = read_jsonl(&run.task_nodes_path)?;
        nodes.sort_by(|left, right| {
            left.position
                .cmp(&right.position)
                .then(left.created_at.cmp(&right.created_at))
        });
        Ok(nodes)
    }

    pub fn load_task_node(
        &self,
        run: &HarnessRunManifest,
        task_node_id: &str,
    ) -> Result<TaskNodeRecord> {
        self.list_task_nodes(run)?
            .into_iter()
            .find(|node| node.id == task_node_id)
            .ok_or_else(|| anyhow::anyhow!("未找到 task node：{task_node_id}"))
    }

    pub fn update_task_node(&self, run: &HarnessRunManifest, node: &TaskNodeRecord) -> Result<()> {
        let mut updated = node.clone();
        updated.updated_at = Utc::now();
        rewrite_jsonl(&run.task_nodes_path, |items: &mut Vec<TaskNodeRecord>| {
            replace_by_id(items, &updated.id, updated.clone())
        })
    }

    fn persist_run(&self, thread_id: &str, run: &HarnessRunManifest) -> Result<()> {
        let path = self.run_manifest_path(thread_id, &run.id);
        fs::write(
            &path,
            serde_json::to_vec_pretty(run).context("序列化 run manifest 失败")?,
        )
        .with_context(|| format!("写入 run manifest 失败：{}", path.display()))
    }
}
