use super::*;

impl AppShell {
    pub(super) fn refresh_project(&mut self, override_defaults: bool) -> Result<()> {
        let target_dir = PathBuf::from(self.form.target_dir.clone());
        let config_path = self.form.config_path.clone();
        self.project = load_project_context(
            &target_dir,
            optional_path(&config_path),
            &mut self.form,
            override_defaults,
        )?;
        remember_target_dir(&self.project.target_dir)?;
        if self.history_index >= self.project.sessions.len() {
            self.history_index = 0;
        }
        if let Some(item) = self.project.sessions.get(self.history_index) {
            self.selected_session = load_session(&self.project.target_dir, Some(&item.id)).ok();
            self.refresh_history_detail_if_needed();
        } else {
            self.selected_session = None;
            self.history_detail = None;
        }
        Ok(())
    }

    pub(super) fn open_history_selection(&mut self) -> Result<()> {
        if let Some(item) = self.project.sessions.get(self.history_index) {
            self.selected_session = Some(load_session(&self.project.target_dir, Some(&item.id))?);
            self.refresh_history_detail_if_needed();
            self.navigate_to(Route::History);
        }
        Ok(())
    }

    pub(super) fn open_history_detail(&mut self) {
        if self.route != Route::History {
            self.navigate_to(Route::History);
        }
        if let Some(session) = &self.selected_session {
            self.history_detail = Some(HistoryDetailState::from_session(session));
            self.push_notice("已打开历史详情，可切换查看计划、运行和产物原文。");
        } else {
            self.push_notice("请先在历史页选中一个会话。");
        }
    }

    pub(super) fn open_clean_selected_confirm(&mut self) {
        if self.route != Route::History {
            return;
        }
        let Some(session) = &self.selected_session else {
            self.push_notice("请先在历史页选中一个会话。");
            return;
        };
        self.confirm_dialog = Some(ConfirmDialogState {
            action: ConfirmAction::CleanSelected {
                session_id: session.id.clone(),
            },
            title: "确认清理当前历史".to_string(),
            lines: vec![
                format!("将删除 session `{}`。", session.id),
                "如果有基于它继续生成的后续迭代，也会一起删除，避免 lineage 悬空。".to_string(),
                "对应 workers / integration / summary 等 `.codex-forge` 产物也会一并清掉。"
                    .to_string(),
                "按 Enter 确认，按 Esc 取消。".to_string(),
            ],
        });
    }

    pub(super) fn open_reset_selected_confirm(&mut self) {
        if self.route != Route::History {
            return;
        }
        let Some(session) = &self.selected_session else {
            self.push_notice("请先在历史页选中一个会话。");
            return;
        };
        self.confirm_dialog = Some(ConfirmDialogState {
            action: ConfirmAction::ResetSelected {
                session_id: session.id.clone(),
            },
            title: "确认一键重置".to_string(),
            lines: vec![
                format!(
                    "将回退 session `{}` 及其后续迭代落地的自动提交。",
                    session.id
                ),
                "只有当这些提交仍位于当前 HEAD 尾部时，系统才会执行回滚，避免误删后续人工提交。"
                    .to_string(),
                "回滚成功后，对应 `.codex-forge` 历史、worker 产物和 summary 也会一并删除。"
                    .to_string(),
                "按 Enter 确认，按 Esc 取消。".to_string(),
            ],
        });
    }

    pub(super) fn open_clean_all_confirm(&mut self) {
        if self.route != Route::History {
            return;
        }
        self.confirm_dialog = Some(ConfirmDialogState {
            action: ConfirmAction::CleanAll,
            title: "确认一键清空".to_string(),
            lines: vec![
                format!(
                    "将删除当前仓库下整个 `{}` 目录。",
                    self.project.target_dir.join(".codex-forge").display()
                ),
                "所有历史 session、回放文件、worker 产物和 summary 都会被清空。".to_string(),
                "按 Enter 确认，按 Esc 取消。".to_string(),
            ],
        });
    }

