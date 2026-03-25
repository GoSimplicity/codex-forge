use std::path::PathBuf;

use anyhow::Result;

use crate::config::{ProjectConfig, load_project_config};
use crate::harness::{
    ApprovalRecord, ArtifactRecord, HarnessEventRecord, HarnessMessage, HarnessRunManifest,
    HarnessStore, HarnessThreadManifest, MemoryEntry, TaskNodeRecord,
};

use super::tabs::{DetailTab, FocusMode};

pub(crate) struct TuiApp {
    pub(crate) repo_root: PathBuf,
    pub(crate) store: HarnessStore,
    pub(crate) config: ProjectConfig,
    pub(crate) threads: Vec<HarnessThreadManifest>,
    pub(crate) selected_thread_id: Option<String>,
    pub(crate) selected_run_id: Option<String>,
    pub(crate) selected_task_node_id: Option<String>,
    pub(crate) messages: Vec<HarnessMessage>,
    pub(crate) runs: Vec<HarnessRunManifest>,
    pub(crate) task_nodes: Vec<TaskNodeRecord>,
    pub(crate) events: Vec<HarnessEventRecord>,
    pub(crate) approvals: Vec<ApprovalRecord>,
    pub(crate) artifacts: Vec<ArtifactRecord>,
    pub(crate) working_memory: Vec<MemoryEntry>,
    pub(crate) project_memory: Vec<MemoryEntry>,
    pub(crate) detail_tab: DetailTab,
    pub(crate) focus: FocusMode,
    pub(crate) composer: String,
    pub(crate) status: String,
}

impl TuiApp {
    pub(crate) fn new(repo_root: PathBuf, selected_thread_id: Option<String>) -> Result<Self> {
        let config = load_project_config(&repo_root)?.value;
        Ok(Self {
            store: HarnessStore::new(&repo_root),
            repo_root,
            config,
            threads: Vec::new(),
            selected_thread_id,
            selected_run_id: None,
            selected_task_node_id: None,
            messages: Vec::new(),
            runs: Vec::new(),
            task_nodes: Vec::new(),
            events: Vec::new(),
            approvals: Vec::new(),
            artifacts: Vec::new(),
            working_memory: Vec::new(),
            project_memory: Vec::new(),
            detail_tab: DetailTab::Messages,
            focus: FocusMode::Browse,
            composer: String::new(),
            status:
                "j/k thread | h/l run | J/K node | i 输入 | a/x 审批 | s 恢复 | c 取消 | R 重试节点 | q 退出"
                    .to_string(),
        })
    }
}
