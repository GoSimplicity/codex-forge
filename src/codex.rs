use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tokio::time::sleep;

use crate::model::{
    ApplyDecision, HandoffArtifact, RuntimeEvent, WorkerLaunchSpec, WorkerResult, WorkerStatus,
};
use crate::worktree::capture_git_artifacts;

/// 启动前先确认本机真的能调用 `codex`，避免在更深层执行中才报环境错误。
pub fn ensure_codex_available() -> Result<String> {
    let path = which::which("codex").context("未找到 `codex` 命令，请先确认 Codex CLI 已安装")?;
    Ok(path.display().to_string())
}

pub async fn run_json_once(
    prompt: &str,
    cwd: &Path,
    model: Option<&str>,
    output_path: &Path,
    log_path: &Path,
    max_retries: usize,
) -> Result<String> {
    // planner / summarizer 一类结构化调用都走这里，统一处理重试与日志落盘。
    let attempts = max_retries.max(1);
    let mut last_error = None;

    for attempt in 1..=attempts {
        match run_json_attempt(prompt, cwd, model, output_path, log_path).await {
            Ok(content) => return Ok(content),
            Err(error) => {
                let retryable = classify_retryable(error.to_string().as_str());
                append_text(
                    log_path,
                    &format!("\n[attempt {attempt}/{attempts}] json call failed: {error}\n"),
                )?;
                last_error = Some(error);
                if !retryable || attempt == attempts {
                    break;
                }
                sleep(backoff_for(attempt)).await;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("未知 JSON 调用失败")))
}

async fn run_json_attempt(
    prompt: &str,
    cwd: &Path,
    model: Option<&str>,
    output_path: &Path,
    log_path: &Path,
) -> Result<String> {
    let mut command = Command::new("codex");
    configure_codex_command(&mut command);
    command
        .arg("exec")
        .arg("--skip-git-repo-check")
        .arg("--ephemeral")
        .arg("--color")
        .arg("never")
        .arg("-C")
        .arg(cwd)
        .arg("-o")
        .arg(output_path);

    if let Some(model) = model {
        command.arg("-m").arg(model);
    }

    command
        .kill_on_drop(true)
        .arg(prompt)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = command
        .output()
        .await
        .context("执行 codex exec JSON 调用失败")?;

    let mut combined = Vec::new();
    combined.extend_from_slice(&output.stdout);
    combined.extend_from_slice(b"\n--- STDERR ---\n");
    combined.extend_from_slice(&output.stderr);
    fs::write(log_path, combined)
        .with_context(|| format!("写入 Codex 调用日志失败：{}", log_path.display()))?;

    if !output.status.success() {
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{}",
            format_codex_failure("Codex JSON 调用失败", &stderr_text)
        );
    }

    if !output_path.exists() {
        bail!(
            "Codex 调用成功但没有生成结果文件：{}",
            output_path.display()
        );
    }

    let raw = fs::read_to_string(output_path)
        .with_context(|| format!("读取 JSON 输出失败：{}", output_path.display()))?;
    let json = extract_json_document(&raw)?;
    let value: Value = serde_json::from_str(&json).context("JSON 输出解析失败")?;
    let normalized = serde_json::to_string_pretty(&value).context("JSON 输出规范化失败")?;
    fs::write(output_path, &normalized)
        .with_context(|| format!("回写规范化 JSON 失败：{}", output_path.display()))?;
    Ok(normalized)
}

fn extract_json_document(text: &str) -> Result<String> {
    if serde_json::from_str::<Value>(text).is_ok() {
        return Ok(text.trim().to_string());
    }

    if let Some(block) = extract_fenced_json_block(text)
        && serde_json::from_str::<Value>(&block).is_ok()
    {
        return Ok(block);
    }

    if let Some(object) = extract_balanced_json_object(text)
        && serde_json::from_str::<Value>(&object).is_ok()
    {
        return Ok(object);
    }

    bail!("输出中未找到合法 JSON 文档")
}

fn extract_fenced_json_block(text: &str) -> Option<String> {
    let start = text.find("```json")?;
    let after = &text[start + "```json".len()..];
    let end = after.find("```")?;
    let block = after[..end].trim();
    if block.is_empty() {
        None
    } else {
        Some(block.to_string())
    }
}

