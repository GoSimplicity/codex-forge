use std::fs;

use anyhow::{Context, Result};

use crate::harness::store::HarnessStore;
use crate::harness::types::{
    ArtifactKind, HarnessRunManifest, HarnessThreadManifest, SandboxState, ToolCallRequest,
};

use super::executor::{ToolExecutionResult, materialize_text_artifact, required_string_alias};

pub(super) fn execute_list_tree(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    sandbox: &SandboxState,
) -> Result<ToolExecutionResult> {
    let mut entries = walkdir::WalkDir::new(&sandbox.repo_workdir)
        .max_depth(3)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path() != sandbox.repo_workdir)
        .take(80)
        .map(|entry| {
            entry
                .path()
                .strip_prefix(&sandbox.repo_workdir)
                .unwrap_or(entry.path())
                .display()
                .to_string()
        })
        .collect::<Vec<_>>();
    entries.sort();
    let text = entries.join("\n");
    let artifact =
        materialize_text_artifact(store, thread, run, "tree", ArtifactKind::ToolResult, &text)?;
    Ok(ToolExecutionResult {
        message: format!("list_tree 结果：\n{}", text),
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_read_file(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    sandbox: &SandboxState,
    call: &ToolCallRequest,
) -> Result<ToolExecutionResult> {
    let path = required_string_alias(&call.arguments, &["path"])?;
    let target = sandbox.repo_workdir.join(&path);
    let content = fs::read_to_string(&target)
        .with_context(|| format!("读取文件失败：{}", target.display()))?;
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        "read-file",
        ArtifactKind::ToolResult,
        &content,
    )?;
    Ok(ToolExecutionResult {
        message: format!("read_file `{path}` 成功：\n{}", content),
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_write_file(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    sandbox: &SandboxState,
    call: &ToolCallRequest,
) -> Result<ToolExecutionResult> {
    let path = required_string_alias(&call.arguments, &["path"])?
        .trim()
        .to_string();
    let content = required_string_alias(&call.arguments, &["content", "text"])?;
    let target = sandbox.repo_workdir.join(&path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败：{}", parent.display()))?;
    }
    fs::write(&target, &content).with_context(|| format!("写入文件失败：{}", target.display()))?;
    let artifact = store.append_artifact(
        &thread.id,
        &run.id,
        format!("write-file:{path}"),
        ArtifactKind::File,
        target,
    )?;
    Ok(ToolExecutionResult {
        message: format!("write_file `{path}` 成功"),
        artifacts: vec![artifact],
    })
}
