use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use tokio::task::JoinHandle;

use crate::config::{AppConfig, load_app_config};
use crate::harness::types::{ChatRunOutcome, ExecutionContract, ProgressLedger, SubagentRecord};
use crate::harness::{
    ApprovalRecord, ArtifactRecord, HarnessEventRecord, HarnessMessage, HarnessRunManifest,
    HarnessStore, HarnessThreadManifest, MemoryEntry, TaskNodeRecord,
};

use super::tabs::{BrowsePane, FocusMode};

pub(crate) enum PendingTaskOutcome {
    Chat(ChatRunOutcome),
    Run(HarnessRunManifest),
}

pub(crate) enum PendingTaskKind {
    ChatMessage,
    ConfirmPlan,
    ResolveApproval { approval_id: String, approved: bool },
    ResumeRun,
    RetryTaskNode { task_node_id: String },
}

pub(crate) struct PendingSend {
    pub(crate) thread_id: String,
    pub(crate) kind: PendingTaskKind,
    pub(crate) handle: JoinHandle<Result<PendingTaskOutcome>>,
}

pub(crate) struct TuiApp {
    pub(crate) repo_root: PathBuf,
    pub(crate) store: HarnessStore,
    pub(crate) config: AppConfig,
    pub(crate) threads: Vec<HarnessThreadManifest>,
    pub(crate) selected_thread_id: Option<String>,
    pub(crate) selected_run_id: Option<String>,
    pub(crate) selected_task_node_id: Option<String>,
    pub(crate) messages: Vec<HarnessMessage>,
    pub(crate) runs: Vec<HarnessRunManifest>,
    pub(crate) task_nodes: Vec<TaskNodeRecord>,
    pub(crate) events: Vec<HarnessEventRecord>,
    pub(crate) approvals: Vec<ApprovalRecord>,
    pub(crate) selected_approval_index: usize,
    pub(crate) artifacts: Vec<ArtifactRecord>,
    pub(crate) subagents: Vec<SubagentRecord>,
    pub(crate) current_contract: Option<ExecutionContract>,
    pub(crate) current_progress: Option<ProgressLedger>,
    pub(crate) working_memory: Vec<MemoryEntry>,
    pub(crate) project_memory: Vec<MemoryEntry>,
    pub(crate) focus: FocusMode,
    pub(crate) browse_pane: BrowsePane,
    pub(crate) detail_parent_pane: BrowsePane,
    pub(crate) composer: String,
    pub(crate) pending_send: Option<PendingSend>,
    pub(crate) pending_delete_thread_id: Option<String>,
    pub(crate) last_refresh_at: Instant,
    pub(crate) live_output_title: String,
    pub(crate) live_output_body: String,
    pub(crate) live_output_scroll: u16,
    pub(crate) live_output_follow_latest: bool,
    pub(crate) detail_viewport_width: u16,
    pub(crate) detail_viewport_height: u16,
    pub(crate) status: String,
}

impl TuiApp {
    pub(crate) fn new(repo_root: PathBuf, selected_thread_id: Option<String>) -> Result<Self> {
        let config = load_app_config(&repo_root)?;
        let store = crate::harness::HarnessStore::new(&repo_root, config.backend.provider);
        Ok(Self {
            store,
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
            selected_approval_index: 0,
            artifacts: Vec::new(),
            subagents: Vec::new(),
            current_contract: None,
            current_progress: None,
            working_memory: Vec::new(),
            project_memory: Vec::new(),
            focus: FocusMode::Browse,
            browse_pane: BrowsePane::Threads,
            detail_parent_pane: BrowsePane::Runs,
            composer: String::new(),
            pending_send: None,
            pending_delete_thread_id: None,
            last_refresh_at: Instant::now(),
            live_output_title: "实时输出".to_string(),
            live_output_body: "等待运行...".to_string(),
            live_output_scroll: 0,
            live_output_follow_latest: true,
            detail_viewport_width: 0,
            detail_viewport_height: 0,
            status: "准备就绪：先选 thread，再直接输入任务；所有即时反馈会显示在底部状态栏"
                .to_string(),
        })
    }
}
