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

fn describe_timeline_event(event: &crate::model::RuntimeEvent) -> String {
    match event {
        crate::model::RuntimeEvent::PhaseChanged { phase } => format!("阶段切换 -> {phase}"),
        crate::model::RuntimeEvent::CommanderNote { message } => format!("指挥备注：{message}"),
        crate::model::RuntimeEvent::GraphReady {
            nodes,
            dependencies,
        } => format!("执行图就绪：节点 {nodes} / 依赖 {dependencies}"),
        crate::model::RuntimeEvent::TodoStateChanged {
            todo_id,
            title,
            status,
            message,
            ..
        } => format!("{todo_id} {title} -> {} / {message}", status.label()),
        crate::model::RuntimeEvent::WorkerDispatched {
            agent_id,
            role,
            title,
            ..
        } => format!("启动 {agent_id} / {role} / {title}"),
        crate::model::RuntimeEvent::WorkerUpdate {
            agent_id,
            kind,
            message,
        } => format!("{agent_id} [{kind}] {message}"),
        crate::model::RuntimeEvent::HandoffReady {
            agent_id,
            handoff_path,
        } => format!("交接就绪 {agent_id} -> {}", handoff_path.display()),
        crate::model::RuntimeEvent::WorkerFinished { result } => {
            format!("{} 完成：{}", result.agent_id, result.status.label())
        }
        crate::model::RuntimeEvent::ApplyPlanReady { mode, operations } => {
            format!("apply 计划：{} / {} 个 patch", mode, operations)
        }
        crate::model::RuntimeEvent::ReviewGateReady { report } => format!(
            "review gate：{} / {}",
            report.decision.label(),
            report
                .confidence_reasoning
                .clone()
                .unwrap_or_else(|| "无补充说明".to_string())
        ),
        crate::model::RuntimeEvent::ApplyUpdate { message } => format!("应用更新：{message}"),
        crate::model::RuntimeEvent::VerificationReady {
            stage,
            success,
            message,
        } => format!(
            "验证 {stage}：{} / {message}",
            if *success { "成功" } else { "失败" }
        ),
        crate::model::RuntimeEvent::SummaryReady { summary } => format!("总结：{}", summary.overview),
    }
}
