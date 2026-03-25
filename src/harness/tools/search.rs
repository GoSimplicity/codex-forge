use std::process::Command;

use anyhow::{Context, Result};

use crate::harness::store::HarnessStore;
use crate::harness::types::{
    ArtifactKind, HarnessRunManifest, HarnessThreadManifest, SandboxState, ToolCallRequest,
};

use super::executor::{ToolExecutionResult, materialize_text_artifact, required_string};

pub(super) fn execute_search_files(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    sandbox: &SandboxState,
    call: &ToolCallRequest,
) -> Result<ToolExecutionResult> {
    let pattern = required_string(&call.arguments, "pattern")?;
    let output = Command::new("rg")
        .arg("-n")
        .arg(&pattern)
        .arg(&sandbox.repo_workdir)
        .output()
        .with_context(|| format!("执行 rg 失败：{}", sandbox.repo_workdir.display()))?;
    let text = if output.status.success() {
        String::from_utf8_lossy(&output.stdout).to_string()
    } else {
        String::from_utf8_lossy(&output.stderr).to_string()
    };
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        "search",
        ArtifactKind::ToolResult,
        &text,
    )?;
    Ok(ToolExecutionResult {
        message: format!("search_files `{pattern}` 结果：\n{}", text.trim()),
        artifacts: vec![artifact],
    })
}
