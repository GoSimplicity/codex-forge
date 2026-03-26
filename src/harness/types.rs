use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::BackendProvider;
use crate::model::ThinkingMode;

fn default_utc_now() -> DateTime<Utc> {
    Utc::now()
}

fn default_agent_backend_kind() -> AgentBackendKind {
    AgentBackendKind::Codex
}

fn default_run_execution_kind() -> RunExecutionKind {
    RunExecutionKind::Orchestrated
}

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
    MemorySnapshot,
    PlanSnapshot,
    ContractSnapshot,
    ProgressSnapshot,
    EvaluationSnapshot,
    SessionBootstrap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLayer {
    Working,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackendKind {
    Codex,
    #[serde(rename = "openai_compatible", alias = "open_ai_compatible")]
    OpenAiCompatible,
}

impl AgentBackendKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OpenAiCompatible => "openai_compatible",
        }
    }
}

impl From<BackendProvider> for AgentBackendKind {
    fn from(value: BackendProvider) -> Self {
        match value {
            BackendProvider::Codex => Self::Codex,
            BackendProvider::OpenAiCompatible => Self::OpenAiCompatible,
        }
    }
}

impl From<AgentBackendKind> for BackendProvider {
    fn from(value: AgentBackendKind) -> Self {
        match value {
            AgentBackendKind::Codex => Self::Codex,
            AgentBackendKind::OpenAiCompatible => Self::OpenAiCompatible,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunExecutionKind {
    AutonomousCodex,
    Orchestrated,
}

impl RunExecutionKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::AutonomousCodex => "autonomous_codex",
            Self::Orchestrated => "orchestrated",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::AutonomousCodex => "Codex 自主执行",
            Self::Orchestrated => "编排式执行",
        }
    }

