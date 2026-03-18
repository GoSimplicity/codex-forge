use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::sleep;

use crate::model::{RuntimeEventRecord, UiMode};
use crate::session::load_session;
use crate::ui::UiController;

pub async fn replay_session(
    target_dir: &Path,
    session_id: Option<&str>,
    ui_mode: UiMode,
) -> Result<()> {
    let manifest = load_session(target_dir, session_id)?;
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
