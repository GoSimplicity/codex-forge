use std::fs;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::sandbox::{DockerSandboxProvider, ShellExecResult};
use super::store::HarnessStore;
use super::types::{
    ArtifactKind, ArtifactRecord, HarnessRunManifest, HarnessThreadManifest, SandboxState,
    ToolCallRecord, ToolCallRequest, ToolCallStatus,
};

#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    pub message: String,
    pub artifacts: Vec<ArtifactRecord>,
}

pub fn tool_requires_approval(name: &str) -> bool {
    matches!(name, "run_shell" | "write_file")
}

pub fn approval_reason(name: &str) -> &'static str {
    match name {
        "run_shell" => "执行 shell 命令会修改 Docker 沙箱内工作区或产生副作用",
        "write_file" => "写文件会修改 Docker 沙箱内工作区内容",
        _ => "该工具默认需要人工确认",
    }
}

pub fn execute_tool_call(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    sandbox: &SandboxState,
    call: &ToolCallRequest,
) -> Result<ToolExecutionResult> {
    match call.name.as_str() {
        "list_tree" => execute_list_tree(store, thread, run, sandbox, call),
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

fn execute_list_tree(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    sandbox: &SandboxState,
    _call: &ToolCallRequest,
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
    let artifact = materialize_text_artifact(store, thread, run, "tree", ArtifactKind::ToolResult, &text)?;
    Ok(ToolExecutionResult {
        message: format!("list_tree 结果：\n{}", text),
        artifacts: vec![artifact],
    })
}

fn execute_read_file(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    sandbox: &SandboxState,
    call: &ToolCallRequest,
) -> Result<ToolExecutionResult> {
    let path = required_string(&call.arguments, "path")?;
    let target = sandbox.repo_workdir.join(&path);
    let content = fs::read_to_string(&target)
        .with_context(|| format!("读取文件失败：{}", target.display()))?;
    let artifact = materialize_text_artifact(store, thread, run, "read-file", ArtifactKind::ToolResult, &content)?;
    Ok(ToolExecutionResult {
        message: format!("read_file `{path}` 成功：\n{}", content),
        artifacts: vec![artifact],
    })
}

fn execute_search_files(
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
    let artifact = materialize_text_artifact(store, thread, run, "search", ArtifactKind::ToolResult, &text)?;
    Ok(ToolExecutionResult {
        message: format!("search_files `{pattern}` 结果：\n{}", text.trim()),
        artifacts: vec![artifact],
    })
}

fn execute_run_shell(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    sandbox: &SandboxState,
    call: &ToolCallRequest,
) -> Result<ToolExecutionResult> {
    let command = required_string(&call.arguments, "command")?;
    let provider = DockerSandboxProvider {
        image: sandbox.image.clone(),
    };
    let result = provider.exec_shell(sandbox, &command)?;
    shell_result_to_artifacts(store, thread, run, "run-shell", command, result)
}

fn execute_write_file(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    sandbox: &SandboxState,
    call: &ToolCallRequest,
) -> Result<ToolExecutionResult> {
    let path = required_string(&call.arguments, "path")?;
    let content = required_string(&call.arguments, "content")?;
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

fn shell_result_to_artifacts(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    label: &str,
    command: String,
    result: ShellExecResult,
) -> Result<ToolExecutionResult> {
    let combined = format!(
        "$ {command}\n[exit={:?}]\nstdout:\n{}\nstderr:\n{}",
        result.exit_code, result.stdout, result.stderr
    );
    let artifact =
        materialize_text_artifact(store, thread, run, label, ArtifactKind::SandboxLog, &combined)?;
    Ok(ToolExecutionResult {
        message: if result.success {
            combined
        } else {
            format!("命令失败：\n{combined}")
        },
        artifacts: vec![artifact],
    })
}

fn materialize_text_artifact(
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
    let path = artifacts_dir.join(format!("{}-{}.txt", label, chrono::Utc::now().timestamp_millis()));
    fs::write(&path, content).with_context(|| format!("写入 artifact 失败：{}", path.display()))?;
    store.append_artifact(&thread.id, &run.id, label.to_string(), kind, path)
}

fn required_string(arguments: &Value, key: &str) -> Result<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(|value| value.to_string())
        .ok_or_else(|| anyhow::anyhow!("缺少字符串参数：{key}"))
}