    pub(super) fn open_quit_confirm(&mut self) {
        self.confirm_dialog = Some(ConfirmDialogState {
            action: ConfirmAction::Quit,
            title: "确认退出 TUI".to_string(),
            lines: vec![
                "将退出当前 codex-forge TUI 界面。".to_string(),
                "不会中断已经写入磁盘的历史会话；如果后台动作仍在执行，退出后你可以下次重新进入查看。".to_string(),
                "按 Enter 确认退出，按 Esc 取消。".to_string(),
            ],
        });
    }

    pub(super) fn refresh_history_detail_if_needed(&mut self) {
        let current_session_id = self
            .history_detail
            .as_ref()
            .map(|detail| detail.session_id().to_string());
        let Some(current_session_id) = current_session_id else {
            return;
        };
        let Some(session) = &self.selected_session else {
            self.history_detail = None;
            return;
        };
        if current_session_id != session.id {
            self.history_detail = Some(HistoryDetailState::from_session(session));
        }
    }

    pub(super) fn approve_current_review_file(&mut self) -> Result<()> {
        let Some(review) = &mut self.manual_review else {
            return Ok(());
        };
        let Some(index) = review
            .state
            .files
            .get(review.file_index)
            .map(|_| review.file_index)
        else {
            return Ok(());
        };
        review.state.files[index].status = ManualReviewFileStatus::Approved;
        let file_path = review.state.files[index].path.clone();
        review.state.selected_file = Some(file_path.clone());
        let session_id = review.session_id.clone();
        let state = review.state.clone();
        persist_manual_review_state(&self.project.target_dir, &session_id, state)?;
        self.refresh_project(false)?;
        self.push_notice(&format!("已通过文件：{}", file_path));
        Ok(())
    }

