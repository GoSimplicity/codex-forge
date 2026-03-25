use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::harness::store::HarnessStore;
use crate::harness::types::{
    ArtifactKind, HarnessRunManifest, HarnessThreadManifest, SandboxState, ToolCallRequest,
};

use super::executor::{ToolExecutionResult, materialize_text_artifact, required_string_alias};

pub(super) fn execute_search_files(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    sandbox: &SandboxState,
    call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let pattern = required_string_alias(&call.arguments, &["pattern", "query", "q", "keyword"])?;
    let search_root = resolve_search_root(sandbox.repo_workdir.clone(), &call.arguments);
    let max_results = call
        .arguments
        .get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .max(1)
        .to_string();
    let output = Command::new("rg")
        .arg("-n")
        .arg("-m")
        .arg(max_results)
        .arg(&pattern)
        .arg(&search_root)
        .output()
        .with_context(|| format!("执行 rg 失败：{}", search_root.display()))?;
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
        task_node_id,
        subagent_id,
    )?;
    Ok(ToolExecutionResult {
        message: format!("search_files `{pattern}` 结果：\n{}", text.trim()),
        artifacts: vec![artifact],
    })
}

fn resolve_search_root(repo_workdir: PathBuf, arguments: &Value) -> PathBuf {
    arguments
        .get("path")
        .and_then(Value::as_str)
        .map(|path| repo_workdir.join(path))
        .unwrap_or(repo_workdir)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::harness::store::HarnessStore;
    use crate::harness::types::{SandboxState, ToolCallRequest};
    use crate::model::ThinkingMode;

    use super::execute_search_files;

    #[test]
    fn search_files_accepts_query_alias() {
        let dir = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(dir.path());
        let thread = store.create_thread(Some("搜索")).expect("thread");
        let run = store
            .create_run(
                &thread.id,
                Some("gpt-5".to_string()),
                ThinkingMode::Balanced,
            )
            .expect("run");
        let repo_workdir = run.run_dir.join("sandbox").join("repo");
        fs::create_dir_all(&repo_workdir).expect("mkdir");
        fs::write(repo_workdir.join("README.md"), "hello codex-forge\n").expect("write");
        let sandbox = SandboxState {
            provider: "test".to_string(),
            image: "test-image".to_string(),
            container_name: "test-box".to_string(),
            workspace_root: run.run_dir.join("sandbox"),
            repo_workdir,
            active: true,
        };

        let result = execute_search_files(
            &store,
            &thread,
            &run,
            &sandbox,
            &ToolCallRequest {
                name: "search_files".to_string(),
                arguments: serde_json::json!({
                    "query": "codex-forge",
                    "path": ".",
                    "max_results": 5
                }),
            },
            None,
            None,
        )
        .expect("search");
        assert!(result.message.contains("codex-forge"));
    }
}
