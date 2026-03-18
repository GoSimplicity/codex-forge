use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiMode {
    Rich,
    Minimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyMode {
    AutoSafe,
    Bundle,
    None,
}

impl ApplyMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::AutoSafe => "auto-safe",
            Self::Bundle => "bundle",
            Self::None => "none",
        }
    }
}

impl Display for ApplyMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Planning,
    Running,
    Integrating,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

impl WorkerStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "待执行",
            Self::Running => "执行中",
            Self::Succeeded => "成功",
            Self::Failed => "失败",
            Self::Skipped => "已跳过",
        }
    }
}

impl Display for WorkerStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyStatus {
    Skipped,
    Applied,
    Bundled,
    VerificationFailed,
    SyncFailed,
}

impl ApplyStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Skipped => "跳过应用",
            Self::Applied => "已应用",
            Self::Bundled => "已降级为 bundle",
            Self::VerificationFailed => "集成验证失败",
            Self::SyncFailed => "同步目标工作区失败",
        }
    }
}

impl Display for ApplyStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyDecision {
    AllowFull,
    AllowPartial,
    Block,
}

impl ApplyDecision {
    pub fn label(self) -> &'static str {
        match self {
            Self::AllowFull => "全部放行",
            Self::AllowPartial => "部分放行",
            Self::Block => "阻止应用",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    Skipped,
}

impl CheckStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Passed => "通过",
            Self::Failed => "失败",
            Self::Skipped => "跳过",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeDrift {
    None,
    Minor,
    Major,
}

impl ScopeDrift {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "无漂移",
            Self::Minor => "轻微漂移",
            Self::Major => "重大漂移",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    High,
    Medium,
    Low,
}

impl TrustLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::High => "高",
            Self::Medium => "中",
            Self::Low => "低",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Passed,
    Failed,
    BlockedByEnvironment,
    Skipped,
}

