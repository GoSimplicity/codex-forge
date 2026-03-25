use super::*;

impl AppShell {
    pub(super) fn current_start_action(&self) -> StartAction {
        StartAction::all()[self.start_action_index.min(StartAction::all().len() - 1)]
    }

    pub(super) fn current_history_action(&self) -> HistoryAction {
        let actions = available_history_actions(self.selected_session.as_ref());
        actions[self.history_action_index.min(actions.len() - 1)]
    }

    pub(super) fn current_run_action(&self) -> RunAction {
        RunAction::all()[self.run_action_index.min(RunAction::all().len() - 1)]
    }

    pub(super) fn current_nav_route(&self) -> Route {
        Route::all()[self.nav_index.min(Route::all().len() - 1)]
    }

    pub(super) fn restore_page_focus(&mut self) {
        match self.route {
            Route::Start => {
                if !self.advanced_settings_open && self.start_focus == StartFocus::AdvancedFields {
                    self.start_focus = StartFocus::TaskInput;
                }
            }
            Route::Run => {
                self.run_focus = RunFocus::Subviews;
            }
            Route::History => {
                self.history_focus = HistoryFocus::Sessions;
            }
        }
    }

    pub(super) async fn activate_start_focus(&mut self) -> Result<()> {
        match self.start_focus {
            StartFocus::TaskInput => self.start_editing(FormField::Task),
            StartFocus::Actions => match self.current_start_action() {
                StartAction::Doctor => self.start_action(ShellAction::Doctor).await?,
                StartAction::Plan => self.start_action(ShellAction::Plan).await?,
                StartAction::Run => self.start_action(ShellAction::Run).await?,
                StartAction::ConfigValidate => {
                    self.start_action(ShellAction::ConfigValidate).await?
                }
                StartAction::AgentsList => self.start_action(ShellAction::AgentsList).await?,
                StartAction::ToggleSettings => self.toggle_advanced_settings(),
            },
            StartFocus::AdvancedFields => {
                let field = advanced_fields()[self.selected_field];
                if field_is_editable(field) {
                    self.start_editing(field);
                } else {
                    self.cycle_current_field(true);
                }
            }
        }
        Ok(())
    }

    pub(super) async fn activate_history_focus(&mut self) -> Result<()> {
        match self.history_focus {
            HistoryFocus::Sessions => self.open_history_detail(),
            HistoryFocus::Actions => match self.current_history_action() {
                HistoryAction::ExecutePlan => {
                    self.start_action(ShellAction::ExecutePlanSelected).await?
                }
                HistoryAction::Continue => self.start_action(ShellAction::ContinueSelected).await?,
                HistoryAction::EditFeedback => self.start_editing(FormField::ContinueFeedback),
                HistoryAction::ContinueMode => {
                    self.form.continue_mode = cycle_continue_mode(self.form.continue_mode, true);
                    self.push_notice(&format!(
                        "继续模式已切换为：{}。",
                        continue_mode_user_label(self.form.continue_mode)
                    ));
                }
                HistoryAction::ManualWrite => self.deliver_selected_session_manually().await?,
                HistoryAction::Replay => self.start_action(ShellAction::ReplaySelected).await?,
                HistoryAction::Detail => self.open_history_detail(),
                HistoryAction::ResetSelected => self.open_reset_selected_confirm(),
                HistoryAction::CleanSelected => self.open_clean_selected_confirm(),
                HistoryAction::CleanAll => self.open_clean_all_confirm(),
                HistoryAction::BackToStart => {
                    self.navigate_to(Route::Start);
                    self.push_notice("已返回开始页；如需更多配置，请在右侧动作区打开“高级设置”。");
                }
            },
        }
        Ok(())
    }

