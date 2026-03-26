use std::path::Path;

use anyhow::Result;

use crate::codex::run_plain_once;
use crate::config::AppConfig;
use crate::harness::store::HarnessStore;
use crate::harness::types::{ArtifactKind, HarnessEvent, HarnessMessageRole, HarnessRunManifest};

use super::engine::{
    ensure_sandbox_ready, finish_run, first_non_empty_line, render_memory_context,
    render_session_context, render_skills_context,
};

pub(super) async fn run_autonomous_codex_execution(
    repo_root: &Path,
    config: &AppConfig,
    store: &HarnessStore,
    run: &mut HarnessRunManifest,
) -> Result<()> {
    let thread = store.load_thread(&run.thread_id)?;
    ensure_sandbox_ready(repo_root, config, store, run)?;

    run.status = crate::harness::HarnessRunStatus::Running;
    run.blocked_reason = None;
    store.update_run(&run.thread_id, run)?;
    if run.turn_count == 0 {
        store.append_run_event(
            &run.thread_id,
            &run.id,
            HarnessEvent::RunStarted {
                thread_id: run.thread_id.clone(),
                run_id: run.id.clone(),
            },
        )?;
    }

    let messages = store.list_messages(&run.thread_id)?;
    let memory_context = render_memory_context(store, &thread.id);
    let skills_context = render_skills_context(config.backend.provider);
    let session_context =
        render_session_context(store, &thread.id, config.runtime.bootstrap_message_limit);
    let execution_root = run
        .sandbox
        .as_ref()
        .map(|sandbox| sandbox.repo_workdir.clone())
        .unwrap_or_else(|| repo_root.to_path_buf());
    let prompt = render_autonomous_codex_prompt(
        &thread.title,
        execution_root.as_path(),
        &messages,
        &memory_context,
        &skills_context,
        &session_context,
    );

    run.turn_count += 1;
    store.update_run(&run.thread_id, run)?;

    match run_plain_once(
        &prompt,
        execution_root.as_path(),
        run.model.as_deref(),
        run.thinking_mode,
        config.backend.turn_timeout_secs,
        &run.output_path,
        &run.log_path,
        2,
    )
    .await
    {
        Ok(final_response) => {
            let summary = first_non_empty_line(&final_response).to_string();
            store.append_message(
                &run.thread_id,
                HarnessMessageRole::Assistant,
                final_response,
                Some(run.id.clone()),
            )?;
            store.append_artifact(
                &run.thread_id,
                &run.id,
                None,
                None,
                "assistant-output".to_string(),
                ArtifactKind::Text,
                run.output_path.clone(),
            )?;
            store.append_artifact(
                &run.thread_id,
                &run.id,
                None,
                None,
                "codex-log".to_string(),
                ArtifactKind::SandboxLog,
                run.log_path.clone(),
            )?;
            finish_run(store, run, Some(summary), None)?;
            Ok(())
        }
        Err(error) => {
            let error_text = format!("{error:#}");
            finish_run(
                store,
                run,
                Some(first_non_empty_line(&error_text).to_string()),
                Some(error_text),
            )?;
            Err(error)
        }
    }
}

fn render_autonomous_codex_prompt(
    thread_title: &str,
    execution_root: &Path,
    messages: &[crate::harness::types::HarnessMessage],
    memory_context: &str,
    skills_context: &str,
    session_context: &str,
) -> String {
    let mut rendered = String::new();
    rendered.push_str("执行模式：autonomous_codex\n");
    rendered.push_str(
        "你是直接运行的 Codex CLI 代理，不要输出 JSON，不要模拟 tool_calls/subagent_calls。\n",
    );
    rendered.push_str("你需要基于用户目标在当前仓库内直接完成任务，默认使用中文，保持务实。\n");
    rendered.push_str("你可以自主读取、修改文件、执行命令、验证结果，并给出最终交付。\n");
    rendered.push_str(
        "如果任务较大，也由你自己规划和持续推进，不受 planner/generator/evaluator 节点限制。\n",
    );
    rendered.push_str("最后只输出给用户的最终回复正文。\n");
    rendered.push_str(&format!("线程标题：{thread_title}\n"));
    rendered.push_str(&format!("当前工作目录：{}\n", execution_root.display()));
    if !memory_context.trim().is_empty() {
        rendered.push_str("\nMemory：\n");
        rendered.push_str(memory_context);
        rendered.push('\n');
    }
    if !skills_context.trim().is_empty() {
        rendered.push_str("\nSkills：\n");
        rendered.push_str(skills_context);
        rendered.push('\n');
    }
    if !session_context.trim().is_empty() {
        rendered.push_str("\nSession Context：\n");
        rendered.push_str(session_context);
        rendered.push('\n');
    }
    rendered.push_str("\n最近消息：\n");
    for message in messages.iter().rev().take(16).rev() {
        let role = match message.role {
            HarnessMessageRole::User => "user",
            HarnessMessageRole::Assistant => "assistant",
            HarnessMessageRole::System => "system",
            HarnessMessageRole::Tool => "tool",
            HarnessMessageRole::Summary => "summary",
        };
        rendered.push_str(&format!("[{role}] {}\n\n", message.content));
    }
    rendered
}