fn extract_balanced_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let slice = &text[start..];
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (index, ch) in slice.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(slice[..=index].trim().to_string());
                }
            }
            _ => {}
        }
    }

    None
}

pub async fn run_worker(
    spec: WorkerLaunchSpec,
    model: Option<&str>,
    tx: mpsc::Sender<RuntimeEvent>,
    stop_rx: Option<watch::Receiver<bool>>,
) -> Result<WorkerResult> {
    // worker 是 orchestrator 调度的最小执行单元：
    // 负责写 prompt、运行 codex、解析事件、提取 handoff，并把状态回推给调度层。
    if let Some(parent) = spec.prompt_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建 worker 目录失败：{}", parent.display()))?;
    }
    fs::write(&spec.prompt_path, &spec.prompt)
        .with_context(|| format!("写入 worker prompt 失败：{}", spec.prompt_path.display()))?;

    let attempts = spec.max_retries.max(1);
    let mut last_error = None;
    let stop_rx = stop_rx;

    for attempt in 1..=attempts {
        let result = run_worker_attempt(&spec, model, tx.clone(), attempt, stop_rx.clone()).await?;
        let retryable = result.status == WorkerStatus::Failed
            && result
                .error
                .as_deref()
                .map(classify_retryable)
                .unwrap_or(false);

        if result.status == WorkerStatus::Succeeded || !retryable || attempt == attempts {
            let _ = tx
                .send(RuntimeEvent::WorkerFinished {
                    result: Box::new(result.clone()),
                })
                .await;
            return Ok(result);
        }

        last_error = result.error.clone();
        let _ = tx
            .send(RuntimeEvent::WorkerUpdate {
                agent_id: spec.agent_id.clone(),
                kind: "retry".to_string(),
                message: format!(
                    "第 {attempt} 次执行失败，将在 {}ms 后重试：{}",
                    backoff_for(attempt).as_millis(),
                    result.error.as_deref().unwrap_or("未知错误")
                ),
            })
            .await;
        sleep(backoff_for(attempt)).await;
    }

    let result = failed_result(&spec, last_error, None, attempts);
    let _ = tx
        .send(RuntimeEvent::WorkerFinished {
            result: Box::new(result.clone()),
        })
        .await;
    Ok(result)
}

