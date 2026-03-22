use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::model::{
    ContinuationConfig, FeedbackRecord, FinalSummary, MemoryEntry, MemoryEntryKind, MemoryManifest,
    MemoryScope, MemoryView, ReviewGateReport, SessionManifest, VerificationReport, WorkerResult,
};

const MAX_HISTORY_SESSIONS: usize = 3;

#[derive(Debug, Clone)]
pub struct CommanderMemoryContext {
    pub prompt_block: String,
    pub entries: usize,
    pub sessions: usize,
}

#[derive(Debug, Clone)]
pub struct MaterializedMemoryView {
    pub view: MemoryView,
    pub markdown_path: PathBuf,
}

pub fn build_commander_memory_context(
    repo_root: &Path,
    current_session_id: Option<&str>,
    continuation: Option<&ContinuationConfig>,
) -> Result<CommanderMemoryContext> {
    let mut entries = Vec::new();
    if let Some(continuation) = continuation {
        entries.extend(entries_from_feedback(
            &continuation.parent_session_id,
            &continuation.feedback_history,
        ));
    }

    let manifests =
        recent_completed_manifests(repo_root, current_session_id, MAX_HISTORY_SESSIONS)?;
    for manifest in &manifests {
        entries.extend(entries_from_manifest(manifest, true));
    }

    dedup_entries(&mut entries);
    let prompt_block = if entries.is_empty() {
        "共享记忆摘要：\n- 当前仓库还没有可复用的共享记忆，请基于当前任务和仓库事实推进。\n\n"
            .to_string()
    } else {
        let lines = entries
            .iter()
            .take(6)
            .map(|entry| {
                let detail = if entry.details.is_empty() {
                    entry.summary.clone()
                } else {
                    format!("{}；{}", entry.summary, entry.details.join("；"))
                };
                format!(
                    "- [{}] {}：{}",
                    entry.kind.label(),
                    entry.title,
                    truncate(&detail, 160)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "共享记忆摘要：\n- 复用历史 session：{}\n- 可消费记忆条目：{}\n{}\n\n",
            manifests.len(),
            entries.len(),
            lines
        )
    };

    Ok(CommanderMemoryContext {
        prompt_block,
        entries: entries.len(),
        sessions: manifests.len(),
    })
}

pub fn build_worker_memory_view(
    manifest: &SessionManifest,
    agent_id: &str,
    role: &str,
    dependency_results: &[&WorkerResult],
) -> Result<MaterializedMemoryView> {
    let mut entries = Vec::new();
    entries.extend(entries_from_feedback(
        &manifest.id,
        &manifest.feedback_history,
    ));
    for result in dependency_results {
        if let Some(handoff) = &result.handoff {
            let mut details = handoff.risks.clone();
            if !handoff.downstream_suggestions.is_empty() {
                details.push(format!(
                    "后续建议：{}",
                    handoff.downstream_suggestions.join("；")
                ));
            }
            entries.push(MemoryEntry {
                id: format!("{}:{}:handoff", manifest.id, result.agent_id),
                scope: MemoryScope::Session,
                kind: MemoryEntryKind::Handoff,
                title: format!("{} 的上游交接", result.agent_id),
                summary: handoff.summary.clone(),
                details,
                tags: handoff.touched_files.clone(),
                role_hints: vec![role.to_string()],
                source_session_id: manifest.id.clone(),
                source_agent_id: Some(result.agent_id.clone()),
                created_at: manifest.created_at,
            });
        }
    }

    for history in recent_completed_manifests(
        &manifest.repo_snapshot.repo_root,
        Some(&manifest.id),
        MAX_HISTORY_SESSIONS,
    )? {
        entries.extend(entries_from_manifest(&history, false));
    }

    dedup_entries(&mut entries);
    entries.retain(|entry| {
        entry.role_hints.is_empty() || entry.role_hints.iter().any(|item| item == role)
    });

    let summary = if entries.is_empty() {
        format!("{agent_id} 当前没有可复用共享记忆，按节点约束直接推进。")
    } else {
        format!(
            "{agent_id} 共加载 {} 条共享记忆，优先吸收反馈、已验证事实和直接上游交接。",
            entries.len()
        )
    };
    let view = MemoryView {
        id: format!("{}:{agent_id}", manifest.id),
        role: role.to_string(),
        agent_id: agent_id.to_string(),
        summary,
        generated_at: Utc::now(),
        entries,
    };

    let json_path = view_json_path(&manifest.repo_snapshot.repo_root, &manifest.id, agent_id);
    let markdown_path =
        view_markdown_path(&manifest.repo_snapshot.repo_root, &manifest.id, agent_id);
    ensure_parent(&json_path)?;
    ensure_parent(&markdown_path)?;
    fs::write(
        &json_path,
        serde_json::to_vec_pretty(&view).context("序列化 memory view 失败")?,
    )
    .with_context(|| format!("写入 memory view JSON 失败：{}", json_path.display()))?;
    fs::write(&markdown_path, render_memory_view_markdown(&view)).with_context(|| {
        format!(
            "写入 memory view Markdown 失败：{}",
            markdown_path.display()
        )
    })?;

    Ok(MaterializedMemoryView {
        view,
        markdown_path,
    })
}

pub fn append_worker_memory(
    repo_root: &Path,
    session_id: &str,
    result: &WorkerResult,
) -> Result<usize> {
    let mut entries = Vec::new();
    if let Some(handoff) = &result.handoff {
        entries.push(MemoryEntry {
            id: format!("{session_id}:{}:handoff", result.agent_id),
            scope: MemoryScope::Session,
            kind: MemoryEntryKind::Handoff,
            title: format!("{} 节点交付", result.agent_id),
            summary: handoff.summary.clone(),
            details: handoff
                .risks
                .iter()
                .chain(handoff.verification.iter())
                .cloned()
                .collect(),
            tags: handoff.touched_files.clone(),
            role_hints: vec![
                "architect".to_string(),
                "implementer".to_string(),
                "reviewer".to_string(),
                "tester".to_string(),
            ],
            source_session_id: session_id.to_string(),
            source_agent_id: Some(result.agent_id.clone()),
            created_at: Utc::now(),
        });

        if let Some(decision) = handoff.apply_decision {
            entries.push(memory_entry_from_review_gate(
                session_id,
                &result.agent_id,
                &ReviewGateReport {
                    decision,
                    blocking_findings: handoff.blocking_findings.clone(),
                    accepted_scopes: handoff.accepted_scopes.clone(),
                    rejected_scopes: handoff.rejected_scopes.clone(),
                    confidence_reasoning: handoff.confidence_reasoning.clone(),
                },
            ));
        }
    }

    append_entries(repo_root, session_id, &entries)?;
    Ok(entries.len())
}

pub fn append_verification_memory(
    repo_root: &Path,
    session_id: &str,
    report: &VerificationReport,
) -> Result<usize> {
    let mut details = Vec::new();
    if !report.verified_capabilities.is_empty() {
        details.push(format!(
            "已验证：{}",
            report.verified_capabilities.join("；")
        ));
    }
    if !report.blocked_verifications.is_empty() {
        details.push(format!(
            "环境阻塞：{}",
            report.blocked_verifications.join("；")
        ));
    }
    if !report.failed_capabilities.is_empty() {
        details.push(format!("失败项：{}", report.failed_capabilities.join("；")));
    }
    let entry = MemoryEntry {
        id: format!("{session_id}:verification"),
        scope: MemoryScope::Shared,
        kind: MemoryEntryKind::Verification,
        title: "全链路验证结论".to_string(),
        summary: format!("验证总体状态：{}", report.overall_status.label()),
        details,
        tags: vec!["verification".to_string()],
        role_hints: vec![
            "implementer".to_string(),
            "reviewer".to_string(),
            "tester".to_string(),
        ],
        source_session_id: session_id.to_string(),
        source_agent_id: None,
        created_at: Utc::now(),
    };
    append_entries(repo_root, session_id, &[entry])?;
    Ok(1)
}

pub fn append_summary_memory(
    repo_root: &Path,
    manifest: &SessionManifest,
    summary: &FinalSummary,
) -> Result<usize> {
    let task_brief = render_task_brief(manifest, summary);
    let task_brief_path = task_brief_path(repo_root, &manifest.id);
    ensure_parent(&task_brief_path)?;
    fs::write(&task_brief_path, task_brief)
        .with_context(|| format!("写入 task brief 失败：{}", task_brief_path.display()))?;

    let entry = MemoryEntry {
        id: format!("{}:summary", manifest.id),
        scope: MemoryScope::Shared,
        kind: MemoryEntryKind::Summary,
        title: format!("Session {} 总结", manifest.id),
        summary: summary.overview.clone(),
        details: summary
            .recommended_next_action
            .iter()
            .chain(summary.open_risks.iter())
            .cloned()
            .collect(),
        tags: summary.accepted_files.clone(),
        role_hints: vec![
            "architect".to_string(),
            "implementer".to_string(),
            "reviewer".to_string(),
            "tester".to_string(),
        ],
        source_session_id: manifest.id.clone(),
        source_agent_id: None,
        created_at: Utc::now(),
    };
    append_entries(repo_root, &manifest.id, &[entry])?;
    Ok(1)
}

pub fn sync_memory_manifest(manifest: &mut SessionManifest) -> Result<()> {
    let repo_root = &manifest.repo_snapshot.repo_root;
    let session_id = manifest.id.clone();
    let shared_index_path = shared_index_path(repo_root);
    let session_entries_path = session_entries_path(repo_root, &session_id);
    let task_brief_path = task_brief_path(repo_root, &session_id);
    let manifest_path = memory_manifest_path(repo_root, &session_id);
    ensure_parent(&shared_index_path)?;
    ensure_parent(&session_entries_path)?;
    ensure_parent(&task_brief_path)?;
    ensure_parent(&manifest_path)?;

    if !shared_index_path.exists() {
        fs::write(&shared_index_path, "[]").with_context(|| {
            format!(
                "初始化 shared memory index 失败：{}",
                shared_index_path.display()
            )
        })?;
    }
    if !session_entries_path.exists() {
        fs::write(&session_entries_path, "[]").with_context(|| {
            format!(
                "初始化 session memory entries 失败：{}",
                session_entries_path.display()
            )
        })?;
    }
    if !task_brief_path.exists() {
        fs::write(&task_brief_path, "当前 session 尚未形成 task brief。\n")
            .with_context(|| format!("初始化 task brief 失败：{}", task_brief_path.display()))?;
    }

    let views_dir = session_memory_dir(repo_root, &session_id).join("views");
    let mut view_paths = if views_dir.exists() {
        fs::read_dir(&views_dir)
            .with_context(|| format!("读取 memory views 失败：{}", views_dir.display()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|item| item.to_str()) == Some("json"))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    view_paths.sort();

    let memory_manifest = MemoryManifest {
        version: manifest.shared_context_version,
        manifest_path: manifest_path.clone(),
        shared_index_path: shared_index_path.clone(),
        session_entries_path: session_entries_path.clone(),
        task_brief_path: task_brief_path.clone(),
        view_paths,
    };
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&memory_manifest).context("序列化 memory manifest 失败")?,
    )
    .with_context(|| format!("写入 memory manifest 失败：{}", manifest_path.display()))?;

    manifest.memory_manifest = Some(memory_manifest);
    manifest.artifact_manifest.memory_manifest_path = Some(manifest_path);
    manifest.artifact_manifest.shared_memory_index_path = Some(shared_index_path);
    manifest.artifact_manifest.session_memory_entries_path = Some(session_entries_path);
    manifest.artifact_manifest.task_brief_path = Some(task_brief_path);
    Ok(())
}

fn append_entries(repo_root: &Path, session_id: &str, entries: &[MemoryEntry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let session_path = session_entries_path(repo_root, session_id);
    let shared_path = shared_index_path(repo_root);
    ensure_parent(&session_path)?;
    ensure_parent(&shared_path)?;

    let mut session_entries = load_entries(&session_path)?;
    upsert_entries(&mut session_entries, entries);
    fs::write(
        &session_path,
        serde_json::to_vec_pretty(&session_entries)
            .context("序列化 session memory entries 失败")?,
    )
    .with_context(|| {
        format!(
            "写入 session memory entries 失败：{}",
            session_path.display()
        )
    })?;

    let mut shared_entries = load_entries(&shared_path)?;
    upsert_entries(&mut shared_entries, entries);
    fs::write(
        &shared_path,
        serde_json::to_vec_pretty(&shared_entries).context("序列化 shared memory index 失败")?,
    )
    .with_context(|| format!("写入 shared memory index 失败：{}", shared_path.display()))?;
    Ok(())
}

fn upsert_entries(target: &mut Vec<MemoryEntry>, incoming: &[MemoryEntry]) {
    let incoming_ids = incoming
        .iter()
        .map(|item| item.id.as_str())
        .collect::<HashSet<_>>();
    target.retain(|item| !incoming_ids.contains(item.id.as_str()));
    target.extend_from_slice(incoming);
}

fn load_entries(path: &Path) -> Result<Vec<MemoryEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("读取 memory entries 失败：{}", path.display()))?;
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

fn entries_from_feedback(
    session_id: &str,
    feedback_history: &[FeedbackRecord],
) -> Vec<MemoryEntry> {
    feedback_history
        .iter()
        .enumerate()
        .rev()
        .take(3)
        .map(|(index, item)| MemoryEntry {
            id: format!("{session_id}:feedback:{index}"),
            scope: MemoryScope::Shared,
            kind: MemoryEntryKind::Feedback,
            title: item.title.clone().unwrap_or_else(|| "人类反馈".to_string()),
            summary: item.intent_summary.clone(),
            details: item.scope_delta.clone(),
            tags: vec!["feedback".to_string()],
            role_hints: vec![
                "architect".to_string(),
                "implementer".to_string(),
                "reviewer".to_string(),
                "tester".to_string(),
            ],
            source_session_id: session_id.to_string(),
            source_agent_id: None,
            created_at: item.created_at,
        })
        .collect()
}

fn entries_from_manifest(manifest: &SessionManifest, commander_mode: bool) -> Vec<MemoryEntry> {
    let mut entries = Vec::new();
    if let Some(summary) = &manifest.final_summary {
        entries.push(MemoryEntry {
            id: format!("{}:summary", manifest.id),
            scope: MemoryScope::Shared,
            kind: MemoryEntryKind::Summary,
            title: format!("Session {} 总结", manifest.id),
            summary: summary.overview.clone(),
            details: summary
                .recommended_next_action
                .iter()
                .chain(summary.open_risks.iter())
                .take(6)
                .cloned()
                .collect(),
            tags: summary.accepted_files.clone(),
            role_hints: vec![
                "architect".to_string(),
                "implementer".to_string(),
                "reviewer".to_string(),
                "tester".to_string(),
            ],
            source_session_id: manifest.id.clone(),
            source_agent_id: None,
            created_at: manifest.created_at,
        });
        if let Some(review_report) = &summary.review_report {
            entries.push(memory_entry_from_review_gate(
                &manifest.id,
                "reviewer-1",
                review_report,
            ));
        }
    }
    if let Some(report) = &manifest.verification_report {
        let mut details = report.verified_capabilities.clone();
        details.extend(report.blocked_verifications.clone());
        entries.push(MemoryEntry {
            id: format!("{}:verification", manifest.id),
            scope: MemoryScope::Shared,
            kind: MemoryEntryKind::Verification,
            title: "历史验证结论".to_string(),
            summary: format!("验证总体状态：{}", report.overall_status.label()),
            details: details.into_iter().take(6).collect(),
            tags: vec!["verification".to_string()],
            role_hints: if commander_mode {
                vec!["architect".to_string()]
            } else {
                vec![
                    "implementer".to_string(),
                    "reviewer".to_string(),
                    "tester".to_string(),
                ]
            },
            source_session_id: manifest.id.clone(),
            source_agent_id: None,
            created_at: manifest.created_at,
        });
    }
    entries
}

fn memory_entry_from_review_gate(
    session_id: &str,
    agent_id: &str,
    review_report: &ReviewGateReport,
) -> MemoryEntry {
    let mut details = review_report.blocking_findings.clone();
    if !review_report.accepted_scopes.is_empty() {
        details.push(format!(
            "放行范围：{}",
            review_report.accepted_scopes.join("；")
        ));
    }
    if !review_report.rejected_scopes.is_empty() {
        details.push(format!(
            "拦截范围：{}",
            review_report.rejected_scopes.join("；")
        ));
    }
    MemoryEntry {
        id: format!("{session_id}:{agent_id}:review-gate"),
        scope: MemoryScope::Shared,
        kind: MemoryEntryKind::ReviewGate,
        title: "reviewer gate 结论".to_string(),
        summary: review_report.decision.label().to_string(),
        details,
        tags: vec!["review".to_string()],
        role_hints: vec![
            "implementer".to_string(),
            "reviewer".to_string(),
            "tester".to_string(),
        ],
        source_session_id: session_id.to_string(),
        source_agent_id: Some(agent_id.to_string()),
        created_at: Utc::now(),
    }
}

fn recent_completed_manifests(
    repo_root: &Path,
    exclude_session_id: Option<&str>,
    limit: usize,
) -> Result<Vec<SessionManifest>> {
    let sessions_root = repo_root.join(".codex-forge").join("sessions");
    if !sessions_root.exists() {
        return Ok(Vec::new());
    }

    let mut manifests = fs::read_dir(&sessions_root)
        .with_context(|| format!("读取 sessions 目录失败：{}", sessions_root.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join("manifest.json"))
        .filter(|path| path.exists())
        .filter_map(|path| {
            let raw = fs::read_to_string(&path).ok()?;
            serde_json::from_str::<SessionManifest>(&raw).ok()
        })
        .filter(|manifest| manifest.continuable())
        .filter(|manifest| exclude_session_id.is_none_or(|id| manifest.id != id))
        .collect::<Vec<_>>();

    manifests.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    manifests.truncate(limit);
    Ok(manifests)
}

fn dedup_entries(entries: &mut Vec<MemoryEntry>) {
    let mut seen = HashSet::new();
    entries.retain(|entry| seen.insert(entry.id.clone()));
}

fn render_memory_view_markdown(view: &MemoryView) -> String {
    let body = if view.entries.is_empty() {
        "- 无可复用共享记忆".to_string()
    } else {
        view.entries
            .iter()
            .map(|entry| {
                let details = if entry.details.is_empty() {
                    String::new()
                } else {
                    format!("\n  - {}", entry.details.join("\n  - "))
                };
                format!(
                    "- [{}] {}：{}{}",
                    entry.kind.label(),
                    entry.title,
                    entry.summary,
                    details
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "# 共享记忆视图\n\n- agent：{}\n- role：{}\n- 生成时间：{}\n- 摘要：{}\n\n## 条目\n\n{}\n",
        view.agent_id, view.role, view.generated_at, view.summary, body
    )
}

pub fn render_memory_prompt_block(view: &MemoryView) -> String {
    if view.entries.is_empty() {
        "共享记忆视图：\n- 当前没有可复用共享记忆，请严格按当前节点目标、上游 handoff 和仓库事实推进。\n"
            .to_string()
    } else {
        let entries = view
            .entries
            .iter()
            .map(|entry| {
                let details = if entry.details.is_empty() {
                    String::new()
                } else {
                    format!("；{}", truncate(&entry.details.join("；"), 160))
                };
                format!(
                    "- [{}] {}：{}{}",
                    entry.kind.label(),
                    entry.title,
                    entry.summary,
                    details
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("共享记忆视图：\n- 摘要：{}\n{}\n", view.summary, entries)
    }
}

fn render_task_brief(manifest: &SessionManifest, summary: &FinalSummary) -> String {
    format!(
        "# Task Brief\n\n- session：{}\n- 任务：{}\n- 总览：{}\n- 结果：{}\n- reviewer gate：{}\n- apply：{}\n\n## 已验证能力\n\n{}\n\n## 风险\n\n{}\n\n## 下一步\n\n{}\n",
        manifest.id,
        manifest.task,
        summary.overview,
        summary.result_status.label(),
        summary
            .review_gate
            .map(|item| item.label().to_string())
            .unwrap_or_else(|| "无".to_string()),
        summary.apply_status.label(),
        render_bullets(&summary.verified_capabilities),
        render_bullets(&summary.open_risks),
        render_bullets(&summary.recommended_next_action),
    )
}

fn render_bullets(items: &[String]) -> String {
    if items.is_empty() {
        "- 无".to_string()
    } else {
        items
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败：{}", parent.display()))?;
    }
    Ok(())
}

fn memory_root(repo_root: &Path) -> PathBuf {
    repo_root.join(".codex-forge").join("memory")
}

fn session_memory_dir(repo_root: &Path, session_id: &str) -> PathBuf {
    memory_root(repo_root).join("session").join(session_id)
}

fn shared_index_path(repo_root: &Path) -> PathBuf {
    memory_root(repo_root).join("shared").join("index.json")
}

fn session_entries_path(repo_root: &Path, session_id: &str) -> PathBuf {
    session_memory_dir(repo_root, session_id).join("entries.json")
}

fn task_brief_path(repo_root: &Path, session_id: &str) -> PathBuf {
    session_memory_dir(repo_root, session_id).join("task-brief.md")
}

fn memory_manifest_path(repo_root: &Path, session_id: &str) -> PathBuf {
    session_memory_dir(repo_root, session_id).join("manifest.json")
}

fn view_json_path(repo_root: &Path, session_id: &str, agent_id: &str) -> PathBuf {
    session_memory_dir(repo_root, session_id)
        .join("views")
        .join(format!("{agent_id}.json"))
}

fn view_markdown_path(repo_root: &Path, session_id: &str, agent_id: &str) -> PathBuf {
    session_memory_dir(repo_root, session_id)
        .join("views")
        .join(format!("{agent_id}.md"))
}

fn truncate(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let preview = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

#[cfg(test)]
mod tests {
    use super::{build_commander_memory_context, render_memory_prompt_block, sync_memory_manifest};
    use crate::model::{
        ApplyMode, ArtifactManifest, FinalSummary, RepoSnapshot, ResultStatus, SessionKind,
        SessionManifest, SessionStatus, ThinkingMode, TrustLevel, UiMode,
    };
    use chrono::Utc;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn sample_manifest(root: &Path, id: &str) -> SessionManifest {
        let session_dir = root.join(".codex-forge").join("sessions").join(id);
        SessionManifest {
            id: id.to_string(),
            task: "实现 memory".to_string(),
            repo_snapshot: RepoSnapshot {
                repo_root: root.to_path_buf(),
                display_name: "demo".to_string(),
                top_level_entries: vec!["src".to_string()],
                detected_stacks: vec!["Rust".to_string()],
                readme_excerpt: None,
            },
            created_at: Utc::now(),
            status: SessionStatus::Completed,
            session_kind: SessionKind::Run,
            ui_mode: UiMode::Minimal,
            workers_requested: 2,
            role_set: "default".to_string(),
            model: None,
            thinking_mode: ThinkingMode::Balanced,
            cleanup_success: false,
            apply_mode: ApplyMode::AutoSafe,
            max_retries: 1,
            fail_fast: false,
            verification_commands: Vec::new(),
            config_path: None,
            preset: None,
            iteration_index: 1,
            shared_context_version: 1,
            root_session_id: id.to_string(),
            parent_session_id: None,
            continuation_kind: None,
            feedback_history: Vec::new(),
            supersedes_session_id: None,
            baseline_artifacts: Default::default(),
            plan_todo: None,
            todo_states: Vec::new(),
            execution_graph: None,
            execution_contract: None,
            worker_results: Vec::new(),
            artifact_manifest: ArtifactManifest::default(),
            apply_result: None,
            verification_report: None,
            change_trust_report: None,
            doctor_report: None,
            final_summary: Some(FinalSummary {
                overview: "完成 memory 内核".to_string(),
                result_status: ResultStatus::Completed,
                review_gate: None,
                apply_status: crate::model::ApplyStatus::Applied,
                trust_level: TrustLevel::High,
                accepted_files: vec!["src/memory.rs".to_string()],
                manual_review_files: Vec::new(),
                rejected_files: Vec::new(),
                verified_capabilities: vec!["cargo test".to_string()],
                blocked_verifications: Vec::new(),
                open_risks: vec!["需要补 smoke".to_string()],
                recommended_next_action: vec!["运行真实 PTY 冒烟".to_string()],
                todo_states: Vec::new(),
                used_fallback: false,
                review_report: None,
                evidence_summary: Vec::new(),
                iteration_index: 1,
                based_on_session_id: None,
                feedback_summary: Vec::new(),
                delta_summary: Vec::new(),
                completed_this_iteration: Vec::new(),
                unaccepted_feedback: Vec::new(),
            }),
            manual_delivery_result: None,
            manual_review_state: None,
            review_fix: None,
            memory_manifest: None,
            source_plan_session_id: None,
            reused_plan_session_id: None,
            resumed_from_session_id: None,
            artifact_index: Vec::new(),
            timeline_events: Vec::new(),
            demo_summary: Vec::new(),
            lineage: Vec::new(),
            session_dir: session_dir.clone(),
            timeline_path: session_dir.join("timeline.jsonl"),
            graph_path: session_dir.join("commander").join("execution-graph.json"),
            execution_contract_path: session_dir
                .join("commander")
                .join("execution-contract.json"),
            summary_json_path: session_dir.join("summary.json"),
            summary_markdown_path: session_dir.join("summary.md"),
            artifact_manifest_path: session_dir.join("artifact-manifest.json"),
            apply_plan_path: session_dir.join("integration").join("apply-plan.json"),
            apply_result_path: session_dir.join("integration").join("apply-result.json"),
            verification_report_path: session_dir
                .join("integration")
                .join("verification-report.json"),
            change_trust_report_path: session_dir
                .join("integration")
                .join("change-trust-report.json"),
        }
    }

    #[test]
    fn sync_memory_manifest_initializes_files() {
        let tempdir = TempDir::new().expect("tempdir");
        let mut manifest = sample_manifest(tempdir.path(), "s1");
        sync_memory_manifest(&mut manifest).expect("sync");
        let memory_manifest = manifest.memory_manifest.expect("memory manifest");
        assert!(memory_manifest.manifest_path.exists());
        assert!(memory_manifest.shared_index_path.exists());
        assert!(memory_manifest.session_entries_path.exists());
        assert!(memory_manifest.task_brief_path.exists());
    }

    #[test]
    fn commander_memory_context_reads_history() {
        let tempdir = TempDir::new().expect("tempdir");
        let manifest = sample_manifest(tempdir.path(), "s1");
        let session_dir = tempdir
            .path()
            .join(".codex-forge")
            .join("sessions")
            .join("s1");
        fs::create_dir_all(&session_dir).expect("mkdir");
        fs::write(
            session_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).expect("json"),
        )
        .expect("write manifest");

        let context =
            build_commander_memory_context(tempdir.path(), Some("current"), None).expect("context");
        assert!(context.prompt_block.contains("共享记忆摘要"));
        assert!(context.prompt_block.contains("完成 memory 内核"));
        assert_eq!(context.sessions, 1);
    }

    #[test]
    fn memory_prompt_block_mentions_entries() {
        let view = crate::model::MemoryView {
            id: "v1".to_string(),
            role: "implementer".to_string(),
            agent_id: "implementer-1".to_string(),
            summary: "有 1 条共享记忆".to_string(),
            generated_at: Utc::now(),
            entries: vec![crate::model::MemoryEntry {
                id: "e1".to_string(),
                scope: crate::model::MemoryScope::Shared,
                kind: crate::model::MemoryEntryKind::Summary,
                title: "上轮结论".to_string(),
                summary: "需要补 smoke".to_string(),
                details: Vec::new(),
                tags: Vec::new(),
                role_hints: Vec::new(),
                source_session_id: "s1".to_string(),
                source_agent_id: None,
                created_at: Utc::now(),
            }],
        };
        let prompt = render_memory_prompt_block(&view);
        assert!(prompt.contains("共享记忆视图"));
        assert!(prompt.contains("上轮结论"));
    }
}
