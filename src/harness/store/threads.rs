use std::fs;

use anyhow::{Context, Result};
use chrono::Utc;

use crate::harness::types::{
    HarnessEvent, HarnessEventRecord, HarnessMessage, HarnessMessageRole, HarnessThreadManifest,
};

use super::HarnessStore;
use super::ids::make_id;
use super::jsonl::{append_jsonl, read_jsonl};

impl HarnessStore {
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
        let contract_path = thread_dir.join("contract.json");
        let progress_path = thread_dir.join("progress.json");
        let bootstrap_path = thread_dir.join("session-bootstrap.md");
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
            contract_path,
            progress_path,
            bootstrap_path,
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
            let Ok(mut manifest) = serde_json::from_str::<HarnessThreadManifest>(&raw) else {
                continue;
            };
            if manifest.contract_path.as_os_str().is_empty() {
                manifest.contract_path = manifest.thread_dir.join("contract.json");
            }
            if manifest.progress_path.as_os_str().is_empty() {
                manifest.progress_path = manifest.thread_dir.join("progress.json");
            }
            if manifest.bootstrap_path.as_os_str().is_empty() {
                manifest.bootstrap_path = manifest.thread_dir.join("session-bootstrap.md");
            }
            threads.push(manifest);
        }

        threads.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(threads)
    }

    pub fn load_thread(&self, thread_id: &str) -> Result<HarnessThreadManifest> {
        let path = self.thread_manifest_path(thread_id);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("读取 thread manifest 失败：{}", path.display()))?;
        let mut thread: HarnessThreadManifest = serde_json::from_str(&raw)
            .with_context(|| format!("解析 thread manifest 失败：{}", path.display()))?;
        if thread.contract_path.as_os_str().is_empty() {
            thread.contract_path = thread.thread_dir.join("contract.json");
        }
        if thread.progress_path.as_os_str().is_empty() {
            thread.progress_path = thread.thread_dir.join("progress.json");
        }
        if thread.bootstrap_path.as_os_str().is_empty() {
            thread.bootstrap_path = thread.thread_dir.join("session-bootstrap.md");
        }
        Ok(thread)
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

    fn append_runless_event(&self, thread_id: &str, event: HarnessEvent) -> Result<()> {
        let thread = self.load_thread(thread_id)?;
        let path = thread.thread_dir.join("thread-events.jsonl");
        let record = HarnessEventRecord {
            at: Utc::now(),
            payload: event,
        };
        append_jsonl(&path, &record)
    }

    pub(super) fn persist_thread(&self, thread: &HarnessThreadManifest) -> Result<()> {
        let path = self.thread_manifest_path(&thread.id);
        fs::write(
            &path,
            serde_json::to_vec_pretty(thread).context("序列化 thread manifest 失败")?,
        )
        .with_context(|| format!("写入 thread manifest 失败：{}", path.display()))
    }
}