async fn run_worker_attempt(
    spec: &WorkerLaunchSpec,
    model: Option<&str>,
    tx: mpsc::Sender<RuntimeEvent>,
    attempt: usize,
    mut stop_rx: Option<watch::Receiver<bool>>,
) -> Result<WorkerResult> {
    if spec.final_output_path.exists() {
        let _ = fs::remove_file(&spec.final_output_path);
    }

    let mut command = Command::new("codex");
    configure_codex_command(&mut command);
    command
        .arg("exec")
        .arg("--json")
        .arg("--skip-git-repo-check")
        .arg("--ephemeral")
        .arg("--color")
        .arg("never")
        .arg("--full-auto")
        .arg("-C")
        .arg(&spec.worktree_path)
        .arg("-o")
        .arg(&spec.final_output_path);

    if let Some(model) = model {
        command.arg("-m").arg(model);
    }

    command
        .kill_on_drop(true)
        .arg(&spec.prompt)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Ok(failed_result(spec, Some(error.to_string()), None, attempt));
        }
    };

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("无法捕获 worker stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("无法捕获 worker stderr"))?;

    let (line_tx, mut line_rx) = mpsc::channel::<LineEvent>(128);
    let stdout_task = tokio::spawn(read_stream_lines(
        "stdout",
        stdout,
        line_tx.clone(),
        spec.stdout_path.clone(),
        spec.events_path.clone(),
    ));
    let stderr_task = tokio::spawn(read_stream_lines(
        "stderr",
        stderr,
        line_tx,
        spec.stderr_path.clone(),
        spec.events_path.clone(),
    ));

    let mut cancelled = false;
    loop {
        // 这里同时监听两类输入：
        // 1. worker stdout/stderr 转出来的事件
        // 2. 来自 orchestrator / TUI 的停止信号
        if stop_rx
            .as_ref()
            .map(|receiver| *receiver.borrow())
            .unwrap_or(false)
        {
            cancelled = true;
            break;
        }

        let next_line = if let Some(receiver) = stop_rx.as_mut() {
            tokio::select! {
                maybe_line = line_rx.recv() => maybe_line,
                changed = receiver.changed() => {
                    if changed.is_ok() && *receiver.borrow() {
                        cancelled = true;
                    }
                    None
                }
            }
        } else {
            line_rx.recv().await
        };

        let Some(line_event) = next_line else {
            break;
        };
        let trimmed_line = line_event.line.trim();
        if !trimmed_line.is_empty() {
            let _ = tx
                .send(RuntimeEvent::WorkerOutput {
                    agent_id: spec.agent_id.clone(),
                    stream: line_event.stream.clone(),
                    message: trimmed_line.to_string(),
                })
                .await;
        }
        let (kind, message, _) = parse_event_line(&line_event.line);
        let kind = if line_event.stream == "stderr" {
            format!("stderr:{kind}")
        } else {
            kind
        };
        let _ = tx
            .send(RuntimeEvent::WorkerUpdate {
                agent_id: spec.agent_id.clone(),
                kind,
                message,
            })
            .await;
    }

    let status = if cancelled {
        // 停止时显式 kill 子进程，避免只退出上层 future 却留下后台 codex 进程。
        let _ = child.kill().await;
        let _ = child.wait().await;
        None
    } else {
        Some(child.wait().await.context("等待 worker 进程结束失败")?)
    };
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let final_message = fs::read_to_string(&spec.final_output_path).unwrap_or_default();
    let changed_files =
        capture_git_artifacts(&spec.worktree_path, &spec.git_status_path, &spec.diff_path)
            .await
            .unwrap_or_default();

    let handoff = parse_handoff(
        &spec.agent_id,
        &spec.role,
        &spec.task_title,
        &final_message,
        &changed_files,
    );
    if let Some(handoff) = &handoff {
        fs::write(
            &spec.handoff_path,
            serde_json::to_vec_pretty(handoff).context("序列化 handoff 失败")?,
        )
        .with_context(|| format!("写入 handoff 失败：{}", spec.handoff_path.display()))?;
        let _ = tx
            .send(RuntimeEvent::HandoffReady {
                agent_id: spec.agent_id.clone(),
                handoff_path: spec.handoff_path.clone(),
            })
            .await;
    }

    let stderr_excerpt = read_tail(&spec.stderr_path, 240).unwrap_or_default();
    if cancelled {
        return Ok(cancelled_result(spec, attempt));
    }

    let status = status.expect("cancelled 分支已提前返回");
    let error = match (status.success(), spec.final_output_path.exists()) {
        (true, true) => None,
        (true, false) => Some("worker 返回成功但缺少结构化输出文件".to_string()),
        (false, _) => Some(format_codex_failure(
            &format!(
                "worker 退出码异常：{}",
                status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "被信号终止".to_string())
            ),
            &stderr_excerpt,
        )),
    };

    let result = WorkerResult {
        agent_id: spec.agent_id.clone(),
        role: spec.role.clone(),
        task_title: spec.task_title.clone(),
        status: if error.is_none() {
            WorkerStatus::Succeeded
        } else {
            WorkerStatus::Failed
        },
        exit_code: status.code(),
        attempts: attempt,
        diagnostic_summary: build_diagnostic_summary(
            error.as_deref(),
            &spec.stdout_path,
            &spec.stderr_path,
        ),
        summary: extract_summary_section(&final_message),
        final_message,
        changed_files,
        worktree_path: spec.worktree_path.clone(),
        prompt_path: spec.prompt_path.clone(),
        stdout_path: spec.stdout_path.clone(),
        stderr_path: spec.stderr_path.clone(),
        events_path: spec.events_path.clone(),
        final_output_path: spec.final_output_path.clone(),
        diff_path: Some(spec.diff_path.clone()),
        git_status_path: Some(spec.git_status_path.clone()),
        handoff_path: handoff.as_ref().map(|_| spec.handoff_path.clone()),
        handoff,
        error,
    };

    Ok(result)
}

pub fn parse_event_line(line: &str) -> (String, String, Option<Value>) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return ("empty".to_string(), String::new(), None);
    }

    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("json")
            .to_string();
        let message = extract_message(&value).unwrap_or_else(|| compact_json(&value));
        return (kind, message, Some(value));
    }

    ("raw".to_string(), trimmed.to_string(), None)
}

#[derive(Debug)]
struct LineEvent {
    stream: String,
    line: String,
}

