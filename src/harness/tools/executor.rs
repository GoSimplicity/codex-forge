use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs;

use crate::harness::store::HarnessStore;
use crate::harness::types::{
    ArtifactKind, ArtifactRecord, HarnessRunManifest, HarnessThreadManifest, SandboxState,
    ToolCallRecord, ToolCallRequest, ToolCallStatus,
};

use super::fs_tools::{execute_list_tree, execute_read_file, execute_write_file};
use super::search::execute_search_files;
use super::shell::execute_run_shell;

#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    pub message: String,
    pub artifacts: Vec<ArtifactRecord>,
}

pub fn execute_tool_call(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    sandbox: &SandboxState,
    call: &ToolCallRequest,
) -> Result<ToolExecutionResult> {
    match call.name.as_str() {
        "list_tree" => execute_list_tree(store, thread, run, sandbox),
        "read_file" => execute_read_file(store, thread, run, sandbox, call),
        "search_files" => execute_search_files(store, thread, run, sandbox, call),
        "run_shell" => execute_run_shell(store, thread, run, sandbox, call),
        "write_file" => execute_write_file(store, thread, run, sandbox, call),
        other => bail!("未知工具：{other}"),
    }
}

pub fn mark_tool_approved(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    tool_call_id: &str,
    approval_id: &str,
) -> Result<ToolCallRecord> {
    let calls = store.list_tool_calls(run)?;
    let Some(mut record) = calls.into_iter().find(|item| item.id == tool_call_id) else {
        bail!("未找到 tool call：{tool_call_id}");
    };
    record.approval_id = Some(approval_id.to_string());
    record.status = ToolCallStatus::PendingApproval;
    record.updated_at = chrono::Utc::now();
    store.update_tool_call(run, &record)?;
    Ok(record)
}

pub fn mark_tool_resolution(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    tool_call_id: &str,
    status: ToolCallStatus,
    summary: Option<String>,
    error: Option<String>,
) -> Result<ToolCallRecord> {
    let calls = store.list_tool_calls(run)?;
    let Some(mut record) = calls.into_iter().find(|item| item.id == tool_call_id) else {
        bail!("未找到 tool call：{tool_call_id}");
    };
    record.status = status;
    record.output_summary = summary;
    record.error = error;
    record.updated_at = chrono::Utc::now();
    store.update_tool_call(run, &record)?;
    Ok(record)
}

pub(super) fn materialize_text_artifact(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    label: &str,
    kind: ArtifactKind,
    content: &str,
) -> Result<ArtifactRecord> {
    let artifacts_dir = run.run_dir.join("artifact-files");
    fs::create_dir_all(&artifacts_dir)
        .with_context(|| format!("创建 artifact 目录失败：{}", artifacts_dir.display()))?;
    let path = artifacts_dir.join(format!(
        "{}-{}.txt",
        label,
        chrono::Utc::now().timestamp_millis()
    ));
    fs::write(&path, content).with_context(|| format!("写入 artifact 失败：{}", path.display()))?;
    store.append_artifact(&thread.id, &run.id, label.to_string(), kind, path)
}

pub(super) fn required_string(arguments: &Value, key: &str) -> Result<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(|value| value.to_string())
        .ok_or_else(|| anyhow::anyhow!("缺少字符串参数：{key}"))
}

pub(super) fn required_string_alias(arguments: &Value, keys: &[&str]) -> Result<String> {
    for key in keys {
        if let Some(value) = arguments.get(key).and_then(Value::as_str) {
            return Ok(value.to_string());
        }
    }
    Err(anyhow::anyhow!("缺少字符串参数：{}", keys.join(" / ")))
}
