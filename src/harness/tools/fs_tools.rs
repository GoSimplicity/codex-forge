use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::harness::store::HarnessStore;
use crate::harness::types::{
    ArtifactKind, HarnessRunManifest, HarnessThreadManifest, SandboxState, ToolCallRequest,
};

use super::executor::{
    ToolExecutionResult, materialize_text_artifact, required_string_alias, resolve_repo_path,
    sync_sandbox_file_to_repo,
};

pub(super) fn execute_list_tree(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    sandbox: &SandboxState,
    call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let root = resolve_root(
        thread,
        sandbox,
        call.arguments.get("path").and_then(Value::as_str),
    )?;
    let max_depth = call
        .arguments
        .get("max_depth")
        .and_then(Value::as_u64)
        .unwrap_or(3) as usize;
    let mut entries = walkdir::WalkDir::new(&root)
        .max_depth(max_depth)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path() != root)
        .take(80)
        .map(|entry| {
            entry
                .path()
                .strip_prefix(&root)
                .unwrap_or(entry.path())
                .display()
                .to_string()
        })
        .collect::<Vec<_>>();
    entries.sort();
    let text = entries.join("\n");
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        "tree",
        ArtifactKind::ToolResult,
        &text,
        task_node_id,
        subagent_id,
    )?;
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
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let path = required_string_alias(&call.arguments, &["path"])?;
    let target = resolve_repo_path(thread, sandbox, &path)?;
    let content = fs::read_to_string(&target)
        .with_context(|| format!("读取文件失败：{}", target.display()))?;
    let content = truncate_utf8_on_char_boundary(
        content,
        call.arguments.get("max_bytes").and_then(Value::as_u64),
    );
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        "read-file",
        ArtifactKind::ToolResult,
        &content,
        task_node_id,
        subagent_id,
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
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let path = required_string_alias(&call.arguments, &["path"])?
        .trim()
        .to_string();
    let content = required_string_alias(&call.arguments, &["content", "text"])?;
    let target = resolve_repo_path(thread, sandbox, &path)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败：{}", parent.display()))?;
    }
    fs::write(&target, &content).with_context(|| format!("写入文件失败：{}", target.display()))?;
    let host_target = sync_sandbox_file_to_repo(thread, sandbox, &target)?;
    let artifact = store.append_artifact(
        &thread.id,
        &run.id,
        task_node_id.map(ToOwned::to_owned),
        subagent_id.map(ToOwned::to_owned),
        format!("write-file:{path}"),
        ArtifactKind::File,
        target,
    )?;
    Ok(ToolExecutionResult {
        message: format!(
            "write_file `{path}` 成功，目标目录文件：{}",
            host_target.display()
        ),
        artifacts: vec![artifact],
    })
}

fn resolve_root(
    thread: &HarnessThreadManifest,
    sandbox: &SandboxState,
    relative: Option<&str>,
) -> Result<PathBuf> {
    match relative {
        Some(path) => resolve_repo_path(thread, sandbox, path),
        None => Ok(sandbox.repo_workdir.clone()),
    }
}

fn truncate_utf8_on_char_boundary(content: String, max_bytes: Option<u64>) -> String {
    let Some(max_bytes) = max_bytes else {
        return content;
    };
    let max_bytes = max_bytes as usize;
    if content.len() <= max_bytes {
        return content;
    }
    let mut end = max_bytes;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    content[..end].to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::harness::store::HarnessStore;
    use crate::harness::types::{SandboxState, ToolCallRequest};
    use crate::model::ThinkingMode;

    use super::{execute_list_tree, execute_read_file, execute_write_file};

    fn setup() -> (
        TempDir,
        HarnessStore,
        crate::harness::HarnessThreadManifest,
        crate::harness::HarnessRunManifest,
        SandboxState,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(dir.path());
        let thread = store.create_thread(Some("文件工具")).expect("thread");
        let run = store
            .create_run(
                &thread.id,
                Some("gpt-5".to_string()),
                ThinkingMode::Balanced,
                crate::harness::types::AgentBackendKind::Codex,
            )
            .expect("run");
        fs::create_dir_all(dir.path().join("nested")).expect("mkdir");
        let sandbox = SandboxState {
            provider: "test".to_string(),
            image: "test-image".to_string(),
            container_name: "test-box".to_string(),
            workspace_root: run.run_dir.join("sandbox"),
            repo_workdir: dir.path().to_path_buf(),
            container_repo_workdir: "/workspace/repo".into(),
            mount_strategy: "direct_rw".to_string(),
            repair_owner_on_exit: false,
            host_uid: None,
            host_gid: None,
            active: true,
        };
        (dir, store, thread, run, sandbox)
    }

    #[test]
    fn read_file_honors_max_bytes() {
        let (_dir, store, thread, run, sandbox) = setup();
        fs::write(
            sandbox.repo_workdir.join("README.md"),
            "你好，codex-forge\n",
        )
        .expect("write");
        let result = execute_read_file(
            &store,
            &thread,
            &run,
            &sandbox,
            &ToolCallRequest {
                name: "read_file".to_string(),
                arguments: serde_json::json!({
                    "path": "README.md",
                    "max_bytes": 4
                }),
            },
            None,
            None,
        )
        .expect("read");
        assert!(result.message.contains("你"));
        assert!(!result.message.contains("forg"));
    }

    #[test]
    fn list_tree_honors_path_and_depth() {
        let (_dir, store, thread, run, sandbox) = setup();
        fs::write(sandbox.repo_workdir.join("nested").join("demo.txt"), "ok\n").expect("write");
        let result = execute_list_tree(
            &store,
            &thread,
            &run,
            &sandbox,
            &ToolCallRequest {
                name: "list_tree".to_string(),
                arguments: serde_json::json!({
                    "path": "nested",
                    "max_depth": 2
                }),
            },
            None,
            None,
        )
        .expect("list");
        assert!(result.message.contains("demo.txt"));
        assert!(!result.message.contains("nested/demo.txt"));
    }

    #[test]
    fn absolute_repo_path_is_mapped_back_into_sandbox() {
        let (dir, store, thread, run, sandbox) = setup();
        let host_repo_file = dir.path().join("README.md");
        fs::write(&host_repo_file, "host\n").expect("write host");
        fs::write(sandbox.repo_workdir.join("README.md"), "sandbox\n").expect("write sandbox");
        let result = execute_read_file(
            &store,
            &thread,
            &run,
            &sandbox,
            &ToolCallRequest {
                name: "read_file".to_string(),
                arguments: serde_json::json!({
                    "path": thread.repo_root.join("README.md").display().to_string()
                }),
            },
            None,
            None,
        )
        .expect("read");
        assert!(result.message.contains("sandbox"));
        assert!(!result.message.contains("host"));
    }

    #[test]
    fn write_file_syncs_back_to_host_repo() {
        let (dir, store, thread, run, sandbox) = setup();
        let result = execute_write_file(
            &store,
            &thread,
            &run,
            &sandbox,
            &ToolCallRequest {
                name: "write_file".to_string(),
                arguments: serde_json::json!({
                    "path": "nested/output.txt",
                    "content": "hello\n"
                }),
            },
            None,
            None,
        )
        .expect("write");
        assert!(result.message.contains("目标目录文件"));
        assert_eq!(
            fs::read_to_string(sandbox.repo_workdir.join("nested").join("output.txt"))
                .expect("read sandbox"),
            "hello\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("nested").join("output.txt")).expect("read host"),
            "hello\n"
        );
        assert_eq!(thread.repo_root, dir.path());
    }
}