async fn read_stream_lines<R>(
    stream_name: &'static str,
    reader: R,
    tx: mpsc::Sender<LineEvent>,
    raw_path: std::path::PathBuf,
    event_path: std::path::PathBuf,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        append_line(&raw_path, &line)?;
        append_event_line(stream_name, &event_path, &line)?;
        if tx
            .send(LineEvent {
                stream: stream_name.to_string(),
                line,
            })
            .await
            .is_err()
        {
            break;
        }
    }
    Ok(())
}

fn append_line(path: &Path, line: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("打开日志文件失败：{}", path.display()))?;
    writeln!(file, "{line}").context("写入日志失败")
}

fn append_text(path: &Path, text: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("打开日志文件失败：{}", path.display()))?;
    write!(file, "{text}").context("写入日志失败")
}

fn append_event_line(stream_name: &str, path: &Path, line: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("打开事件文件失败：{}", path.display()))?;

    if serde_json::from_str::<Value>(line).is_ok() {
        writeln!(file, "{line}").context("写入 JSON 事件失败")
    } else {
        let wrapped = serde_json::json!({
            "type": format!("raw_{stream_name}"),
            "message": line,
        });
        writeln!(file, "{}", serde_json::to_string(&wrapped)?).context("写入包装事件失败")
    }
}

fn extract_message(value: &Value) -> Option<String> {
    let primary_candidates = [
        value.get("message").and_then(Value::as_str),
        value.get("delta").and_then(Value::as_str),
        value.get("content").and_then(Value::as_str),
        value.pointer("/content/0/text").and_then(Value::as_str),
    ];

    for text in primary_candidates.into_iter().flatten() {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    if let Some(summary) = summarize_event_message(value) {
        return Some(summary);
    }

    let secondary_candidates = [
        value.pointer("/item/text").and_then(Value::as_str),
        value.pointer("/item/content").and_then(Value::as_str),
    ];

    for text in secondary_candidates.into_iter().flatten() {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    None
}

fn summarize_event_message(value: &Value) -> Option<String> {
    match value.get("type").and_then(Value::as_str) {
        Some("thread.started") => Some("线程已启动".to_string()),
        Some("turn.started") => Some("开始新一轮响应".to_string()),
        Some("turn.completed") => Some("本轮响应结束".to_string()),
        Some("item.started") | Some("item.completed") | Some("item.updated") => {
            summarize_item_event(
                value.get("type").and_then(Value::as_str).unwrap_or("item"),
                value.get("item")?,
            )
        }
        _ => None,
    }
}

fn summarize_item_event(event_type: &str, item: &Value) -> Option<String> {
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match item_type {
        "agent_message" => item
            .get("text")
            .and_then(Value::as_str)
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty()),
        "command_execution" => summarize_command_execution_event(event_type, item),
        "todo_list" => summarize_todo_list_event(event_type, item),
        "file_change" => summarize_file_change_event(item),
        _ => item
            .get("text")
            .and_then(Value::as_str)
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty()),
    }
}

fn summarize_command_execution_event(event_type: &str, item: &Value) -> Option<String> {
    let command = item
        .get("command")
        .and_then(Value::as_str)
        .map(|text| truncate_inline(text, 72))
        .unwrap_or_else(|| "未命名命令".to_string());
    match event_type {
        "item.started" => Some(format!("命令执行中：{command}")),
        "item.completed" => {
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("completed");
            let exit_code = item
                .get("exit_code")
                .and_then(Value::as_i64)
                .map(|code| format!(" / 退出码 {code}"))
                .unwrap_or_default();
            let output_hint = item
                .get("aggregated_output")
                .and_then(Value::as_str)
                .and_then(first_non_empty_line)
                .map(|line| format!(" / {}", truncate_inline(&line, 60)))
                .unwrap_or_default();
            Some(format!(
                "命令{}：{}{}{}",
                if status == "completed" {
                    "完成"
                } else {
                    status
                },
                command,
                exit_code,
                output_hint
            ))
        }
        _ => Some(format!("命令更新：{command}")),
    }
}