    pub(super) fn mark_current_review_file_needs_fix(&mut self) -> Result<()> {
        let Some(review) = &mut self.manual_review else {
            return Ok(());
        };
        let Some(index) = review
            .state
            .files
            .get(review.file_index)
            .map(|_| review.file_index)
        else {
            return Ok(());
        };
        review.state.files[index].status = ManualReviewFileStatus::NeedsFix;
        if review.state.files[index]
            .issue_summary
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            let file_path = review.state.files[index].path.clone();
            review.state.files[index].issue_summary = Some(format!(
                "请修复文件 `{}` 的人工审查问题，并保持改动范围只限该文件。",
                file_path
            ));
        }
        let file_path = review.state.files[index].path.clone();
        review.state.selected_file = Some(file_path.clone());
        let session_id = review.session_id.clone();
        let state = review.state.clone();
        persist_manual_review_state(&self.project.target_dir, &session_id, state)?;
        self.refresh_project(false)?;
        self.push_notice(&format!("已标记需修复：{}", file_path));
        Ok(())
    }

    pub(super) async fn start_manual_review_fix(&mut self) -> Result<()> {
        if self
            .active_command
            .as_ref()
            .is_some_and(|command| command.state.is_running())
        {
            self.push_notice("已有动作在执行，请等待当前命令结束。");
            self.navigate_to(Route::Run);
            return Ok(());
        }
        let Some(review) = &self.manual_review else {
            return Ok(());
        };
        let Some(parent_session) = self.selected_session.clone() else {
            self.push_notice("请先在历史页选中一个会话。");
            return Ok(());
        };
        let Some(file) = review.selected_file() else {
            self.push_notice("当前没有可返修文件。");
            return Ok(());
        };
        let issue = file.issue_summary.clone().unwrap_or_else(|| {
            format!(
                "请只基于文件 `{}` 修复人工审查问题，并保留其余文件不动。",
                file.path
            )
        });

        let (tx, rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.pending_review_fix = Some(PendingReviewFix {
            parent_session_id: review.session_id.clone(),
            target_file: file.path.clone(),
        });
        self.runtime_state = Some(RuntimeViewState::new(
            &parent_session.id,
            &format!("人工审查返修 {}", file.path),
        ));
        self.run_subview = RunSubview::Timeline;
        spawn_review_fix_action(
            self.project.target_dir.clone(),
            parent_session,
            file.path.clone(),
            issue.clone(),
            tx,
            Some(cancel_rx),
        );
        self.active_command = Some(ActiveCommand {
            action: ShellAction::ReviewFixSelected,
            state: CommandState::Running,
            started_at: Instant::now(),
            finished_at: None,
            stop_requested: false,
            output: vec![
                format!("准备动作：修复当前文件 {}", file.path),
                format!("审查问题：{}", issue),
                "返修完成后会自动回到人工审查页，并展示修复前后 diff。".to_string(),
            ],
            cancel_tx: Some(cancel_tx),
            rx,
        });
        self.push_notice(&format!("已开始单文件返修：{}", file.path));
        self.navigate_to(Route::Run);
        Ok(())
    }

    pub(super) async fn deliver_manual_review_approved(&mut self) -> Result<()> {
        let Some(review) = &self.manual_review else {
            return Ok(());
        };
        let Some(session) = self.selected_session.clone() else {
            self.push_notice("请先在历史页选中一个会话。");
            return Ok(());
        };
        let review_state = review.state.clone();
        let approved_files = review_state
            .files
            .iter()
            .filter(|item| item.status == ManualReviewFileStatus::Approved)
            .map(|item| item.path.clone())
            .collect::<Vec<_>>();
        if approved_files.is_empty() {
            self.push_notice("当前还没有人工通过的文件。");
            return Ok(());
        }

        match deliver_manual_review_approved_files(
            &self.project.target_dir,
            &session.id,
            &review_state,
            session.repo_root(),
        )
        .await
        {
            Ok(delivered_files) => {
                persist_manual_delivery_result(
                    &self.project.target_dir,
                    &session.id,
                    ManualDeliveryResult {
                        delivered_at: Utc::now(),
                        target_dir: session.repo_root().to_path_buf(),
                        delivered_files: delivered_files.clone(),
                        skipped_files: review_state
                            .files
                            .iter()
                            .filter(|item| item.status != ManualReviewFileStatus::Approved)
                            .map(|item| item.path.clone())
                            .collect(),
                        success: true,
                        source_apply_status: session.apply_result.as_ref().map(|item| item.status),
                        review_gate: session
                            .apply_result
                            .as_ref()
                            .and_then(|item| item.review_gate),
                        error: None,
                    },
                )?;
                self.refresh_project(false)?;
                self.push_notice(&format!(
                    "已将 {} 个人工通过文件交付到目标目录。",
                    delivered_files.len()
                ));
            }
            Err(error) => {
                self.push_notice(&format!("人工审查交付失败：{error}"));
            }
        }
        Ok(())
    }

    pub(super) async fn deliver_selected_session_manually(&mut self) -> Result<()> {
        let Some(session) = self.selected_session.clone() else {
            self.push_notice("请先在历史页选中一个会话。");
            return Ok(());
        };

        match deliver_session_accepted_files(
            &self.project.target_dir,
            &session.id,
            session.repo_root(),
        )
        .await
        {
            Ok(delivered_files) => {
                persist_manual_delivery_result(
                    &self.project.target_dir,
                    &session.id,
                    ManualDeliveryResult {
                        delivered_at: Utc::now(),
                        target_dir: session.repo_root().to_path_buf(),
                        delivered_files: delivered_files.clone(),
                        skipped_files: session
                            .apply_result
                            .as_ref()
                            .map(|result| {
                                result
                                    .accepted_files
                                    .iter()
                                    .filter(|path| !delivered_files.contains(*path))
                                    .cloned()
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default(),
                        success: true,
                        source_apply_status: session.apply_result.as_ref().map(|item| item.status),
                        review_gate: session
                            .apply_result
                            .as_ref()
                            .and_then(|item| item.review_gate),
                        error: None,
                    },
                )?;
                self.refresh_project(false)?;
                self.push_notice(&format!(
                    "已将 {} 个 accepted_files 手动写入目标目录。",
                    delivered_files.len()
                ));
            }
            Err(error) => {
                self.push_notice(&format!("手动写入失败：{error}"));
            }
        }
        Ok(())
    }

    pub(super) fn push_notice(&mut self, message: &str) {
        if self.notices.last().is_some_and(|last| last == message) {
            return;
        }
        self.notices.push(message.to_string());
        if self.notices.len() > MAX_NOTICE_LINES {
            let overflow = self.notices.len() - MAX_NOTICE_LINES;
            self.notices.drain(0..overflow);
        }
    }

    pub(super) fn execute_confirm_action(&mut self) -> Result<()> {
        let Some(confirm) = self.confirm_dialog.take() else {
            return Ok(());
        };

        let message = match confirm.action {
            ConfirmAction::ResetSelected { session_id } => {
                let report = reset_session_lineage(&self.project.target_dir, &session_id)?;
                if let Some(reset_to) = report.reset_to {
                    format!(
                        "已回滚 {} 个 commit，重置到 {}，并清理 {} 个 session。",
                        report.reset_commits.len(),
                        truncate(&reset_to, 12),
                        report.removed_sessions.len()
                    )
                } else {
                    format!(
                        "目标 session 没有落地 commit，已直接清理 {} 个 session。",
                        report.removed_sessions.len()
                    )
                }
            }
            ConfirmAction::CleanSelected { session_id } => {
                let report = cleanup_session_lineage(&self.project.target_dir, &session_id)?;
                format!(
                    "已清理 {} 个 session：{}",
                    report.removed_sessions.len(),
                    report.removed_sessions.join("、")
                )
            }
            ConfirmAction::CleanAll => {
                let report = cleanup_all_forge_artifacts(&self.project.target_dir)?;
                if report.had_artifacts {
                    format!(
                        "已清空 `.codex-forge`，共删除 {} 个 session。",
                        report.removed_sessions.len()
                    )
                } else {
                    "当前仓库没有可清理的 `.codex-forge` 产物。".to_string()
                }
            }
            ConfirmAction::Quit => {
                self.should_quit = true;
                "正在退出 TUI。".to_string()
            }
        };

        self.history_detail = None;
        if !self.should_quit {
            self.refresh_project(false)?;
            self.navigate_to(Route::History);
        }
        self.push_notice(&message);
        Ok(())
    }

    pub(super) fn field_value(&self, field: FormField) -> String {
        match field {
            FormField::TargetDir => self.form.target_dir.clone(),
            FormField::ConfigPath => empty_to_dash(&self.form.config_path),
            FormField::Task => {
                if self.form.task.trim().is_empty() {
                    "—".to_string()
                } else {
                    truncate(&self.form.task.replace('\n', " ⏎ "), 72)
                }
            }
            FormField::ContinueFeedback => {
                if self.form.continue_feedback.trim().is_empty() {
                    "—".to_string()
                } else {
                    truncate(&self.form.continue_feedback.replace('\n', " ⏎ "), 72)
                }
            }
            FormField::ReviewIssue => {
                if self.form.review_issue.trim().is_empty() {
                    "—".to_string()
                } else {
                    truncate(&self.form.review_issue.replace('\n', " ⏎ "), 72)
                }
            }
            FormField::ContinueMode => {
                continue_mode_user_label(self.form.continue_mode).to_string()
            }
            FormField::ThinkingMode => format!(
                "{} / {}",
                thinking_mode_user_title(self.form.thinking_mode),
                thinking_mode_user_hint(self.form.thinking_mode)
            ),
            FormField::RoleSet => self.form.role_set.clone(),
            FormField::Workers => self.form.workers.clone(),
            FormField::MaxRetries => self.form.max_retries.clone(),
            FormField::Model => empty_to_dash(&self.form.model),
            FormField::ApplyMode => apply_mode_user_label(self.form.apply_mode).to_string(),
            FormField::Preset => self
                .form
                .preset
                .map(|item| item.label().to_string())
                .unwrap_or_else(|| "none".to_string()),
            FormField::FailFast => bool_label(self.form.fail_fast),
            FormField::CleanupSuccess => bool_label(self.form.cleanup_success),
            FormField::ResumeSession => empty_to_dash(&self.form.resume_session_id),
        }
    }

    pub(super) fn start_editing(&mut self, field: FormField) {
        let current = match field {
            FormField::TargetDir => self.form.target_dir.clone(),
            FormField::ConfigPath => self.form.config_path.clone(),
            FormField::Task => self.form.task.clone(),
            FormField::ContinueFeedback => self.form.continue_feedback.clone(),
            FormField::ReviewIssue => self
                .manual_review
                .as_ref()
                .and_then(|review| review.selected_file())
                .and_then(|item| item.issue_summary.clone())
                .unwrap_or_else(|| self.form.review_issue.clone()),
            FormField::Workers => self.form.workers.clone(),
            FormField::MaxRetries => self.form.max_retries.clone(),
            FormField::Model => self.form.model.clone(),
            FormField::ResumeSession => self.form.resume_session_id.clone(),
            _ => return,
        };
        let history_entries = if field == FormField::Task {
            recent_task_history(&self.project)
        } else {
            Vec::new()
        };
        let history_index = if field == FormField::Task {
            history_entries.iter().position(|item| item == &current)
        } else {
            None
        };
        self.edit_state = Some(EditState {
            field,
            cursor: current.chars().count(),
            buffer: current,
            preferred_column: None,
            history_entries,
            history_index,
        });
    }

    pub(super) fn commit_edit(&mut self) -> Result<()> {
        let Some(edit) = self.edit_state.take() else {
            return Ok(());
        };
        match edit.field {
            FormField::TargetDir => self.form.target_dir = edit.buffer.trim().to_string(),
            FormField::ConfigPath => self.form.config_path = edit.buffer.trim().to_string(),
            FormField::Task => self.form.task = edit.buffer.trim().to_string(),
            FormField::ContinueFeedback => {
                self.form.continue_feedback = edit.buffer.trim().to_string()
            }
            FormField::ReviewIssue => {
                self.form.review_issue = edit.buffer.trim().to_string();
                if let Some(review) = &mut self.manual_review
                    && let Some(file) = review.selected_file_mut()
                {
                    file.issue_summary = if self.form.review_issue.trim().is_empty() {
                        None
                    } else {
                        Some(self.form.review_issue.clone())
                    };
                    review.state.selected_file = Some(file.path.clone());
                    persist_manual_review_state(
                        &self.project.target_dir,
                        &review.session_id,
                        review.state.clone(),
                    )?;
                }
            }
            FormField::Workers => self.form.workers = edit.buffer.trim().to_string(),
            FormField::MaxRetries => self.form.max_retries = edit.buffer.trim().to_string(),
            FormField::Model => self.form.model = edit.buffer.trim().to_string(),
            FormField::ResumeSession => {
                self.form.resume_session_id = edit.buffer.trim().to_string()
            }
            _ => {}
        }
        if matches!(edit.field, FormField::TargetDir | FormField::ConfigPath) {
            self.refresh_project(true)?;
        }
        Ok(())
    }

    pub(super) fn focus_action_after_save(&mut self, action: ShellAction) -> String {
        match action {
            ShellAction::Plan => {
                self.navigate_to(Route::Start);
                self.start_focus = StartFocus::Actions;
                self.start_action_index = StartAction::all()
                    .iter()
                    .position(|item| *item == StartAction::Plan)
                    .unwrap_or(0);
                "内容已保存。已准备好“先看方案”，按 Enter 手动开始。".to_string()
            }
            ShellAction::Run => {
                self.navigate_to(Route::Start);
                self.start_focus = StartFocus::Actions;
                self.start_action_index = StartAction::all()
                    .iter()
                    .position(|item| *item == StartAction::Run)
                    .unwrap_or(0);
                "内容已保存。已准备好“开始运行”，按 Enter 手动开始。".to_string()
            }
            ShellAction::ContinueSelected => {
                self.navigate_to(Route::History);
                self.history_focus = HistoryFocus::Actions;
                self.history_action_index =
                    available_history_actions(self.selected_session.as_ref())
                        .iter()
                        .position(|item| *item == HistoryAction::Continue)
                        .unwrap_or(0);
                "反馈已保存。已准备好“继续优化”，按 Enter 手动开始。".to_string()
            }
            ShellAction::ReviewFixSelected => {
                "内容已保存。现在可以在人工审查里发起单文件返修。".to_string()
            }
            ShellAction::Doctor
            | ShellAction::ReplaySelected
            | ShellAction::ExecutePlanSelected
            | ShellAction::ConfigValidate
            | ShellAction::AgentsList => "内容已保存。".to_string(),
        }
    }

    pub(super) fn command_preview_lines(&self, action: ShellAction) -> CommandPreview {
        build_command_preview(
            &self.project.target_dir,
            &self.form,
            action,
            self.selected_session.as_ref(),
        )
    }
}
