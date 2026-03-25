use std::path::PathBuf;

use anyhow::Result;

use crate::config::{ProjectConfig, load_project_config};
use crate::harness::{
    ApprovalRecord, ArtifactRecord, HarnessEventRecord, HarnessMessage, HarnessRunManifest,
    HarnessStore, HarnessThreadManifest,
};

use super::tabs::{DetailTab, FocusMode};

pub(crate) struct TuiApp {
    pub(crate) repo_root: PathBuf,
    pub(crate) store: HarnessStore,
    pub(crate) config: ProjectConfig,
    pub(crate) threads: Vec<HarnessThreadManifest>,
    pub(crate) selected_thread_id: Option<String>,
    pub(crate) messages: Vec<HarnessMessage>,
    pub(crate) runs: Vec<HarnessRunManifest>,
    pub(crate) events: Vec<HarnessEventRecord>,
    pub(crate) approvals: Vec<ApprovalRecord>,
    pub(crate) artifacts: Vec<ArtifactRecord>,
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
            messages: Vec::new(),
            runs: Vec::new(),
            events: Vec::new(),
            approvals: Vec::new(),
            artifacts: Vec::new(),
            detail_tab: DetailTab::Messages,
            focus: FocusMode::Browse,
            composer: String::new(),
            status:
                "i 输入，Enter 发送，a 通过审批，x 拒绝审批，n 新建 thread，Tab 切换视图，q 退出"
                    .to_string(),
        })
    }
}
