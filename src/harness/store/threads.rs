use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};

use crate::harness::types::{
    HarnessEvent, HarnessEventRecord, HarnessMessage, HarnessMessageRole, HarnessThreadManifest,
};

use super::HarnessStore;
use super::ids::make_id;
use super::jsonl::{append_jsonl, read_jsonl, write_atomic};

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
            let thread_id = entry.file_name().to_string_lossy().to_string();
            let Ok(manifest) = self.read_or_repair_thread_manifest(&thread_id, &manifest_path)
            else {
                continue;
            };
            threads.push(manifest);
        }

        threads.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(threads)
    }

    pub fn load_thread(&self, thread_id: &str) -> Result<HarnessThreadManifest> {
        let path = self.thread_manifest_path(thread_id);
        self.read_or_repair_thread_manifest(thread_id, &path)
    }

    pub fn delete_thread(&self, thread_id: &str) -> Result<()> {
        let thread = self.load_thread(thread_id)?;
        fs::remove_dir_all(&thread.thread_dir)
            .with_context(|| format!("删除 thread 目录失败：{}", thread.thread_dir.display()))
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
        write_atomic(
            &path,
            &serde_json::to_vec_pretty(thread).context("序列化 thread manifest 失败")?,
        )
        .with_context(|| format!("写入 thread manifest 失败：{}", path.display()))
    }

    fn read_or_repair_thread_manifest(
        &self,
        thread_id: &str,
        path: &Path,
    ) -> Result<HarnessThreadManifest> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("读取 thread manifest 失败：{}", path.display()))?;
        if raw.trim().is_empty() {
            return self.rebuild_thread_manifest(thread_id, path);
        }
        match serde_json::from_str::<HarnessThreadManifest>(&raw) {
            Ok(thread) => Ok(normalize_thread_manifest(thread)),
            Err(error) if error.is_eof() => self.rebuild_thread_manifest(thread_id, path),
            Err(error) => {
                Err(error).with_context(|| format!("解析 thread manifest 失败：{}", path.display()))
            }
        }
    }

    fn rebuild_thread_manifest(
        &self,
        thread_id: &str,
        path: &Path,
    ) -> Result<HarnessThreadManifest> {
        let thread_dir = path
            .parent()
            .ok_or_else(|| anyhow!("thread manifest 缺少父目录：{}", path.display()))?
            .to_path_buf();
        let messages_path = thread_dir.join("messages.jsonl");
        let runs_dir = thread_dir.join("runs");
        let approvals_dir = thread_dir.join("approvals");
        let artifacts_dir = thread_dir.join("artifacts");
        let memory_dir = thread_dir.join("memory");
        let updated_at = infer_timestamp(path, &thread_dir);
        let manifest = HarnessThreadManifest {
            id: thread_id.to_string(),
            title: default_thread_title(&self.repo_root),
            repo_root: self.repo_root.clone(),
            created_at: updated_at,
            updated_at,
            message_count: count_jsonl_records(&messages_path),
            run_count: count_run_dirs(&runs_dir),
            last_run_id: None,
            last_run_status: None,
            thread_dir,
            messages_path,
            runs_dir,
            approvals_dir,
            artifacts_dir,
            memory_dir,
            contract_path: path
                .parent()
                .ok_or_else(|| anyhow!("thread manifest 缺少父目录：{}", path.display()))?
                .join("contract.json"),
            progress_path: path
                .parent()
                .ok_or_else(|| anyhow!("thread manifest 缺少父目录：{}", path.display()))?
                .join("progress.json"),
            bootstrap_path: path
                .parent()
                .ok_or_else(|| anyhow!("thread manifest 缺少父目录：{}", path.display()))?
                .join("session-bootstrap.md"),
        };
        self.persist_thread(&manifest)?;
        Ok(manifest)
    }
}

fn normalize_thread_manifest(mut thread: HarnessThreadManifest) -> HarnessThreadManifest {
    if thread.contract_path.as_os_str().is_empty() {
        thread.contract_path = thread.thread_dir.join("contract.json");
    }
    if thread.progress_path.as_os_str().is_empty() {
        thread.progress_path = thread.thread_dir.join("progress.json");
    }
    if thread.bootstrap_path.as_os_str().is_empty() {
        thread.bootstrap_path = thread.thread_dir.join("session-bootstrap.md");
    }
    thread
}

fn default_thread_title(repo_root: &Path) -> String {
    let display_name = repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");
    format!("{display_name} 主线程")
}

fn count_jsonl_records(path: &Path) -> usize {
    fs::read_to_string(path)
        .map(|raw| raw.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0)
}

fn count_run_dirs(path: &Path) -> usize {
    fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().join("run.json").exists())
                .count()
        })
        .unwrap_or(0)
}

fn infer_timestamp(manifest_path: &Path, thread_dir: &Path) -> DateTime<Utc> {
    fs::metadata(manifest_path)
        .and_then(|meta| meta.modified())
        .or_else(|_| fs::metadata(thread_dir).and_then(|meta| meta.modified()))
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now())
}