    pub fn is_autonomous_codex(self) -> bool {
        matches!(self, Self::AutonomousCodex)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentKind {
    #[serde(alias = "explorer")]
    Planner,
    #[serde(alias = "implementer")]
    Generator,
    #[serde(alias = "reviewer", alias = "tester")]
    Evaluator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskGraphStrategy {
    #[serde(alias = "explore_and_summarize")]
    Research,
    #[serde(alias = "implement_and_verify")]
    LongRunningDelivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskNodeKind {
    Plan,
    Initialize,
    BuildExecutionContract,
    PlanReview,
    SelectNextFeature,
    ExecuteFeature,
    EvaluateFeature,
    CheckpointProgress,
    FinalizeDelivery,
    Explore,
    Implement,
    Review,
    Test,
    Summarize,
    ApprovalGate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskNodeStatus {
    Pending,
    Ready,
    Running,
    WaitingForInput,
    Completed,
    Failed,
    Skipped,
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
    #[serde(default)]
    pub contract_path: PathBuf,
    #[serde(default)]
    pub progress_path: PathBuf,
    #[serde(default)]
    pub bootstrap_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub source: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub task_node_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadMemory {
    pub layer: MemoryLayer,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub entries: Vec<MemoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
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
    #[serde(default)]
    pub workspace_root: PathBuf,
    #[serde(default)]
    pub repo_workdir: PathBuf,
    #[serde(default)]
    pub container_repo_workdir: PathBuf,
    #[serde(default)]
    pub mount_strategy: String,
    #[serde(default)]
    pub repair_owner_on_exit: bool,
    #[serde(default)]
    pub host_uid: Option<u32>,
    #[serde(default)]
    pub host_gid: Option<u32>,
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
    #[serde(default = "default_agent_backend_kind")]
    pub backend: AgentBackendKind,
    #[serde(default = "default_run_execution_kind")]
    pub execution_kind: RunExecutionKind,
    pub turn_count: usize,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub blocked_reason: Option<String>,
    pub run_dir: PathBuf,
    pub events_path: PathBuf,
    pub output_path: PathBuf,
    pub log_path: PathBuf,
    pub tool_calls_path: PathBuf,
    pub approvals_path: PathBuf,
    pub artifacts_path: PathBuf,
    pub subagents_path: PathBuf,
    pub task_graph_path: PathBuf,
    pub task_nodes_path: PathBuf,
    #[serde(default)]
    pub evaluation_log_path: PathBuf,
    #[serde(default)]
    pub bootstrap_path: PathBuf,
    #[serde(default)]
    pub active_task_node_id: Option<String>,
    #[serde(default)]
    pub sandbox: Option<SandboxState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskGraphManifest {
    pub id: String,
    pub thread_id: String,
    pub run_id: String,
    pub goal: String,
    pub strategy: TaskGraphStrategy,
    #[serde(default)]
    pub success_criteria: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNodeRecord {
    pub id: String,
    pub graph_id: String,
    pub thread_id: String,
    pub run_id: String,
    pub kind: TaskNodeKind,
    pub title: String,
    pub instructions: String,
    pub depends_on: Vec<String>,
    pub position: usize,
    pub status: TaskNodeStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub output_summary: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub last_subagent_id: Option<String>,
    #[serde(default)]
    pub attempt_count: usize,
    #[serde(default)]
    pub feature_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureSliceStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureSlice {
    pub id: String,
    pub title: String,
    pub intent: String,
    #[serde(default)]
    pub scope_paths: Vec<String>,
    #[serde(default)]
    pub done_when: Vec<AcceptanceCriterion>,
    pub status: FeatureSliceStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionContract {
    pub goal: String,
    #[serde(default)]
    pub non_goals: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub ordered_features: Vec<FeatureSlice>,
    #[serde(default)]
    pub global_acceptance: Vec<AcceptanceCriterion>,
    #[serde(default)]
    pub delivery_notes: Vec<String>,
    #[serde(default = "default_utc_now")]
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressLedger {
    pub goal: String,
    #[serde(default)]
    pub current_phase: Option<String>,
    #[serde(default)]
    pub completed_features: Vec<String>,
    #[serde(default)]
    pub current_feature: Option<String>,
    #[serde(default)]
    pub latest_recoverable_failure: Option<String>,
    #[serde(default)]
    pub blocking_reason: Option<String>,
    #[serde(default)]
    pub known_failures: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    #[serde(default)]
    pub next_step: Option<String>,
    #[serde(default = "default_utc_now")]
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationDecision {
    pub passed: bool,
    pub reason: String,
    #[serde(default)]
    pub follow_up_actions: Vec<String>,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub feature_id: Option<String>,
    #[serde(default = "default_utc_now")]
    pub created_at: DateTime<Utc>,
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
    #[serde(default)]
    pub task_node_id: Option<String>,
    #[serde(default)]
    pub subagent_id: Option<String>,
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
    #[serde(default)]
    pub task_node_id: Option<String>,
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
    #[serde(default)]
    pub task_node_id: Option<String>,
    #[serde(default)]
    pub subagent_id: Option<String>,
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
    #[serde(default)]
    pub task_node_id: Option<String>,
    pub kind: SubagentKind,
    pub task: String,
    #[serde(default)]
    pub model: Option<String>,
    pub thinking_mode: ThinkingMode,
    pub status: HarnessRunStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub output_path: PathBuf,
    pub log_path: PathBuf,
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
    RecoverableFailureDetected {
        thread_id: String,
        run_id: String,
        source: String,
        detail: String,
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
    AgentHandoff {
        thread_id: String,
        run_id: String,
        from: String,
        to: String,
        reason: String,
    },
    TaskGraphCreated {
        thread_id: String,
        run_id: String,
        graph_id: String,
        strategy: TaskGraphStrategy,
    },
    TaskNodeReady {
        thread_id: String,
        run_id: String,
        task_node_id: String,
        kind: TaskNodeKind,
    },
    TaskNodeStarted {
        thread_id: String,
        run_id: String,
        task_node_id: String,
        kind: TaskNodeKind,
    },
    TaskNodeCompleted {
        thread_id: String,
        run_id: String,
        task_node_id: String,
        kind: TaskNodeKind,
        status: TaskNodeStatus,
    },
    TaskNodeFailed {
        thread_id: String,
        run_id: String,
        task_node_id: String,
        kind: TaskNodeKind,
        error: String,
    },
    TaskNodeRetried {
        thread_id: String,
        run_id: String,
        task_node_id: String,
    },
    EvidenceInsufficient {
        thread_id: String,
        run_id: String,
        task_node_id: String,
        detail: String,
    },
    RunCompleted {
        thread_id: String,
        run_id: String,
    },
    RunCancelled {
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
    #[serde(default)]
    pub state_update: Option<Value>,
    #[serde(default)]
    pub selected_feature_id: Option<String>,
    #[serde(default)]
    pub evaluation: Option<EvaluationDecision>,
    #[serde(default)]
    pub needs_handoff: bool,
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