fn summarize_todo_list_event(event_type: &str, item: &Value) -> Option<String> {
    let items = item.get("items")?.as_array()?;
    let total = items.len();
    let completed = items
        .iter()
        .filter(|entry| {
            entry
                .get("completed")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .count();
    Some(format!(
        "Todo {}：已完成 {completed}/{total}",
        if event_type == "item.started" {
            "已发布"
        } else {
            "已更新"
        }
    ))
}

fn summarize_file_change_event(item: &Value) -> Option<String> {
    let changes = item.get("changes")?.as_array()?;
    if changes.is_empty() {
        return Some("文件变更已记录".to_string());
    }
    let mut paths = Vec::new();
    for change in changes.iter().take(3) {
        if let Some(path) = change.get("path").and_then(Value::as_str) {
            paths.push(path.rsplit('/').next().unwrap_or(path).to_string());
        }
    }
    let suffix = if changes.len() > 3 {
        format!(" 等 {} 个文件", changes.len())
    } else {
        format!(" 共 {} 个文件", changes.len())
    };
    Some(format!("文件变更：{}{}", paths.join("、"), suffix))
}

fn first_non_empty_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.to_string())
}

fn truncate_inline(text: &str, max: usize) -> String {
    let trimmed = text.replace('\n', " ");
    if trimmed.chars().count() <= max {
        trimmed
    } else {
        format!("{}...", trimmed.chars().take(max).collect::<String>())
    }
}

fn compact_json(value: &Value) -> String {
    let raw = value.to_string();
    if raw.chars().count() > 140 {
        format!("{}...", raw.chars().take(140).collect::<String>())
    } else {
        raw
    }
}

fn failed_result(
    spec: &WorkerLaunchSpec,
    error: Option<String>,
    exit_code: Option<i32>,
    attempts: usize,
) -> WorkerResult {
    WorkerResult {
        agent_id: spec.agent_id.clone(),
        role: spec.role.clone(),
        task_title: spec.task_title.clone(),
        status: WorkerStatus::Failed,
        exit_code,
        attempts,
        diagnostic_summary: error.clone(),
        final_message: String::new(),
        summary: None,
        changed_files: Vec::new(),
        worktree_path: spec.worktree_path.clone(),
        prompt_path: spec.prompt_path.clone(),
        stdout_path: spec.stdout_path.clone(),
        stderr_path: spec.stderr_path.clone(),
        events_path: spec.events_path.clone(),
        final_output_path: spec.final_output_path.clone(),
        diff_path: Some(spec.diff_path.clone()),
        git_status_path: Some(spec.git_status_path.clone()),
        handoff_path: None,
        handoff: None,
        error,
    }
}

fn cancelled_result(spec: &WorkerLaunchSpec, attempts: usize) -> WorkerResult {
    WorkerResult {
        agent_id: spec.agent_id.clone(),
        role: spec.role.clone(),
        task_title: spec.task_title.clone(),
        status: WorkerStatus::Skipped,
        exit_code: None,
        attempts,
        diagnostic_summary: Some("用户取消执行".to_string()),
        final_message: String::new(),
        summary: None,
        changed_files: Vec::new(),
        worktree_path: spec.worktree_path.clone(),
        prompt_path: spec.prompt_path.clone(),
        stdout_path: spec.stdout_path.clone(),
        stderr_path: spec.stderr_path.clone(),
        events_path: spec.events_path.clone(),
        final_output_path: spec.final_output_path.clone(),
        diff_path: Some(spec.diff_path.clone()),
        git_status_path: Some(spec.git_status_path.clone()),
        handoff_path: None,
        handoff: None,
        error: Some("用户取消执行".to_string()),
    }
}

pub fn extract_summary_section(final_message: &str) -> Option<String> {
    let marker = "# 交付摘要";
    let start = final_message.find(marker)?;
    let after = &final_message[start + marker.len()..];
    let body = if let Some(end) = after.find("\n# ") {
        &after[..end]
    } else {
        after
    };
    let summary = body.trim();
    if summary.is_empty() {
        None
    } else {
        Some(summary.to_string())
    }
}

