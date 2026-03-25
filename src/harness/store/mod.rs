mod artifacts;
mod ids;
mod jsonl;
mod runs;
mod threads;

use std::path::{Path, PathBuf};

pub use ids::make_id;

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

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::harness::types::{ApprovalStatus, HarnessMessageRole, ToolCallRequest};
    use crate::model::ThinkingMode;

    use super::HarnessStore;

    #[test]
    fn thread_run_and_approval_roundtrip() {
        let dir = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(dir.path());
        let thread = store
            .create_thread(Some("测试线程"))
            .expect("create thread");
        store
            .append_message(
                &thread.id,
                HarnessMessageRole::User,
                "你好".to_string(),
                None,
            )
            .expect("append message");
        let run = store
            .create_run(
                &thread.id,
                Some("gpt-5".to_string()),
                ThinkingMode::Balanced,
            )
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
