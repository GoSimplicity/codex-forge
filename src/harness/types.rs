use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::ThinkingMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessRunStatus {
    Pending,
    Running,
    WaitingForInput,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessMessageRole {
    User,
    Assistant,
    System,
    Tool,
    Summary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    PendingApproval,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Text,
    File,
    ToolResult,
    SandboxLog,
    SandboxSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackendKind {
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentKind {
    Explorer,
    Implementer,
    Reviewer,
    Tester,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessThreadManifest {
    pub id: String,
    pub title: String,
    pub repo_root: PathBuf,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
    pub run_count: usize,
    #[serde(default)]
    pub last_run_id: Option<String>,
    #[serde(default)]
    pub last_run_status: Option<HarnessRunStatus>,
    pub thread_dir: PathBuf,
    pub messages_path: PathBuf,
    pub runs_dir: PathBuf,
    pub approvals_dir: PathBuf,
    pub artifacts_dir: PathBuf,
    pub memory_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessMessage {
    pub id: String,
    pub role: HarnessMessageRole,
    pub content: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxState {
    pub provider: String,
    pub image: String,
    pub container_name: String,
    pub workspace_root: PathBuf,
    pub repo_workdir: PathBuf,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessRunManifest {
    pub id: String,
    pub thread_id: String,
    pub status: HarnessRunStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub model: Option<String>,
    pub thinking_mode: ThinkingMode,
    pub backend: AgentBackendKind,
    pub turn_count: usize,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    pub run_dir: PathBuf,
    pub events_path: PathBuf,
    pub output_path: PathBuf,
    pub log_path: PathBuf,
    pub tool_calls_path: PathBuf,
    pub approvals_path: PathBuf,
    pub artifacts_path: PathBuf,
    pub subagents_path: PathBuf,
    #[serde(default)]
    pub sandbox: Option<SandboxState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub id: String,
    pub thread_id: String,
    pub run_id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
    pub status: ToolCallStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub approval_id: Option<String>,
    #[serde(default)]
    pub output_summary: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub id: String,
    pub thread_id: String,
    pub run_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub reason: String,
    pub status: ApprovalStatus,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub resolved_at: Option<DateTime<Utc>>,
    pub tool_call: ToolCallRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub id: String,
    pub thread_id: String,
    pub run_id: String,
    pub label: String,
    pub kind: ArtifactKind,
    pub path: PathBuf,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentRecord {
    pub id: String,
    pub thread_id: String,
    pub run_id: String,
    pub kind: SubagentKind,
    pub task: String,
    pub status: HarnessRunStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    RunCreated {
        thread_id: String,
        run_id: String,
    },
    SandboxReady {
        thread_id: String,
        run_id: String,
        image: String,
        container_name: String,
    },
    RunStarted {
        thread_id: String,
        run_id: String,
    },
    BackendTurnCompleted {
        thread_id: String,
        run_id: String,
        turn: usize,
    },
    MessageAppended {
        thread_id: String,
        message_id: String,
        role: HarnessMessageRole,
    },
    ToolCallPlanned {
        thread_id: String,
        run_id: String,
        tool_call_id: String,
        tool_name: String,
    },
    ToolCallCompleted {
        thread_id: String,
        run_id: String,
        tool_call_id: String,
        status: ToolCallStatus,
    },
    ApprovalRequested {
        thread_id: String,
        run_id: String,
        approval_id: String,
        tool_name: String,
    },
    ApprovalResolved {
        thread_id: String,
        run_id: String,
        approval_id: String,
        status: ApprovalStatus,
    },
    ArtifactCreated {
        thread_id: String,
        run_id: String,
        artifact_id: String,
        label: String,
    },
    SubagentStarted {
        thread_id: String,
        run_id: String,
        subagent_id: String,
        kind: SubagentKind,
    },
    SubagentCompleted {
        thread_id: String,
        run_id: String,
        subagent_id: String,
        status: HarnessRunStatus,
    },
    RunCompleted {
        thread_id: String,
        run_id: String,
    },
    RunFailed {
        thread_id: String,
        run_id: String,
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessEventRecord {
    pub at: DateTime<Utc>,
    pub payload: HarnessEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendSubagentCall {
    pub kind: SubagentKind,
    pub task: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnEnvelope {
    #[serde(default)]
    pub assistant_message: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRequest>,
    #[serde(default)]
    pub subagent_calls: Vec<BackendSubagentCall>,
    #[serde(default)]
    pub final_response: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ChatRunOutcome {
    pub thread: HarnessThreadManifest,
    pub run: HarnessRunManifest,
    pub user_message: HarnessMessage,
    #[allow(dead_code)]
    pub assistant_message: Option<HarnessMessage>,
}