pub fn parse_handoff(
    agent_id: &str,
    role: &str,
    task_title: &str,
    final_message: &str,
    changed_files: &[String],
) -> Option<HandoffArtifact> {
    let sections = extract_markdown_sections(final_message);
    let summary = sections.get("交付摘要")?.trim().to_string();
    let change_intent = sections
        .get("变更清单")
        .map(|item| item.trim().to_string())
        .unwrap_or_default();
    let risks = split_lines(sections.get("风险").map(String::as_str).unwrap_or_default());
    let verification = split_lines(sections.get("验证").map(String::as_str).unwrap_or_default());
    let handoff_section = sections.get("交接").map(String::as_str).unwrap_or_default();
    let apply_decision = parse_apply_decision(handoff_section);
    let downstream_suggestions = split_non_decision_lines(handoff_section);
    let blocking_findings =
        collect_prefixed_lines(handoff_section, &["BLOCKING_FINDING:", "blocking_finding:"]);
    let accepted_scopes =
        collect_prefixed_lines(handoff_section, &["ACCEPT_SCOPE:", "accept_scope:"]);
    let rejected_scopes =
        collect_prefixed_lines(handoff_section, &["REJECT_SCOPE:", "reject_scope:"]);
    let confidence_reasoning = collect_prefixed_value(
        handoff_section,
        &["CONFIDENCE_REASONING:", "confidence_reasoning:"],
    );

    Some(HandoffArtifact {
        agent_id: agent_id.to_string(),
        role: role.to_string(),
        task_title: task_title.to_string(),
        summary,
        change_intent,
        touched_files: changed_files.to_vec(),
        risks,
        verification,
        downstream_suggestions,
        apply_decision,
        contract_scope_claim: changed_files.to_vec(),
        expected_vs_actual_artifacts: Vec::new(),
        verification_claims: split_lines(
            sections.get("验证").map(String::as_str).unwrap_or_default(),
        ),
        scope_exceptions: Vec::new(),
        blocking_findings,
        accepted_scopes,
        rejected_scopes,
        confidence_reasoning,
    })
}

fn extract_markdown_sections(text: &str) -> BTreeMap<String, String> {
    let mut sections = BTreeMap::new();
    let mut current = None::<String>;
    let mut buffer = Vec::new();

    for line in text.lines() {
        if let Some(title) = line.strip_prefix("# ") {
            if let Some(current_title) = current.take() {
                sections.insert(current_title, buffer.join("\n").trim().to_string());
                buffer.clear();
            }
            current = Some(title.trim().to_string());
        } else if current.is_some() {
            buffer.push(line.to_string());
        }
    }

    if let Some(current_title) = current {
        sections.insert(current_title, buffer.join("\n").trim().to_string());
    }

    sections
}

fn split_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.trim().trim_start_matches('-').trim())
        .filter(|line| !line.is_empty() && *line != "无")
        .map(|line| line.to_string())
        .collect()
}

fn split_non_decision_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| parse_apply_decision_line(line).is_none())
        .filter(|line| !has_prefixed_marker(line))
        .map(|line| line.trim_start_matches('-').trim())
        .filter(|line| !line.is_empty() && *line != "无")
        .map(|line| line.to_string())
        .collect()
}

fn collect_prefixed_lines(text: &str, prefixes: &[&str]) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter_map(|line| {
            let normalized = line.trim_start_matches('-').trim();
            prefixes.iter().find_map(|prefix| {
                normalized
                    .strip_prefix(prefix)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| value.to_string())
            })
        })
        .collect()
}

fn collect_prefixed_value(text: &str, prefixes: &[&str]) -> Option<String> {
    text.lines().find_map(|line| {
        let normalized = line.trim().trim_start_matches('-').trim();
        prefixes.iter().find_map(|prefix| {
            normalized
                .strip_prefix(prefix)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string())
        })
    })
}

fn has_prefixed_marker(line: &str) -> bool {
    let normalized = line.trim().trim_start_matches('-').trim();
    [
        "BLOCKING_FINDING:",
        "blocking_finding:",
        "ACCEPT_SCOPE:",
        "accept_scope:",
        "REJECT_SCOPE:",
        "reject_scope:",
        "CONFIDENCE_REASONING:",
        "confidence_reasoning:",
    ]
    .iter()
    .any(|prefix| normalized.starts_with(prefix))
}

fn parse_apply_decision(text: &str) -> Option<ApplyDecision> {
    text.lines().find_map(parse_apply_decision_line)
}

