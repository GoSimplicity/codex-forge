use super::*;

pub(super) fn build_history_detail_summary(
    session: &SessionManifest,
    tab: HistoryDetailTab,
) -> String {
    match tab {
        HistoryDetailTab::Overview => {
            let summary = session
                .final_summary
                .as_ref()
                .map(|item| item.overview.clone())
                .or_else(|| session.plan_todo.as_ref().map(|item| item.summary.clone()))
                .unwrap_or_else(|| "这次还没有可直接展示的摘要。".to_string());
            format!(
                "任务：{}\n\n状态：{}\n类型：{}\n会话：{}\n创建时间：{}\n目标仓库：{}\n目标目录状态：{} / {}\n\n摘要：{}\n\n目标目录：{}\n系统记录目录：{}\n",
                session.task,
                session.status.label(),
                session.session_kind.label(),
                session.id,
                format_beijing(session.created_at, "%Y-%m-%d %H:%M:%S"),
                session.repo_root().display(),
                delivery_status_label(session),
                delivery_status_detail(session),
                summary,
                session.repo_root().display(),
                session.session_dir.display(),
            )
        }
        HistoryDetailTab::Plan => {
            if let Some(plan) = &session.plan_todo {
                let todo_titles = plan
                    .todos
                    .iter()
                    .map(|item| format!("- {} {}", item.id, item.title))
                    .collect::<Vec<_>>()
                    .join("\n");
                let risks = if plan.risks.is_empty() {
                    "- 无".to_string()
                } else {
                    plan.risks
                        .iter()
                        .map(|item| format!("- {item}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                format!(
                    "方案摘要：{}\n\n推进策略：{}\n\n待办数量：{}\n{}\n\n主要风险：\n{}\n\n计划过程文件：{}\n",
                    plan.summary,
                    plan.approach,
                    plan.todos.len(),
                    todo_titles,
                    risks,
                    session.deliverable_plan_path().display(),
                )
            } else {
                "当前会话没有生成方案。".to_string()
            }
        }
        HistoryDetailTab::Runtime => {
            let timeline = if session.timeline_events.is_empty() {
                "还没有过程记录。".to_string()
            } else {
                session
                    .timeline_events
                    .iter()
                    .rev()
                    .take(8)
                    .rev()
                    .map(|item| {
                        format!(
                            "- {} / {} / {}",
                            format_beijing(item.ts, "%H:%M:%S"),
                            item.title,
                            item.detail
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            format!(
                "过程概览\n\n最近关键节点：\n{}\n\n当前状态：{}\nWorker 数量：{}\nTodo 数量：{}\n",
                timeline,
                session.status.label(),
                session.worker_results.len(),
                session.todo_states.len(),
            )
        }
        HistoryDetailTab::Artifacts => {
            let system_artifacts = existing_system_artifacts(session)
                .into_iter()
                .map(|path| format!("- 已生成 / {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n");
            let repo_exports = session
                .final_summary
                .as_ref()
                .map(|_| repo_export_candidates(session))
                .unwrap_or_default()
                .into_iter()
                .map(|path| {
                    let status = if path.exists() {
                        "已导出"
                    } else {
                        "未导出"
                    };
                    format!("- {} / {}", status, repo_export_label(session, &path))
                })
                .collect::<Vec<_>>()
                .join("\n");
            let final_summary = session.final_summary.as_ref();
            format!(
                "结果概览\n\n目标目录状态：{} / {}\n\n系统工件：\n{}\n\n用户结果件：\n{}\n\n已接收文件：{}\n人工复核：{}\n已验证能力：{}\n开放风险：{}\n",
                delivery_status_label(session),
                delivery_status_detail(session),
                if system_artifacts.is_empty() {
                    "- 尚未生成系统工件".to_string()
                } else {
                    system_artifacts
                },
                if repo_exports.is_empty() {
                    if session.is_plan_session() {
                        "- 方案会话不会直接写入目标目录".to_string()
                    } else {
                        "- 当前会话尚未写入目标目录".to_string()
                    }
                } else {
                    repo_exports
                },
                final_summary
                    .map(|item| item.accepted_files.len())
                    .unwrap_or(0),
                final_summary
                    .map(|item| item.manual_review_files.len())
                    .unwrap_or(0),
                final_summary
                    .map(|item| item.verified_capabilities.len())
                    .unwrap_or(0),
                final_summary.map(|item| item.open_risks.len()).unwrap_or(0),
            )
        }
        HistoryDetailTab::Technical => format!(
            "这里保留原始技术资料，方便排障和复盘。\n\nsession 目录：{}\ntimeline：{}\nartifact manifest：{}\nworker 数量：{}\n",
            session.session_dir.display(),
            session.timeline_path.display(),
            session.artifact_manifest_path.display(),
            session.worker_results.len(),
        ),
    }
}

pub(super) fn build_history_detail_body(
    session: &SessionManifest,
    tab: HistoryDetailTab,
) -> String {
    let mut sections = Vec::<String>::new();
    match tab {
        HistoryDetailTab::Overview => {
            sections.push(format!(
                "会话：{}\n任务：{}\n状态：{}\n版本：V{}\n根会话：{}\n来源会话：{}\n创建时间：{}\n目标仓库：{}\n协作模板：{}\nCodex 思考强度：{}\n结果落地：{}\n目标目录状态：{} / {}\n可继续反馈：{}\nSession 目录：{}",
                session.id,
                session.task,
                session.status.label(),
                session.iteration_index_value(),
                session.root_session_id_ref(),
                session.parent_session_id.as_deref().unwrap_or("无"),
                format_beijing(session.created_at, "%Y-%m-%d %H:%M:%S"),
                session.repo_root().display(),
                session.role_set,
                thinking_mode_user_title(session.thinking_mode),
                apply_mode_user_label(session.apply_mode),
                delivery_status_label(session),
                delivery_status_detail(session),
                if session.continuable() { "是" } else { "否" },
                session.session_dir.display()
            ));
            append_serialized_section(&mut sections, "Repo Snapshot", &session.repo_snapshot);
            append_serialized_section(&mut sections, "Lineage", &session.lineage);
            append_serialized_section(&mut sections, "Feedback History", &session.feedback_history);
            append_serialized_section(&mut sections, "Final Summary", &session.final_summary);
            append_serialized_section(&mut sections, "Doctor Report", &session.doctor_report);
            append_serialized_section(&mut sections, "Artifact Index", &session.artifact_index);
        }
        HistoryDetailTab::Plan => {
            append_file_section_if_exists(
                &mut sections,
                "Plan Todo Markdown",
                session.session_dir.join("commander").join("plan-todo.md"),
            );
            append_file_section_if_exists(
                &mut sections,
                "Process Plan Markdown",
                session.deliverable_plan_path(),
            );
            append_optional_file_section(
                &mut sections,
                "Plan Todo JSON",
                session.artifact_manifest.plan_todo_path.clone(),
            );
            append_serialized_section(&mut sections, "Plan Todo (Manifest)", &session.plan_todo);
            append_file_section_if_exists(
                &mut sections,
                "Execution Graph JSON",
                &session.graph_path,
            );
            append_serialized_section(
                &mut sections,
                "Execution Graph (Manifest)",
                &session.execution_graph,
            );
            append_file_section_if_exists(
                &mut sections,
                "Execution Contract JSON",
                &session.execution_contract_path,
            );
            append_serialized_section(
                &mut sections,
                "Execution Contract (Manifest)",
                &session.execution_contract,
            );
            append_optional_file_section(
                &mut sections,
                "Todo State JSON",
                session.artifact_manifest.todo_state_path.clone(),
            );
            append_serialized_section(
                &mut sections,
                "Todo States (Manifest)",
                &session.todo_states,
            );
        }
        HistoryDetailTab::Runtime => {
            append_file_section_if_exists(&mut sections, "Timeline JSONL", &session.timeline_path);
            append_serialized_section(
                &mut sections,
                "Timeline Summary (Manifest)",
                &session.timeline_events,
            );
            append_serialized_section(&mut sections, "Demo Summary", &session.demo_summary);
            append_serialized_section(&mut sections, "Doctor Report", &session.doctor_report);
        }
        HistoryDetailTab::Artifacts => {
            append_file_section_if_exists(
                &mut sections,
                "Summary Markdown",
                &session.summary_markdown_path,
            );
            append_file_section_if_exists(
                &mut sections,
                "Summary JSON",
                &session.summary_json_path,
            );
            append_optional_file_section(
                &mut sections,
                "Apply Plan JSON",
                session.artifact_manifest.apply_plan_path.clone(),
            );
            append_file_section_if_exists(
                &mut sections,
                "Apply Result JSON",
                &session.apply_result_path,
            );
            append_serialized_section(
                &mut sections,
                "Apply Result (Manifest)",
                &session.apply_result,
            );
            append_file_section_if_exists(
                &mut sections,
                "Verification Report JSON",
                &session.verification_report_path,
            );
            append_serialized_section(
                &mut sections,
                "Verification Report (Manifest)",
                &session.verification_report,
            );
            append_file_section_if_exists(
                &mut sections,
                "Change Trust Report JSON",
                &session.change_trust_report_path,
            );
            append_serialized_section(
                &mut sections,
                "Change Trust Report (Manifest)",
                &session.change_trust_report,
            );
            append_optional_file_section(
                &mut sections,
                "Manual Delivery Result JSON",
                session
                    .artifact_manifest
                    .manual_delivery_result_path
                    .clone(),
            );
            append_serialized_section(
                &mut sections,
                "Manual Delivery Result (Manifest)",
                &session.manual_delivery_result,
            );
            append_optional_file_section(
                &mut sections,
                "Manual Review State JSON",
                session.artifact_manifest.manual_review_state_path.clone(),
            );
            append_serialized_section(
                &mut sections,
                "Manual Review State (Manifest)",
                &session.manual_review_state,
            );
            append_file_section_if_exists(
                &mut sections,
                "Process Summary Markdown",
                session.deliverable_summary_path(),
            );
            append_file_section_if_exists(
                &mut sections,
                "Process Changes Markdown",
                session.deliverable_changes_path(),
            );
            append_file_section_if_exists(
                &mut sections,
                "Process Verify Markdown",
                session.deliverable_verify_path(),
            );
            append_repo_export_sections(&mut sections, session);
        }
        HistoryDetailTab::Technical => {
            if session.worker_results.is_empty() {
                sections.push("当前 session 没有 worker 运行输出。".to_string());
            }
            for worker in &session.worker_results {
                sections.push(format!(
                    "Worker：{}\n角色：{}\n标题：{}\n状态：{}\n尝试次数：{}\n退出码：{}\n改动文件：{}\n错误：{}",
                    worker.agent_id,
                    worker.role,
                    worker.task_title,
                    worker.status.label(),
                    worker.attempts,
                    worker
                        .exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "无".to_string()),
                    if worker.changed_files.is_empty() {
                        "无".to_string()
                    } else {
                        worker.changed_files.join("；")
                    },
                    worker.error.clone().unwrap_or_else(|| "无".to_string())
                ));
                append_serialized_section(
                    &mut sections,
                    &format!("Worker {} Result JSON", worker.agent_id),
                    worker,
                );
                append_optional_file_section(
                    &mut sections,
                    &format!("Worker {} Diff", worker.agent_id),
                    worker.diff_path.clone(),
                );
                append_optional_file_section(
                    &mut sections,
                    &format!("Worker {} Git Status", worker.agent_id),
                    worker.git_status_path.clone(),
                );
                append_optional_file_section(
                    &mut sections,
                    &format!("Worker {} Handoff", worker.agent_id),
                    worker.handoff_path.clone(),
                );
                append_file_section_if_exists(
                    &mut sections,
                    &format!("Worker {} Final Output", worker.agent_id),
                    &worker.final_output_path,
                );
                append_file_section_if_exists(
                    &mut sections,
                    &format!("Worker {} Stdout", worker.agent_id),
                    &worker.stdout_path,
                );
                append_file_section_if_exists(
                    &mut sections,
                    &format!("Worker {} Stderr", worker.agent_id),
                    &worker.stderr_path,
                );
            }
            append_serialized_section(
                &mut sections,
                "Artifact Manifest",
                &session.artifact_manifest,
            );
            append_file_section_if_exists(
                &mut sections,
                "Artifact Manifest JSON",
                &session.artifact_manifest_path,
            );
            append_optional_file_section(
                &mut sections,
                "Feedback Markdown",
                session.artifact_manifest.feedback_markdown_path.clone(),
            );
            append_optional_file_section(
                &mut sections,
                "Feedback JSON",
                session.artifact_manifest.feedback_json_path.clone(),
            );
            append_optional_file_section(
                &mut sections,
                "Iteration Summary Markdown",
                session.artifact_manifest.iteration_summary_path.clone(),
            );
            append_optional_file_section(
                &mut sections,
                "Lineage JSON",
                session.artifact_manifest.lineage_path.clone(),
            );
            append_optional_file_section(
                &mut sections,
                "Latest Pointer Markdown",
                session.artifact_manifest.latest_pointer_path.clone(),
            );
        }
    }

    sections.join("\n\n")
}

pub(super) fn append_serialized_section<T>(sections: &mut Vec<String>, title: &str, value: &T)
where
    T: Serialize,
{
    let body = serde_json::to_string_pretty(value)
        .unwrap_or_else(|error| format!("序列化失败：{error:#}"));
    sections.push(format!("===== {title} =====\n\n{body}"));
}

pub(super) fn append_optional_file_section(
    sections: &mut Vec<String>,
    title: &str,
    path: Option<PathBuf>,
) {
    match path {
        Some(path) => append_file_section_if_exists(sections, title, path),
        None => sections.push(format!("===== {title} =====\n\n未生成该文件。")),
    }
}

pub(super) fn append_repo_export_section<P>(sections: &mut Vec<String>, title: &str, path: P)
where
    P: AsRef<Path>,
{
    append_file_section_with_missing_message(sections, title, path, "当前会话未导出该用户文件。");
}

pub(super) fn append_file_section_if_exists<P>(sections: &mut Vec<String>, title: &str, path: P)
where
    P: AsRef<Path>,
{
    append_file_section_with_missing_message(sections, title, path, "文件不存在。");
}

pub(super) fn append_file_section_with_missing_message<P>(
    sections: &mut Vec<String>,
    title: &str,
    path: P,
    missing_message: &str,
) where
    P: AsRef<Path>,
{
    let path = path.as_ref();
    let body = if path.exists() {
        read_text_lossy(path)
    } else {
        missing_message.to_string()
    };
    sections.push(format!(
        "===== {title} =====\n路径：{}\n\n{}",
        path.display(),
        body
    ));
}

pub(super) fn read_text_lossy(path: &Path) -> String {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        Err(error) => format!("读取失败：{error:#}"),
    }
}

pub(super) fn page_count_for_text(text: &str, lines_per_page: usize) -> usize {
    let total_lines = text.lines().count().max(1);
    total_lines.div_ceil(lines_per_page.max(1))
}

pub(super) fn page_text(text: &str, page: usize, lines_per_page: usize) -> String {
    let lines_per_page = lines_per_page.max(1);
    let start = page.saturating_mul(lines_per_page);
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return String::new();
    }
    let start = start.min(lines.len().saturating_sub(1));
    let end = (start + lines_per_page).min(lines.len());
    lines[start..end].join("\n")
}