    pub(super) fn activate_run_focus(&mut self) {
        match self.run_focus {
            RunFocus::Subviews => {}
            RunFocus::Actions => match self.current_run_action() {
                RunAction::Back => {
                    let target = run_back_route(self.run_return_route);
                    self.route = target;
                    self.push_notice(if self.is_command_running() {
                        match target {
                            Route::Start => "已离开执行页；后台动作仍在运行，可随时再回来查看。",
                            Route::Run => "仍停留在执行页。",
                            Route::History => "已切到历史页；后台动作仍在运行。",
                        }
                    } else {
                        match target {
                            Route::Start => "已返回开始页。",
                            Route::Run => "已返回执行页。",
                            Route::History => "已返回历史页。",
                        }
                    });
                }
                RunAction::Stop => self.stop_active_command(),
                RunAction::ViewHistory => self.navigate_to(Route::History),
                RunAction::BackToStart => self.navigate_to(Route::Start),
            },
        }
    }

    pub(super) async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }

        if matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return Ok(());
        }

        if !matches!(key.code, KeyCode::Esc) {
            self.exit_esc_armed_at = None;
        }

        if self.edit_state.is_some() {
            return self.handle_edit_key(key).await;
        }

        if self.manual_review.is_some() {
            return self.handle_manual_review_key(key).await;
        }

        if self.history_detail.is_some() {
            return self.handle_history_detail_key(key);
        }

        if self.confirm_dialog.is_some() {
            return self.handle_confirm_key(key);
        }

        match key.code {
            KeyCode::Char('q') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('1') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate_via_tab(Route::Start)
            }
            KeyCode::Char('2') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate_to(Route::Run)
            }
            KeyCode::Char('3') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate_via_tab(Route::History)
            }
            KeyCode::Esc => {
                if self.is_exit_armable_root() {
                    self.handle_exit_escape();
                } else {
                    self.handle_global_back();
                }
            }
            KeyCode::Char('a') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.route == Route::Start {
                    self.toggle_advanced_settings();
                } else {
                    self.navigate_to(Route::Start);
                    self.start_focus = StartFocus::Actions;
                    self.start_action_index = StartAction::all()
                        .iter()
                        .position(|item| *item == StartAction::ToggleSettings)
                        .unwrap_or(0);
                    self.push_notice("已回到开始页；按 Enter 可打开“高级设置”。");
                }
            }
            KeyCode::Char('g') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.refresh_project(true)?
            }
            KeyCode::Char('m') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                match self.route {
                    Route::Start => {
                        self.form.thinking_mode = cycle_thinking_mode(self.form.thinking_mode, true)
                    }
                    Route::History => {
                        self.form.continue_mode = cycle_continue_mode(self.form.continue_mode, true)
                    }
                    Route::Run => self.cycle_run_subview(true),
                }
            }
            KeyCode::Char('d') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_action(ShellAction::Doctor).await?
            }
            KeyCode::Char('p') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_action(ShellAction::Plan).await?
            }
            KeyCode::Char('r') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_action(ShellAction::Run).await?
            }
            KeyCode::Char('c') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_action(ShellAction::ContinueSelected).await?
            }
            KeyCode::Char('l') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_action(ShellAction::ReplaySelected).await?
            }
            KeyCode::Char('k') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_action(ShellAction::ConfigValidate).await?
            }
            KeyCode::Char('i') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_action(ShellAction::AgentsList).await?
            }
            KeyCode::Char('z') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_reset_selected_confirm()
            }
            KeyCode::Char('x') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_clean_selected_confirm()
            }
            KeyCode::Char('X') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_clean_all_confirm()
            }
            KeyCode::Char('v') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_history_detail()
            }
            KeyCode::Char('s') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.stop_active_command()
            }
            KeyCode::Char('[') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.route == Route::Run && !self.nav_focus {
                    self.cycle_run_subview(false);
                }
            }
            KeyCode::Char(']') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.route == Route::Run && !self.nav_focus {
                    self.cycle_run_subview(true);
                }
            }
            KeyCode::Tab => self.handle_tab_navigation(true),
            KeyCode::BackTab => self.handle_tab_navigation(false),
            KeyCode::Up => self.handle_up_down(-1),
            KeyCode::Down => self.handle_up_down(1),
            KeyCode::Left => self.handle_left_right(false),
            KeyCode::Right => self.handle_left_right(true),
            KeyCode::Enter => self.handle_enter().await?,
            KeyCode::Char('e') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                match self.route {
                    Route::Start => self.activate_start_focus().await?,
                    Route::History => {
                        if self.selected_session.is_some() {
                            self.start_editing(FormField::ContinueFeedback);
                        } else {
                            self.open_history_selection()?;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn handle_history_detail_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(detail) = &mut self.history_detail else {
            return Ok(());
        };
        let mut should_close = false;

        match key.code {
            KeyCode::Esc | KeyCode::Char('v') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                should_close = true;
            }
            KeyCode::Tab | KeyCode::Char(']') | KeyCode::Right
                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                detail.cycle_tab(true);
            }
            KeyCode::BackTab | KeyCode::Char('[') | KeyCode::Left
                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                detail.cycle_tab(false);
            }
            KeyCode::Up => detail.scroll_lines(-1),
            KeyCode::Down => detail.scroll_lines(1),
            KeyCode::PageUp => detail.previous_page(),
            KeyCode::PageDown => detail.next_page(),
            KeyCode::Home => detail.first_page(),
            KeyCode::End => detail.last_page(),
            KeyCode::Char('k') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                detail.scroll_lines(-1)
            }
            KeyCode::Char('j') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                detail.scroll_lines(1)
            }
            _ => {}
        }

        if should_close {
            self.history_detail = None;
            self.push_notice("已关闭历史详情。");
        }

        Ok(())
    }

    pub(super) fn handle_confirm_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.confirm_dialog = None;
                self.push_notice("已取消当前确认操作。");
            }
            KeyCode::Enter => {
                if let Err(error) = self.execute_confirm_action() {
                    self.confirm_dialog = None;
                    self.push_notice(&format!("操作失败：{error:#}"));
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) async fn handle_edit_key(&mut self, key: KeyEvent) -> Result<()> {
        let mut commit = false;
        let mut saved_field = None;
        let mut focus_after_commit = None;
        let mut cancel = false;

        {
            let Some(edit) = &mut self.edit_state else {
                return Ok(());
            };
            let multiline = matches!(
                edit.field,
                FormField::Task | FormField::ContinueFeedback | FormField::ReviewIssue
            );
            match key.code {
                KeyCode::Esc => {
                    cancel = true;
                }
                KeyCode::Enter if multiline && !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    insert_char_at_cursor(edit, '\n');
                }
                KeyCode::Enter => {
                    commit = true;
                    saved_field = Some(edit.field);
                }
                KeyCode::Backspace => backspace_at_cursor(edit),
                KeyCode::Delete => delete_at_cursor(edit),
                KeyCode::Left => move_cursor_horizontal(edit, false),
                KeyCode::Right => move_cursor_horizontal(edit, true),
                KeyCode::Up => move_cursor_vertical(edit, false),
                KeyCode::Down => move_cursor_vertical(edit, true),
                KeyCode::Home => move_cursor_line_edge(edit, false),
                KeyCode::End => move_cursor_line_edge(edit, true),
                KeyCode::Tab if multiline => {
                    insert_char_at_cursor(edit, ' ');
                    insert_char_at_cursor(edit, ' ');
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    commit = true;
                    saved_field = Some(edit.field);
                }
                KeyCode::Char('r')
                    if multiline && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    commit = true;
                    saved_field = Some(edit.field);
                    focus_after_commit = Some(match edit.field {
                        FormField::ContinueFeedback => ShellAction::ContinueSelected,
                        _ => ShellAction::Run,
                    });
                }
                KeyCode::Char('p')
                    if edit.field == FormField::Task
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    commit = true;
                    saved_field = Some(edit.field);
                    focus_after_commit = Some(ShellAction::Plan);
                }
                KeyCode::Char('j')
                    if edit.field == FormField::Task
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    cycle_edit_history(edit, true);
                }
                KeyCode::Char('k')
                    if edit.field == FormField::Task
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    cycle_edit_history(edit, false);
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    insert_char_at_cursor(edit, ch);
                }
                _ => {}
            }
        }

        if cancel {
            self.edit_state = None;
            self.push_notice("已取消当前编辑，原值未变。");
            return Ok(());
        }

        if commit {
            self.commit_edit()?;
            if let Some(action) = focus_after_commit {
                let message = self.focus_action_after_save(action);
                self.push_notice(&message);
            } else if let Some(field) = saved_field {
                self.push_notice(&saved_field_notice(field));
            }
        }

        Ok(())
    }

    pub(super) fn handle_tab_navigation(&mut self, forward: bool) {
        if self.nav_focus {
            self.nav_focus = false;
            self.restore_page_focus();
            return;
        }

        let _ = forward;
        self.nav_focus = true;
        self.nav_index = Route::all()
            .iter()
            .position(|item| *item == self.route)
            .unwrap_or(0);
    }

    pub(super) fn is_exit_armable_root(&self) -> bool {
        self.route == Route::Start
            && !self.advanced_settings_open
            && !self.nav_focus
            && self.edit_state.is_none()
            && self.history_detail.is_none()
            && self.manual_review.is_none()
            && self.confirm_dialog.is_none()
    }

    pub(super) fn handle_exit_escape(&mut self) {
        let now = Instant::now();
        if self
            .exit_esc_armed_at
            .is_some_and(|armed_at| now.duration_since(armed_at) <= EXIT_ESC_ARM_WINDOW)
        {
            self.exit_esc_armed_at = None;
            self.open_quit_confirm();
            return;
        }

        self.exit_esc_armed_at = Some(now);
        self.push_notice("再次按 `Esc` 将弹出退出确认；也可直接按 `Ctrl+C` 退出。");
    }

    pub(super) fn handle_up_down(&mut self, delta: isize) {
        if self.nav_focus {
            if delta > 0 {
                self.nav_focus = false;
                self.restore_page_focus();
            }
            return;
        }

        match self.route {
            Route::Start => match self.start_focus {
                StartFocus::TaskInput => {
                    if delta < 0 {
                        self.nav_focus = true;
                    }
                }
                StartFocus::Actions => {
                    let len = StartAction::all().len() as isize;
                    self.start_action_index =
                        ((self.start_action_index as isize + delta).rem_euclid(len)) as usize;
                }
                StartFocus::AdvancedFields => {
                    let len = advanced_fields().len() as isize;
                    self.selected_field =
                        ((self.selected_field as isize + delta).rem_euclid(len)) as usize;
                }
            },
            Route::Run => match self.run_focus {
                RunFocus::Subviews => {
                    if delta < 0 {
                        self.nav_focus = true;
                    } else {
                        self.run_focus = RunFocus::Actions;
                    }
                }
                RunFocus::Actions => {
                    let len = RunAction::all().len() as isize;
                    self.run_action_index =
                        ((self.run_action_index as isize + delta).rem_euclid(len)) as usize;
                }
            },
            Route::History => match self.history_focus {
                HistoryFocus::Sessions => {
                    if delta < 0 && self.history_index == 0 {
                        self.nav_focus = true;
                    } else {
                        let len = self.project.sessions.len().max(1) as isize;
                        self.history_index =
                            ((self.history_index as isize + delta).rem_euclid(len)) as usize;
                        if let Some(item) = self.project.sessions.get(self.history_index) {
                            self.selected_session =
                                load_session(&self.project.target_dir, Some(&item.id)).ok();
                            self.refresh_history_detail_if_needed();
                        }
                    }
                }
                HistoryFocus::Actions => {
                    let len =
                        available_history_actions(self.selected_session.as_ref()).len() as isize;
                    self.history_action_index =
                        ((self.history_action_index as isize + delta).rem_euclid(len)) as usize;
                }
            },
        }
    }

    pub(super) fn handle_left_right(&mut self, forward: bool) {
        if self.nav_focus {
            let current = self.nav_index.min(Route::all().len().saturating_sub(1));
            self.nav_index = cycle_index(current, Route::all().len(), forward);
            return;
        }

        match self.route {
            Route::Start => self.cycle_start_focus(forward),
            Route::Run => match self.run_focus {
                RunFocus::Subviews => self.cycle_run_subview(forward),
                RunFocus::Actions => {}
            },
            Route::History => self.cycle_history_focus(forward),
        }
    }

    pub(super) async fn handle_enter(&mut self) -> Result<()> {
        if self.nav_focus {
            self.navigate_via_tab(self.current_nav_route());
            return Ok(());
        }

        match self.route {
            Route::Start => self.activate_start_focus().await?,
            Route::Run => self.activate_run_focus(),
            Route::History => self.activate_history_focus().await?,
        }
        Ok(())
    }

    pub(super) fn cycle_start_focus(&mut self, forward: bool) {
        let order = if self.advanced_settings_open {
            vec![
                StartFocus::TaskInput,
                StartFocus::Actions,
                StartFocus::AdvancedFields,
            ]
        } else {
            vec![StartFocus::TaskInput, StartFocus::Actions]
        };
        let current = order
            .iter()
            .position(|item| *item == self.start_focus)
            .unwrap_or(0);
        self.start_focus = order[cycle_index(current, order.len(), forward)];
    }

    pub(super) fn cycle_history_focus(&mut self, forward: bool) {
        let order = [HistoryFocus::Sessions, HistoryFocus::Actions];
        let current = order
            .iter()
            .position(|item| *item == self.history_focus)
            .unwrap_or(0);
        self.history_focus = order[cycle_index(current, order.len(), forward)];
    }

    pub(super) fn cycle_current_field(&mut self, forward: bool) {
        if self.route != Route::Start || !self.advanced_settings_open {
            return;
        }

        match advanced_fields()[self.selected_field] {
            FormField::ContinueMode => {
                self.form.continue_mode = cycle_continue_mode(self.form.continue_mode, forward);
            }
            FormField::ThinkingMode => {
                self.form.thinking_mode = cycle_thinking_mode(self.form.thinking_mode, forward);
            }
            FormField::RoleSet => {
                if !self.project.role_sets.is_empty() {
                    let current = self
                        .project
                        .role_sets
                        .iter()
                        .position(|item| item == &self.form.role_set)
                        .unwrap_or(0);
                    let next = cycle_index(current, self.project.role_sets.len(), forward);
                    self.form.role_set = self.project.role_sets[next].clone();
                }
            }
            FormField::ApplyMode => {
                let modes = [
                    ApplyMode::InPlace,
                    ApplyMode::AutoSafe,
                    ApplyMode::Bundle,
                    ApplyMode::None,
                ];
                let current = modes
                    .iter()
                    .position(|item| *item == self.form.apply_mode)
                    .unwrap_or(0);
                self.form.apply_mode = modes[cycle_index(current, modes.len(), forward)];
            }
            FormField::Preset => {
                let presets = [None, Some(SessionPreset::FeatureDemo)];
                let current = presets
                    .iter()
                    .position(|item| *item == self.form.preset)
                    .unwrap_or(0);
                self.form.preset = presets[cycle_index(current, presets.len(), forward)];
            }
            FormField::FailFast => {
                self.form.fail_fast = !self.form.fail_fast;
            }
            FormField::CleanupSuccess => {
                self.form.cleanup_success = !self.form.cleanup_success;
            }
            _ => {}
        }
    }

    pub(super) fn toggle_advanced_settings(&mut self) {
        if self.is_command_running() {
            self.push_notice("后台动作仍在执行；你仍可回到开始页查看，但高级设置不会自动展开。");
        }

        if self.route != Route::Start {
            self.navigate_to(Route::Start);
            self.start_focus = StartFocus::Actions;
            self.start_action_index = StartAction::all()
                .iter()
                .position(|item| *item == StartAction::ToggleSettings)
                .unwrap_or(0);
            return;
        }

        let opening = !self.advanced_settings_open;
        self.advanced_settings_open = opening;
        self.selected_field = 0;
        self.start_focus = if opening {
            StartFocus::AdvancedFields
        } else {
            StartFocus::Actions
        };
        self.push_notice(if opening {
            "已展开高级设置：现在可以调整模板、并发和结果落地策略。"
        } else {
            "已收起高级设置：返回默认低负担模式。"
        });
    }

    pub(super) async fn start_action(&mut self, action: ShellAction) -> Result<()> {
        if self
            .active_command
            .as_ref()
            .is_some_and(|command| command.state.is_running())
        {
            self.push_notice("已有动作在执行，请等待当前命令结束。");
            self.navigate_to(Route::Run);
            return Ok(());
        }

        if !self.ensure_task_ready(action) {
            return Ok(());
        }
        if !self.ensure_continue_ready(action) {
            return Ok(());
        }
        if !self.ensure_plan_execution_ready(action) {
            return Ok(());
        }

        let preview = self.command_preview_lines(action);
        let supports_stop =
            action_supports_stop(action, &self.form, self.selected_session.as_ref());
        let (tx, rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = if supports_stop {
            let (cancel_tx, cancel_rx) = watch::channel(false);
            (Some(cancel_tx), Some(cancel_rx))
        } else {
            (None, None)
        };
        if let Some(runtime_state) =
            prepare_runtime_state(action, &self.form, self.selected_session.as_ref())
        {
            self.runtime_state = Some(runtime_state);
        }
        self.run_subview = preferred_run_subview(action);
        spawn_embedded_action(
            action,
            self.project.target_dir.clone(),
            self.form.clone(),
            self.selected_session.clone(),
            tx,
            cancel_rx,
        );
        self.active_command = Some(ActiveCommand {
            action,
            state: CommandState::Running,
            started_at: Instant::now(),
            finished_at: None,
            stop_requested: false,
            output: initial_command_output(
                action,
                &preview,
                &self.project.display_target,
                supports_stop,
            ),
            cancel_tx,
            rx,
        });
        self.push_notice(&format!(
            "已开始：{}。{}",
            action.label(),
            truncate(&preview.summary, 56)
        ));
        self.navigate_to(Route::Run);
        Ok(())
    }

    pub(super) fn ensure_task_ready(&mut self, action: ShellAction) -> bool {
        if !action.requires_task() || !self.form.task.trim().is_empty() {
            return true;
        }

        self.navigate_to(Route::Start);
        self.advanced_settings_open = false;
        self.start_editing(FormField::Task);
        self.push_notice("请先输入提示词，再开始运行。");
        false
    }

    pub(super) fn ensure_continue_ready(&mut self, action: ShellAction) -> bool {
        if action != ShellAction::ContinueSelected {
            return true;
        }

        if self.selected_session.is_none() {
            self.navigate_to(Route::History);
            self.push_notice("请先在历史页选中一个已完成 session，再继续优化。");
            return false;
        }
        if !self
            .selected_session
            .as_ref()
            .is_some_and(SessionManifest::continuable)
        {
            self.navigate_to(Route::History);
            self.push_notice("当前 session 还不能继续迭代；请先选一个已完成会话。");
            return false;
        }
        if !self
            .selected_session
            .as_ref()
            .is_some_and(SessionManifest::is_run_session)
        {
            self.navigate_to(Route::History);
            self.push_notice("继续优化只支持执行会话；方案会话请改用“执行此方案”。");
            return false;
        }
        true
    }

    pub(super) fn ensure_plan_execution_ready(&mut self, action: ShellAction) -> bool {
        if action != ShellAction::ExecutePlanSelected {
            return true;
        }

        let Some(session) = self.selected_session.as_ref() else {
            self.navigate_to(Route::History);
            self.push_notice("请先在历史页选中一个已完成方案，再执行此方案。");
            return false;
        };
        if !session.is_plan_session() {
            self.navigate_to(Route::History);
            self.push_notice("当前会话不是方案会话；请先选中一个 plan session。");
            return false;
        }
        if !session.continuable() {
            self.navigate_to(Route::History);
            self.push_notice("当前方案尚未完成；请先等待它结束。");
            return false;
        }
        true
    }

    pub(super) fn poll_command_output(&mut self) -> Result<()> {
        let mut finished = None;
        if let Some(command) = &mut self.active_command {
            // 这里持续把后台线程发回来的运行事件，折叠到 UI 可消费的本地状态里。
            while let Ok(event) = command.rx.try_recv() {
                match event {
                    RunnerEvent::Line(line) => {
                        push_command_output(&mut command.output, line);
                    }
                    RunnerEvent::Runtime(event) => {
                        if let Some(runtime_state) = &mut self.runtime_state {
                            runtime_state.apply(&event);
                        }
                        push_command_output(&mut command.output, describe_runtime_event(&event));
                    }
                    RunnerEvent::Doctor(report) => {
                        self.last_doctor_report = Some(report.clone());
                        self.project.last_error = if report.ok {
                            None
                        } else {
                            Some(build_doctor_failure_summary(&report))
                        };
                        for check in &report.checks {
                            push_command_output(
                                &mut command.output,
                                format!(
                                    "[{}] {} - {}",
                                    check.status.label(),
                                    check.name,
                                    check.detail
                                ),
                            );
                        }
                        push_command_output(
                            &mut command.output,
                            format!(
                                "doctor 结论：{} / {}",
                                report.readiness.label(),
                                report.summary
                            ),
                        );
                    }
                    RunnerEvent::Finished { state, manifest } => {
                        command.state = state;
                        command.finished_at = Some(Instant::now());
                        push_command_output(
                            &mut command.output,
                            format!("动作结束：{} / {}", command.action.label(), state.label()),
                        );
                        finished = Some((command.action, state, *manifest));
                    }
                }
            }
        }

        if let Some((action, state, manifest)) = finished {
            if action == ShellAction::ReviewFixSelected {
                let pending = self.pending_review_fix.take();
                if let (Some(pending), Some(child_manifest)) = (pending, manifest.as_ref()) {
                    record_review_fix_completion(
                        &self.project.target_dir,
                        &pending.parent_session_id,
                        &pending.target_file,
                        child_manifest,
                    )?;
                    self.refresh_project(false)?;
                    self.open_manual_review_popup_for(
                        &pending.parent_session_id,
                        Some(&pending.target_file),
                    )?;
                    self.push_notice(&format!(
                        "单文件返修已结束：{} / {}",
                        pending.target_file,
                        state.label()
                    ));
                    self.navigate_to(Route::History);
                    return Ok(());
                }
            }
            if let Some(manifest) = manifest {
                if let Some(runtime_state) = &mut self.runtime_state {
                    runtime_state.set_identity(manifest.id.clone(), manifest.task.clone());
                }
                self.selected_session = Some(manifest);
                self.refresh_history_detail_if_needed();
            }
            self.push_notice(&format!("{} 已结束：{}", action.label(), state.label()));
            self.refresh_project(false)?;
            if !self.project.sessions.is_empty() {
                self.history_index = 0;
                self.open_history_selection()?;
                if matches!(state, CommandState::Succeeded)
                    && matches!(
                        action,
                        ShellAction::Plan
                            | ShellAction::Run
                            | ShellAction::ExecutePlanSelected
                            | ShellAction::ContinueSelected
                    )
                {
                    self.navigate_to(Route::History);
                } else {
                    self.navigate_to(Route::Run);
                }
            }
        }
        Ok(())
    }

    pub(super) fn stop_active_command(&mut self) {
        let Some(command) = &mut self.active_command else {
            self.push_notice("当前没有运行中的动作。");
            return;
        };
        if !command.state.is_running() {
            self.push_notice("当前动作已经结束，无需再次停止。");
            return;
        }
        if command.stop_requested {
            self.push_notice("停止请求已发出，正在等待安全收口。");
            self.navigate_to(Route::Run);
            return;
        }
        let Some(cancel_tx) = command.cancel_tx.clone() else {
            self.push_notice("当前动作暂不支持安全停止；支持停止时，执行页会明确提示。");
            self.navigate_to(Route::Run);
            return;
        };
        match cancel_tx.send(true) {
            Ok(_) => {
                command.stop_requested = true;
                push_command_output(
                    &mut command.output,
                    "停止请求已发送，等待在跑 worker / replay 安全退出…".to_string(),
                );
                self.push_notice("停止请求已发送，正在等待安全收口。");
                self.navigate_to(Route::Run);
            }
            Err(_) => self.push_notice("停止信号发送失败，当前动作可能已经结束。"),
        }
    }

    pub(super) fn navigate_to(&mut self, route: Route) {
        self.history_return_route =
            next_history_return_route(self.route, self.history_return_route, route);
        self.run_return_route = next_run_return_route(self.route, self.run_return_route, route);
        self.route = route;
        self.nav_focus = false;
        self.nav_index = Route::all()
            .iter()
            .position(|item| *item == route)
            .unwrap_or(0);
        self.restore_page_focus();
    }

    pub(super) fn navigate_via_tab(&mut self, route: Route) {
        if self.is_command_running() && route != Route::Run {
            self.push_notice(
                "当前动作仍在后台执行；你可以继续切页查看信息，或回执行页发送停止信号。",
            );
        }
        self.navigate_to(route);
    }

    pub(super) fn leave_history(&mut self) {
        let target = history_back_route(self.history_return_route);
        self.route = target;
        self.push_notice(match target {
            Route::Start => "已返回开始页。",
            Route::Run => "已返回执行页。",
            Route::History => "已返回历史页。",
        });
    }

    pub(super) fn handle_global_back(&mut self) {
        self.exit_esc_armed_at = None;
        if self.nav_focus {
            self.nav_focus = false;
            self.restore_page_focus();
            self.push_notice("已离开顶部导航。");
            return;
        }
        if self.manual_review.is_some() {
            self.manual_review = None;
            self.push_notice("已关闭人工审查。");
            return;
        }
        match self.route {
            Route::Start => {
                if self.advanced_settings_open {
                    self.advanced_settings_open = false;
                    self.selected_field = 0;
                    self.push_notice("已收起高级设置。");
                }
            }
            Route::Run => {
                if self
                    .active_command
                    .as_ref()
                    .is_some_and(|command| command.state.is_running())
                {
                    self.push_notice("当前动作仍在执行；如需中止请按 `s`。");
                } else {
                    let target = run_back_route(self.run_return_route);
                    self.route = target;
                    self.push_notice(match target {
                        Route::Start => "已返回开始页。",
                        Route::Run => "已返回执行页。",
                        Route::History => "已返回历史页。",
                    });
                }
            }
            Route::History => self.leave_history(),
        }
    }

    pub(super) fn is_command_running(&self) -> bool {
        self.active_command
            .as_ref()
            .is_some_and(|command| command.state.is_running())
    }

    pub(super) fn cycle_run_subview(&mut self, forward: bool) {
        if self.route != Route::Run {
            return;
        }
        let current = RunSubview::all()
            .iter()
            .position(|item| *item == self.run_subview)
            .unwrap_or(0);
        let next = cycle_index(current, RunSubview::all().len(), forward);
        self.run_subview = RunSubview::all()[next];
    }
}
