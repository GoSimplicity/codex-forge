use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::harness::store::HarnessStore;
use crate::harness::types::{
    ArtifactKind, ArtifactRecord, HarnessRunManifest, HarnessThreadManifest, SandboxState,
    ToolCallRecord, ToolCallRequest, ToolCallStatus,
};

use super::fs_tools::{execute_list_tree, execute_read_file, execute_write_file};
use super::meta::{
    execute_apply_patch, execute_create_plan_snapshot, execute_create_session_bootstrap,
    execute_inspect_run, execute_list_artifacts, execute_list_skills, execute_read_artifact,
    execute_read_contract, execute_read_memory, execute_read_progress, execute_read_skill,
    execute_record_evaluation, execute_remember_memory, execute_update_progress,
    execute_write_contract,
};
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
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    match call.name.as_str() {
        "list_tree" => {
            execute_list_tree(store, thread, run, sandbox, call, task_node_id, subagent_id)
        }
        "read_file" => {
            execute_read_file(store, thread, run, sandbox, call, task_node_id, subagent_id)
        }
        "search_files" => {
            execute_search_files(store, thread, run, sandbox, call, task_node_id, subagent_id)
        }
        "apply_patch" => {
            execute_apply_patch(store, thread, run, sandbox, call, task_node_id, subagent_id)
        }
        "run_shell" => {
            execute_run_shell(store, thread, run, sandbox, call, task_node_id, subagent_id)
        }
        "write_file" => {
            execute_write_file(store, thread, run, sandbox, call, task_node_id, subagent_id)
        }
        "list_artifacts" => {
            execute_list_artifacts(store, thread, run, call, task_node_id, subagent_id)
        }
        "read_artifact" => {
            execute_read_artifact(store, thread, run, call, task_node_id, subagent_id)
        }
        "inspect_run" => execute_inspect_run(store, thread, run, call, task_node_id, subagent_id),
        "create_plan_snapshot" => {
            execute_create_plan_snapshot(store, thread, run, task_node_id, subagent_id)
        }
        "read_contract" => {
            execute_read_contract(store, thread, run, call, task_node_id, subagent_id)
        }
        "write_contract" => {
            execute_write_contract(store, thread, run, call, task_node_id, subagent_id)
        }
        "read_progress" => {
            execute_read_progress(store, thread, run, call, task_node_id, subagent_id)
        }
        "update_progress" => {
            execute_update_progress(store, thread, run, call, task_node_id, subagent_id)
        }
        "record_evaluation" => {
            execute_record_evaluation(store, thread, run, call, task_node_id, subagent_id)
        }
        "create_session_bootstrap" => {
            execute_create_session_bootstrap(store, thread, run, call, task_node_id, subagent_id)
        }
        "read_memory" => execute_read_memory(store, thread, run, call, task_node_id, subagent_id),
        "remember_memory" => {
            execute_remember_memory(store, thread, run, call, task_node_id, subagent_id)
        }
        "list_skills" => execute_list_skills(store, thread, run, task_node_id, subagent_id),
        "read_skill" => execute_read_skill(store, thread, run, call, task_node_id, subagent_id),
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

#[allow(clippy::too_many_arguments)]
pub(super) fn materialize_text_artifact(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    label: &str,
    kind: ArtifactKind,
    content: &str,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
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
    store.append_artifact(
        &thread.id,
        &run.id,
        task_node_id.map(ToOwned::to_owned),
        subagent_id.map(ToOwned::to_owned),
        label.to_string(),
        kind,
        path,
    )
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

pub(super) fn resolve_repo_path(
    thread: &HarnessThreadManifest,
    sandbox: &SandboxState,
    raw_path: &str,
) -> Result<PathBuf> {
    let candidate = Path::new(raw_path);
    if candidate.is_absolute() {
        let relative = candidate
            .strip_prefix(&thread.repo_root)
            .map_err(|_| anyhow!("路径超出当前仓库范围：{raw_path}"))?;
        validate_relative_path(relative)?;
        return Ok(sandbox.repo_workdir.join(relative));
    }

    validate_relative_path(candidate)?;
    Ok(sandbox.repo_workdir.join(candidate))
}

pub(super) fn sync_sandbox_file_to_repo(
    thread: &HarnessThreadManifest,
    sandbox: &SandboxState,
    sandbox_path: &Path,
) -> Result<PathBuf> {
    if sandbox.mount_strategy == "direct_rw" {
        let relative = sandbox_path
            .strip_prefix(&thread.repo_root)
            .with_context(|| {
                format!(
                    "直接挂载模式下路径不在目标目录内：{}（repo={})",
                    sandbox_path.display(),
                    thread.repo_root.display()
                )
            })?;
        validate_relative_path(relative)?;
        if !sandbox_path.exists() {
            bail!("目标文件未落到宿主目录：{}", sandbox_path.display());
        }
        return Ok(sandbox_path.to_path_buf());
    }

    let relative = sandbox_path
        .strip_prefix(&sandbox.repo_workdir)
        .with_context(|| {
            format!(
                "沙箱路径不在 repo 工作区内：{}（repo={})",
                sandbox_path.display(),
                sandbox.repo_workdir.display()
            )
        })?;
    validate_relative_path(relative)?;
    let host_path = thread.repo_root.join(relative);
    if let Some(parent) = host_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目标目录失败：{}", parent.display()))?;
    }
    fs::copy(sandbox_path, &host_path).with_context(|| {
        format!(
            "同步文件回目标目录失败：{} -> {}",
            sandbox_path.display(),
            host_path.display()
        )
    })?;
    Ok(host_path)
}

fn validate_relative_path(path: &Path) -> Result<()> {
    for component in path.components() {
        match component {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("不允许访问仓库外路径：{}", path.display());
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}
