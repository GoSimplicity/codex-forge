use anyhow::{Result, anyhow};

use crate::cli::{ArtifactArgs, ArtifactCommands, ArtifactListArgs, ArtifactShowArgs};
use crate::commands::format::artifact_kind_label;
use crate::harness::{ArtifactKind, HarnessStore};
use crate::workspace::resolve_target_dir;

pub fn run(args: ArtifactArgs) -> Result<()> {
    match args.command {
        ArtifactCommands::List(args) => run_list(args),
        ArtifactCommands::Show(args) => run_show(args),
    }
}

fn run_list(args: ArtifactListArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let artifacts = store.list_artifacts(args.thread.as_deref(), args.run.as_deref())?;
    if artifacts.is_empty() {
        println!("当前没有 artifact");
        return Ok(());
    }
    for artifact in artifacts {
        println!(
            "{}\trun={}\tkind={}\t{}\t{}",
            artifact.id,
            artifact.run_id,
            artifact_kind_label(artifact.kind),
            artifact.label,
            artifact.path.display()
        );
    }
    Ok(())
}

fn run_show(args: ArtifactShowArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let store = HarnessStore::new(&repo_root);
    let artifacts = store.list_artifacts(args.thread.as_deref(), None)?;
    let artifact = artifacts
        .into_iter()
        .find(|artifact| artifact.id == args.artifact_id)
        .ok_or_else(|| anyhow!("未找到 artifact：{}", args.artifact_id))?;
    println!("id: {}", artifact.id);
    println!("run: {}", artifact.run_id);
    println!("kind: {}", artifact_kind_label(artifact.kind));
    println!("label: {}", artifact.label);
    println!("path: {}", artifact.path.display());
    if matches!(
        artifact.kind,
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
        println!();
        println!("{}", std::fs::read_to_string(&artifact.path)?);
    }
    Ok(())
}
