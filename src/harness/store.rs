use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::model::ThinkingMode;

use super::types::{
    AgentBackendKind, ApprovalRecord, ApprovalStatus, ArtifactKind, ArtifactRecord, HarnessEvent,
    HarnessEventRecord, HarnessMessage, HarnessMessageRole, HarnessRunManifest, HarnessRunStatus,
    HarnessThreadManifest, SubagentKind, SubagentRecord, ToolCallRecord, ToolCallRequest,
    ToolCallStatus,
};

#[derive(Debug, Clone)]
pub struct HarnessStore {
    repo_root: PathBuf,
}

impl HarnessStore {
    pub fn new(repo_root: &Path) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
        }
    }

    pub fn create_thread(&self, title: Option<&str>) -> Result<HarnessThreadManifest> {
        let display_name = self
            .repo_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
            .to_string();
        let id = make_id("thread");
        let thread_dir = self.threads_dir().join(&id);
        let runs_dir = thread_dir.join("runs");
        let approvals_dir = thread_dir.join("approvals");
        let artifacts_dir = thread_dir.join("artifacts");
        let memory_dir = thread_dir.join("memory");
        let messages_path = thread_dir.join("messages.jsonl");
        fs::create_dir_all(&runs_dir)
            .with_context(|| format!("创建 thread 目录失败：{}", thread_dir.display()))?;
        fs::create_dir_all(&approvals_dir)
            .with_context(|| format!("创建 approvals 目录失败：{}", approvals_dir.display()))?;
        fs::create_dir_all(&artifacts_dir)
            .with_context(|| format!("创建 artifacts 目录失败：{}", artifacts_dir.display()))?;
        fs::create_dir_all(&memory_dir)
            .with_context(|| format!("创建 memory 目录失败：{}", memory_dir.display()))?;
        fs::write(&messages_path, "")
            .with_context(|| format!("初始化消息文件失败：{}", messages_path.display()))?;

        let now = Utc::now();
        let manifest = HarnessThreadManifest {
            id: id.clone(),
            title: title
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("{} 主线程", display_name)),
            repo_root: self.repo_root.clone(),
            created_at: now,
            updated_at: now,
            message_count: 0,
            run_count: 0,
            last_run_id: None,
            last_run_status: None,
            thread_dir,
            messages_path,
            runs_dir,
            approvals_dir,
            artifacts_dir,
            memory_dir,
        };
        self.persist_thread(&manifest)?;
        Ok(manifest)
    }

    pub fn list_threads(&self) -> Result<Vec<HarnessThreadManifest>> {
        let mut threads = Vec::new();
        if !self.threads_dir().exists() {
            return Ok(threads);
        }

        for entry in fs::read_dir(self.threads_dir())
            .with_context(|| format!("读取 thread 目录失败：{}", self.threads_dir().display()))?
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let manifest_path = entry.path().join("thread.json");
            if !manifest_path.exists() {
                continue;
            }
            let raw = match fs::read_to_string(&manifest_path) {
                Ok(raw) => raw,
                Err(_) => continue,
            };
            let Ok(manifest) = serde_json::from_str::<HarnessThreadManifest>(&raw) else {
                continue;
            };
            threads.push(manifest);
        }

        threads.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(threads)
    }

    pub fn load_thread(&self, thread_id: &str) -> Result<HarnessThreadManifest> {
        let path = self.thread_manifest_path(thread_id);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("读取 thread manifest 失败：{}", path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("解析 thread manifest 失败：{}", path.display()))
    }

    pub fn append_message(
        &self,
        thread_id: &str,
        role: HarnessMessageRole,
        content: String,
        run_id: Option<String>,
    ) -> Result<HarnessMessage> {
        let mut thread = self.load_thread(thread_id)?;
        let message = HarnessMessage {
            id: make_id("msg"),
            role,
            content,
            created_at: Utc::now(),
            run_id,
        };
        append_jsonl(&thread.messages_path, &message)?;
        thread.message_count += 1;
        thread.updated_at = Utc::now();
        self.persist_thread(&thread)?;
        self.append_runless_event(
            thread_id,
            HarnessEvent::MessageAppended {
                thread_id: thread_id.to_string(),
                message_id: message.id.clone(),
                role: message.role,
            },
        )?;
        Ok(message)
    }

    pub fn list_messages(&self, thread_id: &str) -> Result<Vec<HarnessMessage>> {
        let thread = self.load_thread(thread_id)?;
        read_jsonl(&thread.messages_path)
    }

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
            run_dir: run_dir.clone(),
            events_path: run_dir.join("events.jsonl"),
            output_path: run_dir.join("assistant.md"),
            log_path: run_dir.join("codex.log"),
            tool_calls_path: run_dir.join("tool-calls.jsonl"),
            approvals_path: run_dir.join("approvals.jsonl"),
            artifacts_path: run_dir.join("artifacts.jsonl"),
            subagents_path: run_dir.join("subagents.jsonl"),
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
        serde_json::from_str(&raw)
            .with_context(|| format!("解析 run manifest 失败：{}", path.display()))
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
            let Ok(manifest) = serde_json::from_str::<HarnessRunManifest>(&raw) else {
                continue;
            };
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

    pub fn append_tool_call(&self, run: &HarnessRunManifest, call: &ToolCallRequest) -> Result<ToolCallRecord> {
        let record = ToolCallRecord {
            id: make_id("tool"),
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
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

    pub fn update_tool_call(&self, run: &HarnessRunManifest, record: &ToolCallRecord) -> Result<()> {
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

    pub fn append_artifact(
        &self,
        thread_id: &str,
        run_id: &str,
        label: String,
        kind: ArtifactKind,
        path: PathBuf,
    ) -> Result<ArtifactRecord> {
        let run = self.load_run(thread_id, run_id)?;
        let thread = self.load_thread(thread_id)?;
        let artifact = ArtifactRecord {
            id: make_id("artifact"),
            thread_id: thread_id.to_string(),
            run_id: run_id.to_string(),
            label,
            kind,
            path,
            created_at: Utc::now(),
        };
        append_jsonl(&run.artifacts_path, &artifact)?;
        append_jsonl(&thread.artifacts_dir.join("index.jsonl"), &artifact)?;
        Ok(artifact)
    }

    pub fn list_artifacts(
        &self,
        thread_id: Option<&str>,
        run_id: Option<&str>,
    ) -> Result<Vec<ArtifactRecord>> {
        if let (Some(thread_id), Some(run_id)) = (thread_id, run_id) {
            let run = self.load_run(thread_id, run_id)?;
            let mut artifacts: Vec<ArtifactRecord> = read_jsonl(&run.artifacts_path)?;
            artifacts.sort_by(|left, right| right.created_at.cmp(&left.created_at));
            return Ok(artifacts);
        }
        if let Some(thread_id) = thread_id {
            let thread = self.load_thread(thread_id)?;
            let mut artifacts: Vec<ArtifactRecord> = read_jsonl(&thread.artifacts_dir.join("index.jsonl"))?;
            artifacts.sort_by(|left, right| right.created_at.cmp(&left.created_at));
            return Ok(artifacts);
        }

        let mut artifacts = Vec::new();
        for thread in self.list_threads()? {
            artifacts.extend(self.list_artifacts(Some(&thread.id), None)?);
        }
        artifacts.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        Ok(artifacts)
    }

    pub fn append_subagent(
        &self,
        run: &HarnessRunManifest,
        kind: SubagentKind,
        task: String,
    ) -> Result<SubagentRecord> {
        let record = SubagentRecord {
            id: make_id("subagent"),
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            kind,
            task,
            status: HarnessRunStatus::Running,
            created_at: Utc::now(),
            updated_at: Utc::now(),
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

    fn append_runless_event(&self, thread_id: &str, event: HarnessEvent) -> Result<()> {
        let thread = self.load_thread(thread_id)?;
        let path = thread.thread_dir.join("thread-events.jsonl");
        let record = HarnessEventRecord {
            at: Utc::now(),
            payload: event,
        };
        append_jsonl(&path, &record)
    }

    fn persist_thread(&self, thread: &HarnessThreadManifest) -> Result<()> {
        let path = self.thread_manifest_path(&thread.id);
        ensure_parent(&path)?;
        fs::write(
            &path,
            serde_json::to_vec_pretty(thread).context("序列化 thread manifest 失败")?,
        )
        .with_context(|| format!("写入 thread manifest 失败：{}", path.display()))
    }

    fn persist_run(&self, thread_id: &str, run: &HarnessRunManifest) -> Result<()> {
        let path = self.run_manifest_path(thread_id, &run.id);
        ensure_parent(&path)?;
        fs::write(
            &path,
            serde_json::to_vec_pretty(run).context("序列化 run manifest 失败")?,
        )
        .with_context(|| format!("写入 run manifest 失败：{}", path.display()))
    }

    fn threads_dir(&self) -> PathBuf {
        self.repo_root.join(".codex-forge").join("threads")
    }

    fn thread_manifest_path(&self, thread_id: &str) -> PathBuf {
        self.threads_dir().join(thread_id).join("thread.json")
    }

    fn run_manifest_path(&self, thread_id: &str, run_id: &str) -> PathBuf {
        self.threads_dir()
            .join(thread_id)
            .join("runs")
            .join(run_id)
            .join("run.json")
    }
}

fn append_jsonl<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    ensure_parent(path)?;
    let payload = serde_json::to_string(value).context("序列化 JSONL 记录失败")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("打开 JSONL 文件失败：{}", path.display()))?;
    writeln!(file, "{payload}")
        .with_context(|| format!("写入 JSONL 文件失败：{}", path.display()))
}

fn overwrite_jsonl<T: serde::Serialize>(path: &Path, values: &[T]) -> Result<()> {
    ensure_parent(path)?;
    let payload = if values.is_empty() {
        String::new()
    } else {
        let mut lines = values
            .iter()
            .map(|item| serde_json::to_string(item).context("序列化 JSONL 记录失败"))
            .collect::<Result<Vec<_>>>()?
            .join("\n");
        lines.push('\n');
        lines
    };
    fs::write(path, payload).with_context(|| format!("覆盖 JSONL 文件失败：{}", path.display()))
}

fn rewrite_jsonl<T, F>(path: &Path, rewrite: F) -> Result<()>
where
    T: serde::Serialize + for<'de> serde::Deserialize<'de>,
    F: FnOnce(&mut Vec<T>),
{
    let mut items: Vec<T> = read_jsonl(path)?;
    rewrite(&mut items);
    overwrite_jsonl(path, &items)
}

fn read_jsonl<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("读取 JSONL 文件失败：{}", path.display()))?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<T>(line).context("解析 JSONL 记录失败"))
        .collect()
}

