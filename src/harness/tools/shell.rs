use anyhow::Result;

use crate::harness::sandbox::{DockerSandboxProvider, ShellExecResult};
use crate::harness::store::HarnessStore;
use crate::harness::types::{
    ArtifactKind, HarnessRunManifest, HarnessThreadManifest, SandboxState, ToolCallRequest,
};

use super::executor::{ToolExecutionResult, materialize_text_artifact, required_string_alias};

pub(super) fn execute_run_shell(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    sandbox: &SandboxState,
    call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let command = normalize_shell_command_for_sandbox(
        required_string_alias(&call.arguments, &["command", "cmd"])?,
        thread,
        sandbox,
    );
    let provider = DockerSandboxProvider::from(sandbox);
    let result = provider.exec_shell(sandbox, &command)?;
    shell_result_to_artifacts(
        store,
        thread,
        run,
        "run-shell",
        command,
        result,
        task_node_id,
        subagent_id,
    )
}

fn normalize_shell_command_for_sandbox(
    command: String,
    thread: &HarnessThreadManifest,
    sandbox: &SandboxState,
) -> String {
    let repo_root = thread.repo_root.display().to_string();
    let sandbox_repo = sandbox.repo_workdir.display().to_string();
    let container_repo = sandbox.container_repo_workdir.display().to_string();

    command
        .replace(&repo_root, &container_repo)
        .replace(&sandbox_repo, &container_repo)
}

#[allow(clippy::too_many_arguments)]
fn shell_result_to_artifacts(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    label: &str,
    command: String,
    result: ShellExecResult,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let combined = format!(
        "$ {command}\n[exit={:?}]\nstdout:\n{}\nstderr:\n{}",
        result.exit_code, result.stdout, result.stderr
    );
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        label,
        ArtifactKind::SandboxLog,
        &combined,
        task_node_id,
        subagent_id,
    )?;
    Ok(ToolExecutionResult {
        message: if result.success {
            combined
        } else {
            format!("命令失败：\n{combined}")
        },
        artifacts: vec![artifact],
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;

    use crate::harness::types::HarnessThreadManifest;

    use super::normalize_shell_command_for_sandbox;
    use crate::harness::types::SandboxState;

    #[test]
    fn rewrites_host_repo_path_to_container_repo_path() {
        let thread = HarnessThreadManifest {
            id: "thread-1".to_string(),
            title: "demo".to_string(),
            repo_root: PathBuf::from("/Users/demo/project"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            message_count: 0,
            run_count: 0,
            last_run_id: None,
            last_run_status: None,
            thread_dir: PathBuf::from("/tmp/thread"),
            messages_path: PathBuf::from("/tmp/thread/messages.jsonl"),
            runs_dir: PathBuf::from("/tmp/thread/runs"),
            approvals_dir: PathBuf::from("/tmp/thread/approvals"),
            artifacts_dir: PathBuf::from("/tmp/thread/artifacts"),
            memory_dir: PathBuf::from("/tmp/thread/memory"),
            contract_path: PathBuf::from("/tmp/thread/contract.json"),
            progress_path: PathBuf::from("/tmp/thread/progress.json"),
            bootstrap_path: PathBuf::from("/tmp/thread/session-bootstrap.md"),
        };
        let sandbox = SandboxState {
            provider: "docker".to_string(),
            image: "demo".to_string(),
            container_name: "box".to_string(),
            workspace_root: PathBuf::from("/tmp/workspace"),
            repo_workdir: PathBuf::from("/Users/demo/project"),
            container_repo_workdir: PathBuf::from("/workspace/repo"),
            mount_strategy: "direct_rw".to_string(),
            repair_owner_on_exit: false,
            host_uid: Some(501),
            host_gid: Some(20),
            active: true,
        };

        let command = normalize_shell_command_for_sandbox(
            "ls -la /Users/demo/project && cat /Users/demo/project/PLAN.md".to_string(),
            &thread,
            &sandbox,
        );

        assert_eq!(
            command,
            "ls -la /workspace/repo && cat /workspace/repo/PLAN.md"
        );
    }
}
