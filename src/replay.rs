use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{mpsc::UnboundedSender, watch};
use tokio::time::sleep;

use crate::model::{RuntimeEvent, RuntimeEventRecord, SessionManifest, UiMode};
use crate::session::load_session;
use crate::ui::{UiController, describe_runtime_event};

pub async fn replay_session(
    target_dir: &Path,
    session_id: Option<&str>,
    ui_mode: UiMode,
    timeline_only: bool,
) -> Result<()> {
    let manifest = load_session(target_dir, session_id)?;

    if timeline_only {
        let timeline = fs::read_to_string(&manifest.timeline_path)
            .with_context(|| format!("读取 timeline 失败：{}", manifest.timeline_path.display()))?;
        println!("Session：{}", manifest.id);
        println!("任务：{}", manifest.task);
        for line in timeline.lines().filter(|line| !line.trim().is_empty()) {
            let record: RuntimeEventRecord =
                serde_json::from_str(line).context("解析 timeline 事件失败")?;
            println!(
                "- {} | {}",
                record.ts.format("%H:%M:%S"),
                describe_timeline_event(&record.payload)
            );
        }
        if let Some(summary) = &manifest.final_summary {
            println!("总结：{}", summary.overview);
        }
        return Ok(());
    }

    let mut ui = UiController::new(&manifest.id, &manifest.task, ui_mode)?;

    let timeline = fs::read_to_string(&manifest.timeline_path)
        .with_context(|| format!("读取 timeline 失败：{}", manifest.timeline_path.display()))?;

    for line in timeline.lines().filter(|line| !line.trim().is_empty()) {
        let record: RuntimeEventRecord =
            serde_json::from_str(line).context("解析 timeline 事件失败")?;
        ui.apply(&record.payload)?;
        sleep(Duration::from_millis(80)).await;
    }

    ui.finish()?;

    println!("回放完成：`{}`", manifest.id);
    if let Some(summary) = &manifest.final_summary {
        println!("总览：{}", summary.overview);
        println!(
            "Markdown 摘要：`{}`",
            manifest.summary_markdown_path.display()
        );
    }
    Ok(())
}

pub async fn replay_session_embedded(
    target_dir: &Path,
    session_id: Option<&str>,
    event_tx: UnboundedSender<RuntimeEvent>,
    mut stop_rx: Option<watch::Receiver<bool>>,
) -> Result<(SessionManifest, bool)> {
    // 内嵌回放和 Rich UI 回放的区别在于：
    // 这里只负责把 timeline 重新转成 RuntimeEvent，供 AppShell 自己渲染。
    let manifest = load_session(target_dir, session_id)?;
    let timeline = fs::read_to_string(&manifest.timeline_path)
        .with_context(|| format!("读取 timeline 失败：{}", manifest.timeline_path.display()))?;
    let mut stopped = false;

    for line in timeline.lines().filter(|line| !line.trim().is_empty()) {
        if stop_rx
            .as_ref()
            .map(|receiver| *receiver.borrow())
            .unwrap_or(false)
        {
            stopped = true;
            break;
        }
        let record: RuntimeEventRecord =
            serde_json::from_str(line).context("解析 timeline 事件失败")?;
        let _ = event_tx.send(record.payload.clone());
        // 回放也支持“中途停止”，因此等待期间同样要监听 stop signal。
        if let Some(receiver) = stop_rx.as_mut() {
            tokio::select! {
                _ = sleep(Duration::from_millis(80)) => {}
                changed = receiver.changed() => {
                    if changed.is_ok() && *receiver.borrow() {
                        stopped = true;
                        break;
                    }
                }
            }
        } else {
            sleep(Duration::from_millis(80)).await;
        }
    }

    Ok((manifest, stopped))
}

fn describe_timeline_event(event: &crate::model::RuntimeEvent) -> String {
    describe_runtime_event(event)
}