fn replace_by_id<T>(items: &mut [T], id: &str, updated: T)
where
    T: RecordId,
{
    if let Some(index) = items.iter().position(|item| item.record_id() == id) {
        items[index] = updated;
    }
}

trait RecordId {
    fn record_id(&self) -> &str;
}

impl RecordId for ToolCallRecord {
    fn record_id(&self) -> &str {
        &self.id
    }
}

impl RecordId for ApprovalRecord {
    fn record_id(&self) -> &str {
        &self.id
    }
}

impl RecordId for SubagentRecord {
    fn record_id(&self) -> &str {
        &self.id
    }
}

fn ensure_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        bail!("路径缺少父目录：{}", path.display());
    };
    fs::create_dir_all(parent).with_context(|| format!("创建父目录失败：{}", parent.display()))
}

pub fn make_id(prefix: &str) -> String {
    format!(
        "{}-{}-{}",
        prefix,
        Utc::now().format("%Y%m%d-%H%M%S-%3f"),
        std::process::id()
    )
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn thread_run_and_approval_roundtrip() {
        let dir = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(dir.path());
        let thread = store.create_thread(Some("测试线程")).expect("create thread");
        store
            .append_message(&thread.id, HarnessMessageRole::User, "你好".to_string(), None)
            .expect("append message");
        let run = store
            .create_run(&thread.id, Some("gpt-5".to_string()), ThinkingMode::Balanced)
            .expect("create run");
        let tool = store
            .append_tool_call(
                &run,
                &ToolCallRequest {
                    name: "write_file".to_string(),
                    arguments: serde_json::json!({"path":"a.txt","content":"hi"}),
                },
            )
            .expect("append tool");
        let approval = store
            .append_approval(
                &thread,
                &run,
                &tool,
                "写文件需要确认".to_string(),
                ToolCallRequest {
                    name: "write_file".to_string(),
                    arguments: serde_json::json!({"path":"a.txt","content":"hi"}),
                },
            )
            .expect("append approval");
        assert_eq!(approval.status, ApprovalStatus::Pending);
        let pending = store
            .list_pending_approvals(Some(&thread.id))
            .expect("pending");
        assert_eq!(pending.len(), 1);
        let resolved = store
            .resolve_approval(&thread.id, &approval.id, ApprovalStatus::Approved)
            .expect("resolve");
        assert_eq!(resolved.status, ApprovalStatus::Approved);
    }
}
