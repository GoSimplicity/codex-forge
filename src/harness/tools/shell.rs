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
    let command = required_string_alias(&call.arguments, &["command", "cmd"])?;
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
