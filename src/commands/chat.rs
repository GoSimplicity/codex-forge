use anyhow::{Result, bail};

use crate::cli::{ChatArgs, ThinkingModeArg};
use crate::commands::format::{status_label, waiting_action_hint};
use crate::config::load_app_config;
use crate::harness::{ChatRequest, HarnessStore, chat_once};
use crate::model::ThinkingMode;
use crate::workspace::resolve_target_dir;

pub async fn run(args: ChatArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let config = load_app_config(&repo_root)?;
    let store = HarnessStore::new(&repo_root, config.backend.provider);
    let thread_id = match args.thread {
        Some(thread_id) => thread_id,
        None => store.create_thread(args.title.as_deref())?.id,
    };
    let outcome = chat_once(
        &repo_root,
        &config,
        ChatRequest {
            thread_id: thread_id.clone(),
            message: validated_input(&args.message)?,
            model: args.model,
            thinking_mode: args
                .thinking_mode
                .map(into_thinking_mode)
                .unwrap_or_default(),
        },
    )
    .await?;

    println!("thread: {}", thread_id);
    println!("run: {}", outcome.run.id);
    println!("status: {}", status_label(outcome.run.status));
    println!();
    if let Some(message) = outcome.assistant_message {
        println!("{}", message.content.trim());
    } else {
        let summary = outcome.run.summary.as_deref().unwrap_or("已进入下一阶段");
        println!("{}", summary);
    }
    if let Some(active_node_id) = outcome.run.active_task_node_id.as_deref() {
        let active_node = store.load_task_node(&outcome.run, active_node_id).ok();
        let pending_approval = store
            .list_pending_approvals(Some(&thread_id))?
            .into_iter()
            .find(|approval| approval.run_id == outcome.run.id);
        if let Some(hint) = waiting_action_hint(
            &outcome.run,
            active_node.as_ref(),
            pending_approval.as_ref(),
        ) {
            println!();
            println!("{hint}");
        }
    }
    Ok(())
}

fn validated_input(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("输入不能为空");
    }
    Ok(trimmed.to_string())
}

fn into_thinking_mode(value: ThinkingModeArg) -> ThinkingMode {
    match value {
        ThinkingModeArg::Quick => ThinkingMode::Quick,
        ThinkingModeArg::Balanced => ThinkingMode::Balanced,
        ThinkingModeArg::HardThink => ThinkingMode::HardThink,
    }
}
