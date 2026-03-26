mod artifacts;
mod ids;
mod jsonl;
mod memory;
mod runs;
mod state;
mod threads;

use std::path::{Path, PathBuf};

use crate::config::BackendProvider;

pub use ids::make_id;

#[derive(Debug, Clone)]
pub struct HarnessStore {
    repo_root: PathBuf,
    provider: BackendProvider,
}

impl HarnessStore {
    pub fn new(repo_root: &Path, provider: BackendProvider) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            provider,
        }
    }

    fn store_root(&self) -> PathBuf {
        self.repo_root
            .join(".codex-forge")
            .join("modes")
            .join(self.provider.config_value())
    }

    fn threads_dir(&self) -> PathBuf {
        self.store_root().join("threads")
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
    use std::fs;

    use tempfile::TempDir;

    use crate::config::BackendProvider;
    use crate::harness::types::{
        AcceptanceCriterion, AgentBackendKind, ApprovalStatus, EvaluationDecision,
        ExecutionContract, FeatureSlice, FeatureSliceStatus, HarnessMessageRole, MemoryLayer,
        ProgressLedger, ToolCallRequest,
    };
    use crate::model::ThinkingMode;

    use super::HarnessStore;

    #[test]
    fn thread_run_and_approval_roundtrip() {
        let dir = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(dir.path(), BackendProvider::Codex);
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
                AgentBackendKind::Codex,
            )
            .expect("create run");
        let tool = store
            .append_tool_call(
                &run,
                &ToolCallRequest {
                    name: "write_file".to_string(),
                    arguments: serde_json::json!({"path":"a.txt","content":"hi"}),
                },
                None,
                None,
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

    #[test]
    fn memory_roundtrip_works() {
        let dir = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(dir.path(), BackendProvider::Codex);
        let thread = store.create_thread(Some("记忆")).expect("thread");
        store
            .append_memory_entry(
                &thread.id,
                MemoryLayer::Working,
                "实现路径是 run -> task graph".to_string(),
                "test".to_string(),
                None,
                None,
            )
            .expect("append working");
        store
            .append_memory_entry(
                &thread.id,
                MemoryLayer::Project,
                "项目默认使用中文".to_string(),
                "test".to_string(),
                None,
                None,
            )
            .expect("append project");
        let working = store
            .load_memory(&thread.id, MemoryLayer::Working)
            .expect("load working");
        let project = store
            .load_memory(&thread.id, MemoryLayer::Project)
            .expect("load project");
        assert_eq!(working.entries.len(), 1);
        assert_eq!(project.entries.len(), 1);
    }

    #[test]
    fn contract_progress_and_evaluation_roundtrip() {
        let dir = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(dir.path(), BackendProvider::Codex);
        let thread = store.create_thread(Some("长任务")).expect("thread");
        let run = store
            .create_run(
                &thread.id,
                None,
                ThinkingMode::Balanced,
                AgentBackendKind::Codex,
            )
            .expect("run");

        let contract = ExecutionContract {
            goal: "实现长期 harness".to_string(),
            non_goals: vec!["不重写 CLI".to_string()],
            constraints: vec!["保持兼容".to_string()],
            ordered_features: vec![FeatureSlice {
                id: "feature-1".to_string(),
                title: "引入 contract".to_string(),
                intent: "持久化执行契约".to_string(),
                scope_paths: vec!["src/harness".to_string()],
                done_when: vec![AcceptanceCriterion {
                    id: "acc-1".to_string(),
                    description: "contract 已落盘".to_string(),
                }],
                status: FeatureSliceStatus::Pending,
            }],
            global_acceptance: vec![],
            delivery_notes: vec![],
            updated_at: chrono::Utc::now(),
        };
        store
            .save_execution_contract(&thread.id, &contract)
            .expect("save contract");
        let loaded_contract = store
            .load_execution_contract(&thread.id)
            .expect("load contract");
        assert_eq!(loaded_contract.goal, contract.goal);

        let progress = ProgressLedger {
            goal: contract.goal.clone(),
            current_phase: Some("执行".to_string()),
            completed_features: vec![],
            current_feature: Some("feature-1".to_string()),
            latest_recoverable_failure: None,
            blocking_reason: None,
            known_failures: vec![],
            decisions: vec!["先做持久化".to_string()],
            open_questions: vec![],
            next_step: Some("执行 feature-1".to_string()),
            updated_at: chrono::Utc::now(),
        };
        store
            .save_progress_ledger(&thread.id, &progress)
            .expect("save progress");
        let loaded_progress = store
            .load_progress_ledger(&thread.id)
            .expect("load progress");
        assert_eq!(loaded_progress.current_feature, progress.current_feature);

        let decision = EvaluationDecision {
            passed: true,
            reason: "验证通过".to_string(),
            follow_up_actions: vec![],
            retryable: false,
            feature_id: Some("feature-1".to_string()),
            created_at: chrono::Utc::now(),
        };
        store
            .append_evaluation(&run, &decision)
            .expect("append evaluation");
        let evaluations = store.list_evaluations(&run).expect("list evaluations");
        assert_eq!(evaluations.len(), 1);
    }

    #[test]
    fn delete_thread_removes_manifest_and_directory() {
        let dir = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(dir.path(), BackendProvider::Codex);
        let thread = store.create_thread(Some("待删除")).expect("thread");
        assert!(thread.thread_dir.exists());

        store.delete_thread(&thread.id).expect("delete thread");

        assert!(!thread.thread_dir.exists());
        assert!(store.load_thread(&thread.id).is_err());
    }

    #[test]
    fn thread_and_run_data_live_under_codex_forge_directory() {
        let dir = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(dir.path(), BackendProvider::Codex);
        let thread = store.create_thread(Some("持久化目录")).expect("thread");
        let run = store
            .create_run(
                &thread.id,
                None,
                ThinkingMode::Balanced,
                AgentBackendKind::Codex,
            )
            .expect("run");

        assert!(
            thread
                .thread_dir
                .starts_with(dir.path().join(".codex-forge").join("modes").join("codex")),
            "{}",
            thread.thread_dir.display()
        );
        assert!(
            run.run_dir
                .starts_with(dir.path().join(".codex-forge").join("modes").join("codex")),
            "{}",
            run.run_dir.display()
        );
    }

    #[test]
    fn different_modes_use_isolated_thread_namespaces() {
        let dir = TempDir::new().expect("tempdir");
        let codex_store = HarnessStore::new(dir.path(), BackendProvider::Codex);
        let openai_store = HarnessStore::new(dir.path(), BackendProvider::OpenAiCompatible);
        let codex_thread = codex_store
            .create_thread(Some("codex"))
            .expect("codex thread");
        let openai_thread = openai_store
            .create_thread(Some("openai"))
            .expect("openai thread");

        assert!(codex_store.load_thread(&codex_thread.id).is_ok());
        assert!(codex_store.load_thread(&openai_thread.id).is_err());
        assert!(openai_store.load_thread(&openai_thread.id).is_ok());
        assert!(openai_store.load_thread(&codex_thread.id).is_err());
    }

    #[test]
    fn load_thread_repairs_empty_manifest() {
        let dir = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(dir.path(), BackendProvider::Codex);
        let thread = store.create_thread(Some("恢复")).expect("thread");
        store
            .append_message(
                &thread.id,
                HarnessMessageRole::User,
                "恢复一条消息".to_string(),
                None,
            )
            .expect("append message");

        let manifest_path = dir
            .path()
            .join(".codex-forge")
            .join("modes")
            .join("codex")
            .join("threads")
            .join(&thread.id)
            .join("thread.json");
        fs::write(&manifest_path, "").expect("truncate manifest");

        let repaired = store.load_thread(&thread.id).expect("repair thread");
        let repaired_raw = fs::read_to_string(&manifest_path).expect("read repaired manifest");

        assert_eq!(repaired.id, thread.id);
        assert_eq!(repaired.message_count, 1);
        assert!(!repaired_raw.trim().is_empty());
    }
}
