use std::fs;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::commands::format::artifact_kind_label;
use crate::harness::skills::SkillAdapter;
use crate::harness::store::HarnessStore;
use crate::harness::types::{
    ArtifactKind, EvaluationDecision, ExecutionContract, HarnessRunManifest, HarnessThreadManifest,
    MemoryLayer, ProgressLedger, SandboxState, ToolCallRequest,
};

use super::executor::{
    ToolExecutionResult, materialize_text_artifact, required_string, required_string_alias,
    resolve_repo_path, sync_sandbox_file_to_repo,
};

pub(super) fn execute_apply_patch(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    sandbox: &SandboxState,
    call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let path = required_string(&call.arguments, "path")?;
    let search = required_string_alias(&call.arguments, &["search", "old"])?;
    let replace = required_string_alias(&call.arguments, &["replace", "new"])?;
    let replace_all = call
        .arguments
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let target = resolve_repo_path(thread, sandbox, &path)?;
    let original = fs::read_to_string(&target)
        .with_context(|| format!("读取待补丁文件失败：{}", target.display()))?;
    if !original.contains(&search) {
        bail!("apply_patch 未找到目标片段：{}", path);
    }
    let updated = if replace_all {
        original.replace(&search, &replace)
    } else {
        original.replacen(&search, &replace, 1)
    };
    fs::write(&target, &updated)
        .with_context(|| format!("写入补丁结果失败：{}", target.display()))?;
    let host_target = sync_sandbox_file_to_repo(thread, sandbox, &target)?;
    let artifact = store.append_artifact(
        &thread.id,
        &run.id,
        task_node_id.map(ToOwned::to_owned),
        subagent_id.map(ToOwned::to_owned),
        format!("apply-patch:{path}"),
        ArtifactKind::File,
        target,
    )?;
    Ok(ToolExecutionResult {
        message: format!(
            "apply_patch `{path}` 成功，目标目录文件：{}",
            host_target.display()
        ),
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_list_artifacts(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let only_current_run = call
        .arguments
        .get("scope")
        .and_then(Value::as_str)
        .map(|scope| scope == "run")
        .unwrap_or(true);
    let artifacts = store.list_artifacts(
        Some(&thread.id),
        if only_current_run {
            Some(&run.id)
        } else {
            None
        },
    )?;
    let text = if artifacts.is_empty() {
        "当前没有 artifact".to_string()
    } else {
        artifacts
            .iter()
            .map(|artifact| {
                format!(
                    "{}\tkind={}\tlabel={}\tpath={}",
                    artifact.id,
                    artifact_kind_label(artifact.kind),
                    artifact.label,
                    artifact.path.display()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        "artifact-list",
        ArtifactKind::ToolResult,
        &text,
        task_node_id,
        subagent_id,
    )?;
    Ok(ToolExecutionResult {
        message: text,
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_read_artifact(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let artifact_id = required_string_alias(&call.arguments, &["artifact_id", "id"])?;
    let artifact_record = store
        .list_artifacts(Some(&thread.id), None)?
        .into_iter()
        .find(|artifact| artifact.id == artifact_id)
        .ok_or_else(|| anyhow!("未找到 artifact：{artifact_id}"))?;
    let content = if matches!(
        artifact_record.kind,
        ArtifactKind::Text
            | ArtifactKind::ToolResult
            | ArtifactKind::SandboxLog
            | ArtifactKind::MemorySnapshot
            | ArtifactKind::PlanSnapshot
            | ArtifactKind::ContractSnapshot
            | ArtifactKind::ProgressSnapshot
            | ArtifactKind::EvaluationSnapshot
            | ArtifactKind::SessionBootstrap
    ) {
        fs::read_to_string(&artifact_record.path).unwrap_or_default()
    } else {
        String::new()
    };
    let text = format!(
        "id: {}\nrun: {}\nkind: {}\nlabel: {}\npath: {}\n\n{}",
        artifact_record.id,
        artifact_record.run_id,
        artifact_kind_label(artifact_record.kind),
        artifact_record.label,
        artifact_record.path.display(),
        content
    );
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        "artifact-read",
        ArtifactKind::ToolResult,
        &text,
        task_node_id,
        subagent_id,
    )?;
    Ok(ToolExecutionResult {
        message: text,
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_inspect_run(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    _call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let nodes = store.list_task_nodes(run)?;
    let text = format!(
        "run={}\nstatus={:?}\nturns={}\nactive_node={}\nsummary={}\nblocked={}\n\nnodes:\n{}",
        run.id,
        run.status,
        run.turn_count,
        run.active_task_node_id.as_deref().unwrap_or("-"),
        run.summary.as_deref().unwrap_or("-"),
        run.blocked_reason.as_deref().unwrap_or("-"),
        nodes
            .iter()
            .map(|node| format!(
                "- {} {:?} {:?} attempts={} {}",
                node.id,
                node.kind,
                node.status,
                node.attempt_count,
                node.output_summary.as_deref().unwrap_or("-")
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        "run-inspect",
        ArtifactKind::ToolResult,
        &text,
        task_node_id,
        subagent_id,
    )?;
    Ok(ToolExecutionResult {
        message: text,
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_create_plan_snapshot(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let graph = store.load_task_graph(run)?;
    let nodes = store.list_task_nodes(run)?;
    let text = format!(
        "# 计划快照\n\ngoal: {}\nstrategy: {:?}\nsuccess_criteria:\n{}\n\nnodes:\n{}",
        graph.goal,
        graph.strategy,
        graph
            .success_criteria
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n"),
        nodes
            .iter()
            .map(|node| format!(
                "- {} {:?} {:?} depends_on={:?} summary={}",
                node.title,
                node.kind,
                node.status,
                node.depends_on,
                node.output_summary.as_deref().unwrap_or("-")
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        "plan-snapshot",
        ArtifactKind::PlanSnapshot,
        &text,
        task_node_id,
        subagent_id,
    )?;
    Ok(ToolExecutionResult {
        message: "已创建 plan snapshot".to_string(),
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_read_contract(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    _call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let text = if thread.contract_path.exists() {
        let contract = store.load_execution_contract(&thread.id)?;
        serde_json::to_string_pretty(&contract).context("序列化 execution contract 失败")?
    } else {
        "当前 thread 还没有 execution contract".to_string()
    };
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        "contract-read",
        ArtifactKind::ContractSnapshot,
        &text,
        task_node_id,
        subagent_id,
    )?;
    Ok(ToolExecutionResult {
        message: text,
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_write_contract(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let contract = if let Some(value) = call.arguments.get("contract") {
        serde_json::from_value::<ExecutionContract>(value.clone())
            .context("解析 contract 参数失败")?
    } else {
        let raw = required_string_alias(&call.arguments, &["content", "json"])?;
        serde_json::from_str::<ExecutionContract>(&raw).context("解析 contract JSON 失败")?
    };
    store.save_execution_contract(&thread.id, &contract)?;
    let artifact = store.append_artifact(
        &thread.id,
        &run.id,
        task_node_id.map(ToOwned::to_owned),
        subagent_id.map(ToOwned::to_owned),
        "execution-contract".to_string(),
        ArtifactKind::ContractSnapshot,
        thread.contract_path.clone(),
    )?;
    Ok(ToolExecutionResult {
        message: "execution contract 已写入".to_string(),
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_read_progress(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    _call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let text = if thread.progress_path.exists() {
        let progress = store.load_progress_ledger(&thread.id)?;
        serde_json::to_string_pretty(&progress).context("序列化 progress 失败")?
    } else {
        "当前 thread 还没有 progress ledger".to_string()
    };
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        "progress-read",
        ArtifactKind::ProgressSnapshot,
        &text,
        task_node_id,
        subagent_id,
    )?;
    Ok(ToolExecutionResult {
        message: text,
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_update_progress(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let progress = if let Some(value) = call.arguments.get("progress") {
        match serde_json::from_value::<ProgressLedger>(value.clone()) {
            Ok(progress) => progress,
            Err(_) => merge_progress_patch(store, &thread.id, value)?,
        }
    } else {
        let raw = required_string_alias(&call.arguments, &["content", "json"])?;
        match serde_json::from_str::<ProgressLedger>(&raw) {
            Ok(progress) => progress,
            Err(_) => {
                let value: Value = serde_json::from_str(&raw).context("解析 progress JSON 失败")?;
                merge_progress_patch(store, &thread.id, &value)?
            }
        }
    };
    store.save_progress_ledger(&thread.id, &progress)?;
    let artifact = store.append_artifact(
        &thread.id,
        &run.id,
        task_node_id.map(ToOwned::to_owned),
        subagent_id.map(ToOwned::to_owned),
        "progress-ledger".to_string(),
        ArtifactKind::ProgressSnapshot,
        thread.progress_path.clone(),
    )?;
    Ok(ToolExecutionResult {
        message: "progress ledger 已更新".to_string(),
        artifacts: vec![artifact],
    })
}

fn merge_progress_patch(
    store: &HarnessStore,
    thread_id: &str,
    patch: &Value,
) -> Result<ProgressLedger> {
    let mut progress = store.load_progress_ledger(thread_id)?;
    let object = patch
        .as_object()
        .ok_or_else(|| anyhow!("progress patch 必须是对象"))?;

    if let Some(items) = object
        .get("completed_features")
        .or_else(|| object.get("completed"))
        .and_then(Value::as_array)
    {
        progress.completed_features = items
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
    }
    if let Some(current) = object
        .get("current_feature")
        .or_else(|| object.get("current"))
        .and_then(Value::as_str)
    {
        progress.current_feature = Some(current.to_string());
    }
    if let Some(phase) = object
        .get("current_phase")
        .or_else(|| object.get("phase"))
        .and_then(Value::as_str)
    {
        progress.current_phase = Some(phase.to_string());
    }
    if let Some(failure) = object
        .get("latest_recoverable_failure")
        .or_else(|| object.get("recoverable_failure"))
        .and_then(Value::as_str)
    {
        progress.latest_recoverable_failure = Some(failure.to_string());
    }
    if let Some(blocking_reason) = object
        .get("blocking_reason")
        .or_else(|| object.get("blocking"))
        .and_then(Value::as_str)
    {
        progress.blocking_reason = Some(blocking_reason.to_string());
    }
    if let Some(known_failures) = object.get("known_failures").and_then(Value::as_array) {
        progress.known_failures = known_failures
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
    }
    if let Some(decisions) = object.get("decisions").and_then(Value::as_array) {
        progress.decisions = decisions
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
    }
    if let Some(open_questions) = object.get("open_questions").and_then(Value::as_array) {
        progress.open_questions = open_questions
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
    }
    if let Some(next_step) = object.get("next_step").and_then(Value::as_str) {
        progress.next_step = Some(next_step.to_string());
    }
    progress.updated_at = chrono::Utc::now();
    Ok(progress)
}

pub(super) fn execute_record_evaluation(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let evaluation = if let Some(value) = call.arguments.get("evaluation") {
        serde_json::from_value::<EvaluationDecision>(value.clone())
            .context("解析 evaluation 参数失败")?
    } else {
        let raw = required_string_alias(&call.arguments, &["content", "json"])?;
        serde_json::from_str::<EvaluationDecision>(&raw).context("解析 evaluation JSON 失败")?
    };
    store.append_evaluation(run, &evaluation)?;
    let artifact = store.append_artifact(
        &thread.id,
        &run.id,
        task_node_id.map(ToOwned::to_owned),
        subagent_id.map(ToOwned::to_owned),
        format!(
            "evaluation:{}",
            evaluation.feature_id.as_deref().unwrap_or("unknown")
        ),
        ArtifactKind::EvaluationSnapshot,
        run.evaluation_log_path.clone(),
    )?;
    Ok(ToolExecutionResult {
        message: "evaluation 已记录".to_string(),
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_create_session_bootstrap(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let content = if let Some(content) = call.arguments.get("content").and_then(Value::as_str) {
        content.to_string()
    } else {
        let contract = store.load_execution_contract(&thread.id).ok();
        let progress = store.load_progress_ledger(&thread.id).ok();
        format!(
            "# Session Bootstrap\n\ngoal: {}\ncurrent_feature: {}\nnext_step: {}",
            contract
                .as_ref()
                .map(|item| item.goal.as_str())
                .unwrap_or("-"),
            progress
                .as_ref()
                .and_then(|item| item.current_feature.as_deref())
                .unwrap_or("-"),
            progress
                .as_ref()
                .and_then(|item| item.next_step.as_deref())
                .unwrap_or("-")
        )
    };
    store.write_session_bootstrap(&thread.id, run, &content)?;
    let artifact = store.append_artifact(
        &thread.id,
        &run.id,
        task_node_id.map(ToOwned::to_owned),
        subagent_id.map(ToOwned::to_owned),
        "session-bootstrap".to_string(),
        ArtifactKind::SessionBootstrap,
        run.bootstrap_path.clone(),
    )?;
    Ok(ToolExecutionResult {
        message: "session bootstrap 已生成".to_string(),
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_read_memory(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let layer = parse_memory_layer(call.arguments.get("layer").and_then(Value::as_str))?;
    let mut sections = Vec::new();
    for item in selected_layers(layer) {
        let memory = store.load_memory(&thread.id, item)?;
        let label = memory_label(item);
        let content = if memory.entries.is_empty() {
            "无".to_string()
        } else {
            memory
                .entries
                .iter()
                .map(|entry| format!("- {} ({})", entry.content, entry.source))
                .collect::<Vec<_>>()
                .join("\n")
        };
        sections.push(format!("[{label}]\n{content}"));
    }
    let text = sections.join("\n\n");
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        "memory-read",
        ArtifactKind::MemorySnapshot,
        &text,
        task_node_id,
        subagent_id,
    )?;
    Ok(ToolExecutionResult {
        message: text,
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_remember_memory(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let layer = parse_memory_layer(call.arguments.get("layer").and_then(Value::as_str))?
        .unwrap_or(MemoryLayer::Working);
    let content = required_string(&call.arguments, "content")?;
    let source = call
        .arguments
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let entry = store.append_memory_entry(
        &thread.id,
        layer,
        content.clone(),
        source.clone(),
        Some(run.id.clone()),
        task_node_id.map(ToOwned::to_owned),
    )?;
    let text = format!("已写入 {} memory：{}", memory_label(layer), entry.content);
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        "memory-write",
        ArtifactKind::MemorySnapshot,
        &text,
        task_node_id,
        subagent_id,
    )?;
    Ok(ToolExecutionResult {
        message: text,
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_list_skills(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let text = SkillAdapter::list(run.backend.into())
        .into_iter()
        .map(|skill| {
            format!(
                "{}\t{}\t{}",
                skill.name,
                skill.description,
                skill.path.display()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let body = if text.is_empty() {
        "当前未发现本地 skill".to_string()
    } else {
        text
    };
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        "skill-list",
        ArtifactKind::ToolResult,
        &body,
        task_node_id,
        subagent_id,
    )?;
    Ok(ToolExecutionResult {
        message: body,
        artifacts: vec![artifact],
    })
}

pub(super) fn execute_read_skill(
    store: &HarnessStore,
    thread: &HarnessThreadManifest,
    run: &HarnessRunManifest,
    call: &ToolCallRequest,
    task_node_id: Option<&str>,
    subagent_id: Option<&str>,
) -> Result<ToolExecutionResult> {
    let name = required_string(&call.arguments, "name")?;
    let content = SkillAdapter::read_body(run.backend.into(), &name)
        .ok_or_else(|| anyhow!("未找到 skill：{name}"))?;
    let artifact = materialize_text_artifact(
        store,
        thread,
        run,
        "skill-read",
        ArtifactKind::ToolResult,
        &content,
        task_node_id,
        subagent_id,
    )?;
    Ok(ToolExecutionResult {
        message: format!("skill `{name}` 内容：\n{content}"),
        artifacts: vec![artifact],
    })
}

fn parse_memory_layer(value: Option<&str>) -> Result<Option<MemoryLayer>> {
    match value {
        None | Some("") | Some("all") => Ok(None),
        Some("working") => Ok(Some(MemoryLayer::Working)),
        Some("project") => Ok(Some(MemoryLayer::Project)),
        Some(other) => bail!("未知 memory layer：{other}"),
    }
}

fn selected_layers(layer: Option<MemoryLayer>) -> Vec<MemoryLayer> {
    match layer {
        Some(item) => vec![item],
        None => vec![MemoryLayer::Working, MemoryLayer::Project],
    }
}

fn memory_label(layer: MemoryLayer) -> &'static str {
    match layer {
        MemoryLayer::Working => "working",
        MemoryLayer::Project => "project",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::config::BackendProvider;
    use crate::harness::store::HarnessStore;
    use crate::harness::types::{SandboxState, ToolCallRequest};
    use crate::model::ThinkingMode;

    use super::{execute_apply_patch, execute_read_memory, execute_remember_memory};

    fn setup() -> (
        TempDir,
        HarnessStore,
        crate::harness::HarnessThreadManifest,
        crate::harness::HarnessRunManifest,
        SandboxState,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(dir.path(), BackendProvider::Codex);
        let thread = store.create_thread(Some("工具测试")).expect("thread");
        let run = store
            .create_run(
                &thread.id,
                Some("gpt-5".to_string()),
                ThinkingMode::Balanced,
                crate::harness::types::AgentBackendKind::Codex,
            )
            .expect("run");
        fs::create_dir_all(dir.path()).expect("mkdir");
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
    fn apply_patch_tool_updates_file() {
        let (dir, store, thread, run, sandbox) = setup();
        let path = sandbox.repo_workdir.join("demo.txt");
        fs::write(&path, "alpha\n").expect("write");
        let result = execute_apply_patch(
            &store,
            &thread,
            &run,
            &sandbox,
            &ToolCallRequest {
                name: "apply_patch".to_string(),
                arguments: serde_json::json!({
                    "path": "demo.txt",
                    "search": "alpha",
                    "replace": "beta"
                }),
            },
            None,
            None,
        )
        .expect("apply patch");
        assert!(result.message.contains("apply_patch"));
        assert_eq!(fs::read_to_string(&path).expect("read"), "beta\n");
        assert_eq!(
            fs::read_to_string(dir.path().join("demo.txt")).expect("read host"),
            "beta\n"
        );
    }

    #[test]
    fn memory_tools_roundtrip() {
        let (_dir, store, thread, run, sandbox) = setup();
        execute_remember_memory(
            &store,
            &thread,
            &run,
            &ToolCallRequest {
                name: "remember_memory".to_string(),
                arguments: serde_json::json!({
                    "layer": "project",
                    "content": "项目支持 task graph",
                    "source": "test"
                }),
            },
            None,
            None,
        )
        .expect("remember");
        let result = execute_read_memory(
            &store,
            &thread,
            &run,
            &ToolCallRequest {
                name: "read_memory".to_string(),
                arguments: serde_json::json!({"layer": "project"}),
            },
            None,
            None,
        )
        .expect("read");
        assert!(result.message.contains("项目支持 task graph"));
        assert!(sandbox.active);
    }
}