impl VerificationStatus {
    pub fn is_success_like(self) -> bool {
        matches!(self, Self::Passed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationOverallStatus {
    Passed,
    Partial,
    Blocked,
    Failed,
}

impl VerificationOverallStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Passed => "通过",
            Self::Partial => "部分通过",
            Self::Blocked => "受环境阻塞",
            Self::Failed => "失败",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    Completed,
    CompletedWithManualReview,
    Failed,
}

impl ResultStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Completed => "完成",
            Self::CompletedWithManualReview => "完成但需人工复核",
            Self::Failed => "失败",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub task: String,
    pub workers: usize,
    pub role_set: String,
    pub model: Option<String>,
    pub ui_mode: UiMode,
    pub target_dir: PathBuf,
    pub cleanup_success: bool,
    pub apply_mode: ApplyMode,
    pub max_retries: usize,
    pub fail_fast: bool,
    pub verification_commands: Vec<String>,
    pub config_path: Option<PathBuf>,
    pub plan_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSnapshot {
    pub repo_root: PathBuf,
    pub display_name: String,
    pub top_level_entries: Vec<String>,
    pub detected_stacks: Vec<String>,
    pub readme_excerpt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleConfig {
    pub key: String,
    pub title: String,
    pub mission: String,
    pub skills: Vec<String>,
    pub working_style: String,
    pub can_edit: bool,
    pub max_concurrency: Option<usize>,
    pub dependency_policy: Option<String>,
    pub prompt_preamble: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionNode {
    pub id: String,
    pub title: String,
    pub role: String,
    pub objective: String,
    pub deliverables: Vec<String>,
    pub dependencies: Vec<String>,
    pub prompt_focus: String,
    pub input_artifacts: Vec<String>,
    pub output_artifacts: Vec<String>,
    pub completion_criteria: Vec<String>,
    pub allow_code_changes: bool,
    #[serde(default)]
    pub expected_artifacts: Vec<String>,
    #[serde(default)]
    pub required_verifications: Vec<String>,
    #[serde(default)]
    pub scope_guard_ref: Option<String>,
    #[serde(default = "default_scope_drift_none")]
    pub acceptable_drift: ScopeDrift,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionGraph {
    pub summary: String,
    pub strategy: String,
    pub nodes: Vec<ExecutionNode>,
    pub used_fallback: bool,
    pub planning_notes: Vec<String>,
}

impl ExecutionGraph {
    pub fn dependency_count(&self) -> usize {
        self.nodes.iter().map(|node| node.dependencies.len()).sum()
    }

    pub fn topological_order(&self) -> anyhow::Result<Vec<String>> {
        let mut indegree = BTreeMap::<String, usize>::new();
        let mut downstream = BTreeMap::<String, Vec<String>>::new();

        for node in &self.nodes {
            indegree.entry(node.id.clone()).or_insert(0);
        }

        for node in &self.nodes {
            for dep in &node.dependencies {
                if !indegree.contains_key(dep) {
                    anyhow::bail!("执行图引用了不存在的依赖节点：{dep}");
                }
                *indegree.entry(node.id.clone()).or_insert(0) += 1;
                downstream
                    .entry(dep.clone())
                    .or_default()
                    .push(node.id.clone());
            }
        }

        let mut ready = indegree
            .iter()
            .filter(|(_, degree)| **degree == 0)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        ready.sort();

        let mut ordered = Vec::new();
        while let Some(id) = ready.first().cloned() {
            ready.remove(0);
            ordered.push(id.clone());
            if let Some(children) = downstream.get(&id) {
                for child in children {
                    if let Some(entry) = indegree.get_mut(child) {
                        *entry = entry.saturating_sub(1);
                        if *entry == 0 {
                            ready.push(child.clone());
                        }
                    }
                }
                ready.sort();
            }
        }

        if ordered.len() != self.nodes.len() {
            anyhow::bail!("执行图存在循环依赖，无法完成拓扑排序");
        }

        Ok(ordered)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftPolicy {
    pub allow_partial_apply: bool,
    pub block_on_forbidden_paths: bool,
    pub minor_out_of_scope_threshold: usize,
}

impl Default for DriftPolicy {
    fn default() -> Self {
        Self {
            allow_partial_apply: true,
            block_on_forbidden_paths: true,
            minor_out_of_scope_threshold: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeContract {
    pub node_id: String,
    pub allowed_paths: Vec<String>,
    pub forbidden_paths: Vec<String>,
    pub expected_artifacts: Vec<String>,
    pub required_verifications: Vec<String>,
    pub acceptable_drift: ScopeDrift,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionContract {
    pub task_fingerprint: String,
    pub allowed_paths: Vec<String>,
    pub forbidden_paths: Vec<String>,
    pub node_contracts: Vec<NodeContract>,
    pub drift_policy: DriftPolicy,
    pub summary_contract: Vec<String>,
    #[serde(default)]
    pub compatibility_notes: Vec<String>,
}

impl ExecutionContract {
    pub fn node_contract(&self, node_id: &str) -> Option<&NodeContract> {
        self.node_contracts
            .iter()
            .find(|item| item.node_id == node_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTodoItem {
    pub id: String,
    pub title: String,
    pub goal: String,
    pub details: Vec<String>,
    pub dependencies: Vec<String>,
    pub completion_criteria: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTodo {
    pub summary: String,
    pub approach: String,
    pub todos: Vec<PlanTodoItem>,
    pub risks: Vec<String>,
    pub used_fallback: bool,
    pub planning_notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerLocalVerification {
    pub agent_id: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffArtifact {
    pub agent_id: String,
    pub role: String,
    pub task_title: String,
    pub summary: String,
    pub change_intent: String,
    pub touched_files: Vec<String>,
    pub risks: Vec<String>,
    pub verification: Vec<String>,
    pub downstream_suggestions: Vec<String>,
    pub apply_decision: Option<ApplyDecision>,
    #[serde(default)]
    pub contract_scope_claim: Vec<String>,
    #[serde(default)]
    pub expected_vs_actual_artifacts: Vec<String>,
    #[serde(default)]
    pub verification_claims: Vec<String>,
    #[serde(default)]
    pub scope_exceptions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerResult {
    pub agent_id: String,
    pub role: String,
    pub task_title: String,
    pub status: WorkerStatus,
    pub exit_code: Option<i32>,
    pub attempts: usize,
    pub diagnostic_summary: Option<String>,
    pub final_message: String,
    pub summary: Option<String>,
    pub changed_files: Vec<String>,
    pub worktree_path: PathBuf,
    pub prompt_path: PathBuf,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub events_path: PathBuf,
    pub final_output_path: PathBuf,
    pub diff_path: Option<PathBuf>,
    pub git_status_path: Option<PathBuf>,
    pub handoff_path: Option<PathBuf>,
    pub handoff: Option<HandoffArtifact>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyOperation {
    pub agent_id: String,
    pub patch_path: PathBuf,
    pub order: usize,
    pub touched_files: Vec<String>,
    pub applied: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyPlan {
    pub mode: ApplyMode,
    pub operations: Vec<ApplyOperation>,
    pub degrade_to_bundle: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeTrustReport {
    pub trust_level: TrustLevel,
    pub scope_drift: ScopeDrift,
    pub safe_to_auto_apply: bool,
    pub accepted_files: Vec<String>,
    pub manual_review_files: Vec<String>,
    pub rejected_files: Vec<String>,
    pub out_of_scope_files: Vec<String>,
    pub blocking_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyResult {
    pub mode: ApplyMode,
    pub status: ApplyStatus,
    pub integration_worktree: Option<PathBuf>,
    pub applied_workers: Vec<String>,
    pub rejected_workers: Vec<String>,
    pub conflicts: Vec<String>,
    pub synced_to_target: bool,
    pub bundle_dir: Option<PathBuf>,
    pub final_patch_path: Option<PathBuf>,
    pub log_path: PathBuf,
    pub review_gate: Option<ApplyDecision>,
    pub trust_level: TrustLevel,
    pub scope_drift: ScopeDrift,
    pub accepted_files: Vec<String>,
    pub manual_review_files: Vec<String>,
    pub rejected_files: Vec<String>,
    pub out_of_scope_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentBlock {
    pub kind: String,
    pub evidence: String,
    pub fallback_used: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationCommandResult {
    pub stage: String,
    pub command: String,
    pub exit_code: i32,
    pub status: VerificationStatus,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub capability: String,
    pub environment_block: Option<EnvironmentBlock>,
}

impl VerificationCommandResult {
    pub fn passed(&self) -> bool {
        self.status.is_success_like()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub worker_local: Vec<WorkerLocalVerification>,
    pub integration: Vec<VerificationCommandResult>,
    pub final_run: Vec<VerificationCommandResult>,
    pub verified_capabilities: Vec<String>,
    pub failed_capabilities: Vec<String>,
    pub blocked_verifications: Vec<String>,
    pub fallback_verifications: Vec<String>,
    pub overall_status: VerificationOverallStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub agent_id: String,
    pub handoff_path: Option<PathBuf>,
    pub diff_path: Option<PathBuf>,
    pub final_output_path: PathBuf,
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ArtifactManifest {
    pub entries: Vec<ArtifactEntry>,
    pub plan_todo_path: Option<PathBuf>,
    pub execution_contract_path: Option<PathBuf>,
    pub apply_plan_path: Option<PathBuf>,
    pub apply_result_path: Option<PathBuf>,
    pub verification_report_path: Option<PathBuf>,
    pub change_trust_report_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalSummary {
    pub overview: String,
    pub result_status: ResultStatus,
    pub review_gate: Option<ApplyDecision>,
    pub apply_status: ApplyStatus,
    pub trust_level: TrustLevel,
    pub accepted_files: Vec<String>,
    pub manual_review_files: Vec<String>,
    pub rejected_files: Vec<String>,
    pub verified_capabilities: Vec<String>,
    pub blocked_verifications: Vec<String>,
    pub open_risks: Vec<String>,
    pub recommended_next_action: Vec<String>,
    pub used_fallback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RuntimeEvent {
    PhaseChanged {
        phase: String,
    },
    CommanderNote {
        message: String,
    },
    GraphReady {
        nodes: usize,
        dependencies: usize,
    },
    WorkerDispatched {
        agent_id: String,
        role: String,
        title: String,
        worktree_path: PathBuf,
    },
    WorkerUpdate {
        agent_id: String,
        kind: String,
        message: String,
    },
    HandoffReady {
        agent_id: String,
        handoff_path: PathBuf,
    },
    WorkerFinished {
        result: Box<WorkerResult>,
    },
    ApplyPlanReady {
        mode: ApplyMode,
        operations: usize,
    },
    ApplyUpdate {
        message: String,
    },
    VerificationReady {
        stage: String,
        success: bool,
        message: String,
    },
    SummaryReady {
        summary: Box<FinalSummary>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEventRecord {
    pub ts: DateTime<Utc>,
    pub payload: RuntimeEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionManifest {
    pub id: String,
    pub task: String,
    pub repo_snapshot: RepoSnapshot,
    pub created_at: DateTime<Utc>,
    pub status: SessionStatus,
    pub ui_mode: UiMode,
    pub workers_requested: usize,
    pub role_set: String,
    pub model: Option<String>,
    pub cleanup_success: bool,
    pub apply_mode: ApplyMode,
    pub max_retries: usize,
    pub fail_fast: bool,
    pub verification_commands: Vec<String>,
    pub config_path: Option<PathBuf>,
    pub plan_todo: Option<PlanTodo>,
    pub execution_graph: Option<ExecutionGraph>,
    pub execution_contract: Option<ExecutionContract>,
    pub worker_results: Vec<WorkerResult>,
    pub artifact_manifest: ArtifactManifest,
    pub apply_result: Option<ApplyResult>,
    pub verification_report: Option<VerificationReport>,
    pub change_trust_report: Option<ChangeTrustReport>,
    pub doctor_report: Option<DoctorReport>,
    pub final_summary: Option<FinalSummary>,
    pub reused_plan_session_id: Option<String>,
    pub session_dir: PathBuf,
    pub timeline_path: PathBuf,
    pub graph_path: PathBuf,
    pub execution_contract_path: PathBuf,
    pub summary_json_path: PathBuf,
    pub summary_markdown_path: PathBuf,
    pub artifact_manifest_path: PathBuf,
    pub apply_plan_path: PathBuf,
    pub apply_result_path: PathBuf,
    pub verification_report_path: PathBuf,
    pub change_trust_report_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct WorkerLaunchSpec {
    pub agent_id: String,
    pub role: String,
    pub task_title: String,
    pub prompt: String,
    pub worktree_path: PathBuf,
    pub prompt_path: PathBuf,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub events_path: PathBuf,
    pub final_output_path: PathBuf,
    pub diff_path: PathBuf,
    pub git_status_path: PathBuf,
    pub handoff_path: PathBuf,
    pub max_retries: usize,
}

fn default_scope_drift_none() -> ScopeDrift {
    ScopeDrift::None
}

#[cfg(test)]
mod tests {
    use super::{ExecutionGraph, ExecutionNode, ScopeDrift};

    fn sample_node(id: &str, dependencies: &[&str]) -> ExecutionNode {
        ExecutionNode {
            id: id.to_string(),
            title: id.to_string(),
            role: "implementer".to_string(),
            objective: "x".to_string(),
            deliverables: vec![],
            dependencies: dependencies.iter().map(|item| item.to_string()).collect(),
            prompt_focus: "x".to_string(),
            input_artifacts: vec![],
            output_artifacts: vec![],
            completion_criteria: vec![],
            allow_code_changes: true,
            expected_artifacts: vec![],
            required_verifications: vec![],
            scope_guard_ref: None,
            acceptable_drift: ScopeDrift::None,
        }
    }

    #[test]
    fn topological_order_succeeds() {
        let graph = ExecutionGraph {
            summary: String::new(),
            strategy: String::new(),
            nodes: vec![
                sample_node("architect-1", &[]),
                sample_node("implementer-1", &["architect-1"]),
                sample_node("reviewer-1", &["implementer-1"]),
            ],
            used_fallback: false,
            planning_notes: vec![],
        };

        let ordered = graph.topological_order().expect("topo sort should work");
        assert_eq!(
            ordered,
            vec![
                "architect-1".to_string(),
                "implementer-1".to_string(),
                "reviewer-1".to_string()
            ]
        );
    }

    #[test]
    fn topological_order_rejects_cycle() {
        let graph = ExecutionGraph {
            summary: String::new(),
            strategy: String::new(),
            nodes: vec![sample_node("a", &["b"]), sample_node("b", &["a"])],
            used_fallback: false,
            planning_notes: vec![],
        };

        assert!(graph.topological_order().is_err());
    }
}