fn parse_apply_decision_line(line: &str) -> Option<ApplyDecision> {
    let normalized = line.trim().trim_start_matches('-').trim();
    let value = normalized
        .strip_prefix("APPLY_DECISION:")
        .or_else(|| normalized.strip_prefix("apply_decision:"))
        .or_else(|| normalized.strip_prefix("APPLY_DECISION="))
        .or_else(|| normalized.strip_prefix("apply_decision="))?
        .trim()
        .to_ascii_lowercase();

    match value.as_str() {
        "allow" | "allow_full" => Some(ApplyDecision::AllowFull),
        "allow_partial" => Some(ApplyDecision::AllowPartial),
        "block" => Some(ApplyDecision::Block),
        _ => None,
    }
}

fn build_diagnostic_summary(
    error: Option<&str>,
    stdout_path: &Path,
    stderr_path: &Path,
) -> Option<String> {
    error.map(|error| {
        format!(
            "{error}；stdout=`{}`；stderr=`{}`",
            stdout_path.display(),
            stderr_path.display()
        )
    })
}

fn configure_codex_command(command: &mut Command) {
    // 在允许外网的真实运行环境中，禁用 SDK telemetry 可减少无关初始化噪音。
    command.env("OTEL_SDK_DISABLED", "true");
}

fn format_codex_failure(prefix: &str, stderr: &str) -> String {
    let trimmed = stderr.trim();
    let mut message = if trimmed.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}：{trimmed}")
    };
    if let Some(hint) = codex_environment_hint(stderr) {
        message.push('；');
        message.push_str(&hint);
    }
    message
}

fn codex_environment_hint(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    if lower.contains("attempted to create a null object")
        || lower.contains("could not create otel exporter")
        || lower.contains("event loop thread panicked")
    {
        return Some(
            "检测到 Codex CLI 在受限运行环境中初始化网络或遥测失败；如果当前在沙箱或受限 CI 中运行，请改为允许外网的环境后重试".to_string(),
        );
    }
    if lower.contains("stream disconnected before completion")
        || lower.contains("error sending request for url")
    {
        return Some(
            "检测到 Codex 上游请求未完成；请优先检查当前环境的外网访问，再决定是否重试".to_string(),
        );
    }
    None
}

fn read_tail(path: &Path, max_chars: usize) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    let chars = trimmed.chars().collect::<Vec<_>>();
    let start = chars.len().saturating_sub(max_chars);
    Some(chars[start..].iter().collect::<String>())
}

fn classify_retryable(message: &str) -> bool {
    let lower = message.to_lowercase();
    if lower.contains("attempted to create a null object")
        || lower.contains("could not create otel exporter")
        || lower.contains("event loop thread panicked")
    {
        return false;
    }
    [
        "timeout",
        "timed out",
        "temporar",
        "connection reset",
        "econnreset",
        "network",
        "429",
        "missing output",
        "缺少结构化输出文件",
        "stream disconnected before completion",
    ]
    .iter()
    .any(|keyword| lower.contains(keyword))
}

fn backoff_for(attempt: usize) -> Duration {
    let millis = 300u64.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1) as u32));
    Duration::from_millis(millis.min(2_400))
}

#[cfg(test)]
mod tests {
    use super::{
        classify_retryable, codex_environment_hint, compact_json, extract_json_document,
        extract_summary_section, parse_event_line, parse_handoff,
    };
    use crate::model::ApplyDecision;
    use serde_json::json;

    #[test]
    fn parses_json_event() {
        let (kind, message, raw) = parse_event_line(r#"{"type":"turn.started","message":"hello"}"#);
        assert_eq!(kind, "turn.started");
        assert_eq!(message, "hello");
        assert!(raw.is_some());
    }

    #[test]
    fn wraps_raw_event() {
        let (kind, message, raw) = parse_event_line("plain stderr");
        assert_eq!(kind, "raw");
        assert_eq!(message, "plain stderr");
        assert!(raw.is_none());
    }

    #[test]
    fn summarizes_command_execution_event() {
        let (kind, message, raw) = parse_event_line(
            r#"{"type":"item.completed","item":{"type":"command_execution","command":"cargo test -q","aggregated_output":"ok\n","exit_code":0,"status":"completed"}}"#,
        );
        assert_eq!(kind, "item.completed");
        assert!(message.contains("命令完成"));
        assert!(message.contains("cargo test -q"));
        assert!(message.contains("ok"));
        assert!(raw.is_some());
    }

    #[test]
    fn summarizes_todo_list_event() {
        let (kind, message, _) = parse_event_line(
            r#"{"type":"item.updated","item":{"type":"todo_list","items":[{"text":"a","completed":true},{"text":"b","completed":false}]}}"#,
        );
        assert_eq!(kind, "item.updated");
        assert_eq!(message, "Todo 已更新：已完成 1/2");
    }

    #[test]
    fn extracts_summary() {
        let content = "# 交付摘要\n实现了核心逻辑\n# 风险\n无";
        assert_eq!(
            extract_summary_section(content).as_deref(),
            Some("实现了核心逻辑")
        );
    }

    #[test]
    fn parses_handoff_sections() {
        let handoff = parse_handoff(
            "implementer-1",
            "implementer",
            "实现主链路",
            "# 交付摘要\n已完成\n# 变更清单\n- 修改 src/main.rs\n# 风险\n- 无\n# 验证\n- cargo test\n# 交接\n- reviewer 看一下边界",
            &["src/main.rs".to_string()],
        )
        .expect("handoff");

        assert_eq!(handoff.summary, "已完成");
        assert_eq!(handoff.touched_files, vec!["src/main.rs".to_string()]);
        assert_eq!(handoff.verification, vec!["cargo test".to_string()]);
        assert_eq!(handoff.apply_decision, None);
    }

    #[test]
    fn parses_reviewer_apply_decision() {
        let handoff = parse_handoff(
            "reviewer-1",
            "reviewer",
            "集成审阅",
            "# 交付摘要\n允许进入 apply\n# 变更清单\n- 无\n# 风险\n- 无\n# 验证\n- 检查 handoff\n# 交接\n- APPLY_DECISION: allow\n- integration 可以继续",
            &[],
        )
        .expect("handoff");

        assert_eq!(handoff.apply_decision, Some(ApplyDecision::AllowFull));
        assert_eq!(handoff.downstream_suggestions, vec!["integration 可以继续"]);
    }

    #[test]
    fn parses_reviewer_partial_and_block_decision() {
        let partial = parse_handoff(
            "reviewer-1",
            "reviewer",
            "集成审阅",
            "# 交付摘要\n部分放行\n# 变更清单\n- 无\n# 风险\n- 需要人工复核\n# 验证\n- 检查 handoff\n# 交接\n- APPLY_DECISION: allow_partial\n- 仅放行安全文件",
            &[],
        )
        .expect("partial handoff");
        assert_eq!(partial.apply_decision, Some(ApplyDecision::AllowPartial));

        let block = parse_handoff(
            "reviewer-1",
            "reviewer",
            "集成审阅",
            "# 交付摘要\n阻止应用\n# 变更清单\n- 无\n# 风险\n- 存在阻断问题\n# 验证\n- 检查 handoff\n# 交接\n- APPLY_DECISION: block\n- 先修复阻断项",
            &[],
        )
        .expect("block handoff");
        assert_eq!(block.apply_decision, Some(ApplyDecision::Block));
    }

    #[test]
    fn compact_json_handles_multibyte_text() {
        let value = json!({
            "item": {
                "text": "查看仓库上下文与约束".repeat(30)
            }
        });

        let compact = compact_json(&value);
        assert!(compact.ends_with("..."));
    }

    #[test]
    fn extracts_json_document_from_fenced_block() {
        let text = "这里是说明\n```json\n{\"ok\":\"hello\"}\n```\n更多说明";
        let json = extract_json_document(text).expect("json");
        assert_eq!(json, "{\"ok\":\"hello\"}");
    }

    #[test]
    fn extracts_json_document_from_prose_wrapped_object() {
        let text = "我会按要求输出。\n{\"ok\":\"hello\",\"items\":[1,2,3]}\n请查收";
        let json = extract_json_document(text).expect("json");
        assert!(json.contains("\"ok\":\"hello\""));
    }

    #[test]
    fn codex_environment_hint_detects_sandbox_runtime_failure() {
        let hint = codex_environment_hint(
            "Attempted to create a NULL object. Could not create otel exporter: panicked during initialization",
        );
        assert!(hint.is_some());
    }

    #[test]
    fn classify_retryable_excludes_sandbox_runtime_failure() {
        assert!(!classify_retryable(
            "Attempted to create a NULL object. Could not create otel exporter"
        ));
        assert!(classify_retryable(
            "stream disconnected before completion: error sending request for url"
        ));
    }
}
