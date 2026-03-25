use super::*;

impl AppShell {
    pub(super) fn render(&self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints(shell_layout_constraints(area))
            .split(area);

        frame.render_widget(self.header_widget(area.width), layout[0]);
        frame.render_widget(self.tabs_widget(area.width), layout[1]);
        self.render_body(frame, layout[2]);
        frame.render_widget(self.footer_widget(area.width), layout[3]);

        if self.edit_state.is_some() {
            let popup = centered_rect(
                popup_percent(area.width, 70, 96),
                popup_percent(area.height, 22, 90),
                area,
            );
            frame.render_widget(Clear, popup);
            self.render_edit_popup(frame, popup);
        }

        if self.history_detail.is_some() {
            let popup = centered_rect(
                popup_percent(area.width, 90, 98),
                popup_percent(area.height, 88, 96),
                area,
            );
            frame.render_widget(Clear, popup);
            self.render_history_detail_popup(frame, popup);
        }

        if self.manual_review.is_some() {
            let popup = centered_rect(
                popup_percent(area.width, 94, 99),
                popup_percent(area.height, 90, 98),
                area,
            );
            frame.render_widget(Clear, popup);
            self.render_manual_review_popup(frame, popup);
        }

        if self.confirm_dialog.is_some() {
            let popup = centered_rect(
                popup_percent(area.width, 72, 94),
                popup_percent(area.height, 28, 54),
                area,
            );
            frame.render_widget(Clear, popup);
            self.render_confirm_popup(frame, popup);
        }
    }

    pub(super) fn header_widget(&self, width: u16) -> Paragraph<'static> {
        let status = self
            .active_command
            .as_ref()
            .map(|command| {
                format!(
                    "动作：{} / {} / {}s",
                    command.action.label(),
                    command.state.label(),
                    command.started_at.elapsed().as_secs()
                )
            })
            .unwrap_or_else(|| "动作：空闲".to_string());
        let doctor_status = self
            .last_doctor_report
            .as_ref()
            .map(|report| format!("环境：{} / {}", report.readiness.label(), report.summary))
            .unwrap_or_else(|| "环境：未检查".to_string());
        let advanced_status = if self.advanced_settings_open {
            "高级设置：已展开"
        } else {
            "高级设置：已收起"
        };

        let lines = if width < 72 {
            vec![
                Line::from(vec![
                    Span::styled(
                        "◢ CF V6 ◣",
                        Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        thinking_mode_user_title(self.form.thinking_mode),
                        Style::default()
                            .fg(thinking_mode_color(self.form.thinking_mode))
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(format!(
                    "{} | {} | {}",
                    truncate(&self.project.display_target, 20),
                    truncate(advanced_status, 12),
                    truncate(&status, 16)
                )),
            ]
        } else {
            vec![
                Line::from(vec![
                    Span::styled(
                        "◢ CODEX-FORGE V6 ◣",
                        Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled("低负担协作入口", Style::default().fg(Color::Yellow)),
                    Span::raw("  "),
                    Span::styled(
                        format!(
                            "{} / {}",
                            thinking_mode_user_title(self.form.thinking_mode),
                            thinking_mode_user_hint(self.form.thinking_mode)
                        ),
                        Style::default()
                            .fg(thinking_mode_color(self.form.thinking_mode))
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(format!(
                    "目标仓库：{}  |  {}",
                    truncate(&self.project.display_target, 56),
                    doctor_status,
                )),
                Line::from(format!(
                    "会话数：{}   {}   {}",
                    self.project.sessions.len(),
                    advanced_status,
                    status
                )),
            ]
        };

        Paragraph::new(lines)
            .block(Block::default().title("总览").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    pub(super) fn tabs_widget(&self, width: u16) -> Tabs<'static> {
        let titles = route_titles(width)
            .into_iter()
            .map(Line::from)
            .collect::<Vec<_>>();
        Tabs::new(titles)
            .select(if self.nav_focus {
                self.nav_index.min(Route::all().len() - 1)
            } else {
                Route::all()
                    .iter()
                    .position(|item| *item == self.route)
                    .unwrap_or(0)
            })
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(if self.nav_focus {
                        "导航（已聚焦，←→ 切换，Enter 进入）"
                    } else {
                        "导航（按 ↑ 或 Tab 进入）"
                    })
                    .border_style(if self.nav_focus {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    }),
            )
    }

    pub(super) fn footer_widget(&self, width: u16) -> Paragraph<'_> {
        let mut lines = contextual_help_lines(
            self.route,
            self.edit_state.as_ref(),
            self.run_subview,
            width,
        );
        lines.push(Line::from(""));
        let notice_limit = if width < 72 {
            1
        } else if width < 100 {
            2
        } else {
            4
        };
        lines.extend(
            self.notices
                .iter()
                .rev()
                .take(notice_limit)
                .map(|item| Line::from(item.clone())),
        );
        Paragraph::new(lines)
            .block(Block::default().title("现在可做").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    pub(super) fn render_body(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        if self.route == Route::Run {
            self.render_run_route(frame, area);
            return;
        }

        let sections = split_main_sections(area);

        match self.route {
            Route::Start => self.render_start_route(frame, sections),
            Route::Run => unreachable!("run route 由 render_run_route 单独渲染"),
            Route::History => {
                frame.render_widget(self.history_left_widget(), sections[0]);
                let right_sections = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(11), Constraint::Min(10)])
                    .split(sections[1]);
                frame.render_widget(self.history_actions_widget(), right_sections[0]);
                frame.render_widget(self.history_right_widget(), right_sections[1]);
            }
        }
    }

    pub(super) fn render_start_route(&self, frame: &mut ratatui::Frame<'_>, sections: Vec<Rect>) {
        frame.render_widget(self.start_main_widget(), sections[0]);
        if self.advanced_settings_open {
            let right_sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(9),
                    Constraint::Percentage(48),
                    Constraint::Percentage(52),
                ])
                .split(sections[1]);
            frame.render_widget(self.start_actions_widget(), right_sections[0]);
            frame.render_widget(self.advanced_settings_widget(), right_sections[1]);
            frame.render_widget(self.advanced_details_widget(), right_sections[2]);
        } else {
            let right_sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(9), Constraint::Min(10)])
                .split(sections[1]);
            frame.render_widget(self.start_actions_widget(), right_sections[0]);
            frame.render_widget(self.start_recent_widget(), right_sections[1]);
        }
    }

    pub(super) fn render_run_route(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints(run_route_constraints(area))
            .split(area);

        frame.render_widget(self.run_subview_tabs(area.width), sections[0]);

        match self.run_subview {
            RunSubview::Dashboard => {
                if let Some(runtime_state) = &self.runtime_state {
                    render_runtime_dashboard(frame, sections[1], runtime_state, "进度总览");
                } else if self
                    .active_command
                    .as_ref()
                    .is_some_and(|command| command.action == ShellAction::Doctor)
                {
                    frame.render_widget(self.run_doctor_widget(), sections[1]);
                } else if self.active_command.as_ref().is_some_and(|command| {
                    matches!(
                        command.action,
                        ShellAction::ConfigValidate | ShellAction::AgentsList
                    )
                }) {
                    frame.render_widget(self.run_readonly_widget(), sections[1]);
                } else {
                    frame.render_widget(self.run_placeholder_widget(), sections[1]);
                }
            }
            RunSubview::Timeline => {
                let split = Layout::default()
                    .direction(if area.width < 120 {
                        Direction::Vertical
                    } else {
                        Direction::Horizontal
                    })
                    .constraints(if area.width < 120 {
                        vec![Constraint::Percentage(42), Constraint::Percentage(58)]
                    } else {
                        vec![Constraint::Percentage(40), Constraint::Percentage(60)]
                    })
                    .split(sections[1]);
                frame.render_widget(self.run_timeline_widget(), split[0]);
                frame.render_widget(self.run_timeline_detail_widget(), split[1]);
            }
            RunSubview::Summary => {
                frame.render_widget(self.run_summary_widget(), sections[1]);
            }
        }

        let bottom_sections = Layout::default()
            .direction(if area.width < 110 {
                Direction::Vertical
            } else {
                Direction::Horizontal
            })
            .constraints(if area.width < 110 {
                vec![Constraint::Length(8), Constraint::Min(6)]
            } else {
                vec![Constraint::Percentage(70), Constraint::Percentage(30)]
            })
            .split(sections[2]);
        frame.render_widget(self.run_log_widget(), bottom_sections[0]);
        frame.render_widget(self.run_actions_widget(), bottom_sections[1]);
    }

    pub(super) fn run_subview_tabs(&self, width: u16) -> Tabs<'static> {
        let titles = run_subview_titles(width)
            .into_iter()
            .map(Line::from)
            .collect::<Vec<_>>();
        Tabs::new(titles)
            .select(
                RunSubview::all()
                    .iter()
                    .position(|item| *item == self.run_subview)
                    .unwrap_or(0),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("执行视图")
                    .border_style(if self.run_focus == RunFocus::Subviews {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    }),
            )
    }

    pub(super) fn start_main_widget(&self) -> Paragraph<'_> {
        let mut lines = vec![
            Line::from(Span::styled(
                "一句话写下你想完成的事",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(if self.form.task.trim().is_empty() {
                Span::styled(
                    "下一步：按 Enter 写任务。",
                    Style::default().fg(Color::LightRed),
                )
            } else {
                Span::styled(
                    truncate(&self.form.task, 160),
                    Style::default().fg(Color::White),
                )
            }),
            Line::from(if self.form.task.trim().is_empty() {
                Span::raw("")
            } else {
                Span::styled(
                    "下一步：右侧可先选“先看方案”，也可以直接“开始运行”。",
                    Style::default().fg(Color::LightGreen),
                )
            }),
            Line::from(format!(
                "当前设置：{} / {}",
                thinking_mode_user_title(self.form.thinking_mode),
                advanced_settings_summary(&self.form)
            )),
            Line::from("高级设置：按 `a` 或右侧“高级设置”，可切换 Codex 思考强度。"),
            Line::from(""),
            Line::from("保存：`Ctrl+S`  快速定位：`Ctrl+P` / `Ctrl+R`（不会自动执行）"),
            Line::from(format!("仓库：{}", self.project.display_target)),
        ];
        if let Some(error) = &self.project.last_error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("阻塞：{}", truncate(error, 120)),
                Style::default().fg(Color::Red),
            )));
        }
        if let Some(report) = &self.last_doctor_report {
            lines.push(Line::from(format!(
                "最近检查：{} / {}",
                report.readiness.label(),
                report.summary
            )));
        }

        Paragraph::new(lines)
            .block(
                Block::default()
                    .title("任务输入")
                    .borders(Borders::ALL)
                    .border_style(if self.start_focus == StartFocus::TaskInput {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    }),
            )
            .wrap(Wrap { trim: true })
    }

    pub(super) fn start_actions_widget(&self) -> List<'_> {
        let items = StartAction::all()
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let selected =
                    self.start_focus == StartFocus::Actions && index == self.start_action_index;
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let (title, detail) = self.start_action_line(*action);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<10}", title), style),
                    Span::raw(detail),
                ]))
            })
            .collect::<Vec<_>>();

        List::new(items).block(
            Block::default()
                .title("主操作")
                .borders(Borders::ALL)
                .border_style(if self.start_focus == StartFocus::Actions {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                }),
        )
    }

    pub(super) fn start_recent_widget(&self) -> Paragraph<'_> {
        let plan_preview = self.command_preview_lines(ShellAction::Plan);
        let run_preview = self.command_preview_lines(ShellAction::Run);
        let mut lines = vec![
            Line::from("建议流程"),
            Line::from(""),
            Line::from("1. 先看方案：先收敛 todo、风险和执行图"),
            Line::from("2. 看方案是否可接受"),
            Line::from("3. 再决定直接执行还是继续调整"),
            Line::from(""),
            Line::from(Span::styled(
                "如果先看方案",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(truncate(&plan_preview.summary, 120)),
            Line::from(""),
            Line::from(Span::styled(
                "如果现在直接执行",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(truncate(&run_preview.summary, 120)),
            Line::from(""),
        ];

        if let Some(session) = &self.selected_session
            && session.is_plan_session()
            && let Some(plan) = &session.plan_todo
        {
            lines.push(Line::from(Span::styled(
                "最近方案",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(format!(
                "会话：{} / Todo {} 项",
                truncate(&session.id, 24),
                plan.todos.len()
            )));
            lines.push(Line::from(truncate(&plan.summary, 120)));
            lines.push(Line::from(""));
        }

        lines.extend([Line::from(Span::styled(
            "最近结果",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))]);

        if self.project.sessions.is_empty() {
            lines.push(Line::from("还没有会话记录。"));
        } else {
            for item in self.project.sessions.iter().take(3) {
                lines.push(Line::from(format!(
                    "{}  {} / {}",
                    item.created_at,
                    truncate(&item.mode_label, 4),
                    truncate(&item.task, 38),
                )));
            }
        }

        Paragraph::new(lines)
            .block(Block::default().title("低频信息").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    pub(super) fn advanced_settings_widget(&self) -> List<'_> {
        let items = advanced_fields()
            .iter()
            .enumerate()
            .map(|(index, field)| {
                let selected = index == self.selected_field;
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<18}", field.label()), style),
                    Span::raw(self.field_value(*field)),
                ]))
            })
            .collect::<Vec<_>>();

        List::new(items).block(
            Block::default()
                .title("高级设置")
                .borders(Borders::ALL)
                .border_style(if self.start_focus == StartFocus::AdvancedFields {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                }),
        )
    }

    pub(super) fn advanced_details_widget(&self) -> Paragraph<'_> {
        let preview = self.command_preview_lines(ShellAction::Run);
        let lines = vec![
            Line::from("这里集中放低频但关键的运行参数。"),
            Line::from("用 ↑↓ 选字段，Enter 修改。"),
            Line::from("其中 “Codex 思考强度” 会直接同步到 Codex reasoning effort。"),
            Line::from(format!("验证：{}", verification_summary(&self.project))),
            Line::from(""),
            Line::from(format!("预览：{}", truncate(&preview.commandline, 120))),
        ];
        Paragraph::new(lines)
            .block(Block::default().title("低频说明").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    pub(super) fn run_placeholder_widget(&self) -> Paragraph<'_> {
        let mut lines = vec![
            Line::from("现在没有任务在跑。"),
            Line::from("下一步：回开始页写任务，或去历史看上一次结果。"),
        ];
        if let Some(session) = &self.selected_session {
            lines.push(Line::from(""));
            lines.push(Line::from(format!("最近会话：{}", session.id)));
            lines.push(Line::from(format!("状态：{}", session.status.label())));
        }
        Paragraph::new(lines)
            .block(Block::default().title("当前状态").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    pub(super) fn run_doctor_widget(&self) -> Paragraph<'_> {
        let mut lines = Vec::new();
        if let Some(report) = &self.last_doctor_report {
            lines.push(Line::from(Span::styled(
                format!(
                    "检查结论：{} / {}",
                    report.readiness.label(),
                    report.summary
                ),
                Style::default()
                    .fg(if report.ok {
                        Color::LightGreen
                    } else {
                        Color::LightRed
                    })
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            for check in &report.checks {
                lines.push(Line::from(format!(
                    "[{}] {} - {}",
                    check.status.label(),
                    check.name,
                    truncate(&check.detail, 96)
                )));
            }
        } else if let Some(command) = &self.active_command {
            lines.push(Line::from("正在检查环境、配置和验证条件…"));
            lines.push(Line::from(""));
            for line in command.output.iter().rev().take(8).rev() {
                lines.push(Line::from(line.clone()));
            }
        } else {
            lines.push(Line::from("还没有环境检查结果。"));
            lines.push(Line::from("下一步：回开始页执行“检查环境”。"));
        }

        Paragraph::new(lines)
            .block(Block::default().title("环境检查").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    pub(super) fn run_log_widget(&self) -> Paragraph<'_> {
        Paragraph::new(self.run_log_lines())
            .block(
                Block::default()
                    .title("执行状态 / 事件")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false })
    }

    pub(super) fn run_log_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Some(command) = &self.active_command {
            let elapsed_secs = command_elapsed_secs(command);
            lines.push(Line::from(format!(
                "现在在做：{} / {}",
                command.action.label(),
                command.state.label()
            )));
            lines.push(Line::from(if command.state.is_running() {
                format!("已运行：{}s", elapsed_secs)
            } else {
                format!("总耗时：{}s", elapsed_secs)
            }));
            if command.state.is_running() && command.stop_requested {
                lines.push(Line::from("停止请求已发送，系统正在等待安全收口。"));
            } else if command.state.is_running() && command.cancel_tx.is_some() {
                lines.push(Line::from(
                    "可以等待，也可以切去“开始”或“历史”；如需中止可按 `s`。",
                ));
            } else if command.state.is_running() {
                lines.push(Line::from("当前动作不支持中途停止，请等待它自然结束。"));
            } else {
                lines.push(Line::from("这次已经结束。可回上一级，或去历史查看结果。"));
            }
            lines.push(Line::from(""));
            for line in command.output.iter().rev().take(8).rev() {
                lines.push(Line::from(line.clone()));
            }
        } else {
            lines.push(Line::from("这里会显示你刚刚做了什么。"));
            lines.push(Line::from("现在没有运行中的动作。"));
            lines.push(Line::from("下一步：回开始页开始，或去历史查看结果。"));
        }
        lines
    }

    pub(super) fn run_actions_widget(&self) -> List<'_> {
        let items = RunAction::all()
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let selected =
                    self.run_focus == RunFocus::Actions && index == self.run_action_index;
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let (title, detail) = self.run_action_line(*action);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<12}", title), style),
                    Span::raw(detail),
                ]))
            })
            .collect::<Vec<_>>();

        List::new(items).block(
            Block::default()
                .title("下一步")
                .borders(Borders::ALL)
                .border_style(if self.run_focus == RunFocus::Actions {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                }),
        )
    }

    pub(super) fn run_timeline_widget(&self) -> Paragraph<'_> {
        let mut lines = Vec::new();
        if let Some(runtime_state) = &self.runtime_state {
            lines.push(Line::from(format!(
                "当前阶段：{}",
                runtime_state.current_user_stage
            )));
            lines.push(Line::from(format!(
                "现在在做：{}",
                runtime_state.current_user_message
            )));
            lines.push(Line::from(format!(
                "下一步：{}",
                runtime_state.next_user_step
            )));
            if let Some(active_worker) = runtime_state.active_worker.as_deref() {
                lines.push(Line::from(format!("当前焦点：{active_worker}")));
            }
            lines.push(Line::from(""));
            let execution_lines = runtime_state.execution_entry_texts(12);
            if execution_lines.is_empty() {
                lines.push(Line::from("执行流还没有新的事件。"));
            } else {
                for line in execution_lines {
                    lines.push(Line::from(truncate(&line, 96)));
                }
            }
        } else if let Some(session) = &self.selected_session {
            lines.push(Line::from(format!("历史会话：{}", session.id)));
            lines.push(Line::from(""));
            if session.timeline_events.is_empty() {
                lines.push(Line::from("该会话还没有执行记录。"));
            } else {
                for item in session.timeline_events.iter().rev().take(10).rev() {
                    lines.push(Line::from(format!(
                        "- {} / {} / {}",
                        format_beijing(item.ts, "%H:%M:%S"),
                        item.title,
                        truncate(&item.detail, 72)
                    )));
                }
            }
        } else {
            lines.push(Line::from("暂无可展示的执行流。"));
            lines.push(Line::from("先执行或回放一次任务，即可在这里回看关键推进。"));
        }

        Paragraph::new(lines)
            .block(Block::default().title("执行流").borders(Borders::ALL))
            .wrap(Wrap { trim: false })
    }

    pub(super) fn run_timeline_detail_widget(&self) -> Paragraph<'_> {
        let mut lines = Vec::new();
        if let Some(runtime_state) = &self.runtime_state {
            lines.push(Line::from(format!(
                "技术细节 / {} / {}",
                runtime_state.phase, runtime_state.current_user_stage
            )));
            if let Some(brain) = &runtime_state.brain {
                lines.push(Line::from(format!(
                    "Brain：{} / 风险 {}",
                    truncate(&brain.current_focus, 72),
                    brain.risk_level.label()
                )));
            }
            lines.push(Line::from(""));
            if let Some(active_worker_id) = runtime_state.active_worker.as_deref()
                && let Some(worker) = runtime_state.workers.get(active_worker_id)
            {
                lines.push(Line::from(format!("焦点 Worker：{}", active_worker_id)));
                lines.push(Line::from(format!("角色：{}", worker.role)));
                lines.push(Line::from(format!("标题：{}", truncate(&worker.title, 88))));
                if let Some(todo_id) = worker.todo_id.as_deref() {
                    lines.push(Line::from(format!("Todo：{todo_id}")));
                }
                lines.push(Line::from(format!(
                    "队列态：{}",
                    worker.queue_state.label()
                )));
                lines.push(Line::from(format!("状态：{}", worker.status.label())));
                lines.push(Line::from(format!("阶段：{}", worker.phase_label)));
                if let Some(reason) = worker.blocked_reason.as_ref() {
                    lines.push(Line::from(format!(
                        "阻塞：{} / {}",
                        reason.label(),
                        truncate(&reason.detail, 72)
                    )));
                }
                lines.push(Line::from(format!(
                    "最近事件：{}",
                    truncate(&worker.last_event, 88)
                )));
                lines.push(Line::from(format!(
                    "Worktree：{}",
                    truncate(&worker.worktree_path, 88)
                )));
                lines.push(Line::from(""));
            }
            let execution_lines = runtime_state.execution_entry_texts(36);
            if execution_lines.is_empty() {
                lines.push(Line::from("当前还没有新的执行流细节。"));
            } else {
                for line in execution_lines {
                    lines.push(Line::from(line));
                }
            }
        } else if let Some(command) = &self.active_command {
            lines.push(Line::from(format!(
                "技术细节 / {} / {}",
                command.action.label(),
                command.state.label()
            )));
            lines.push(Line::from(""));
            for line in command.output.iter().rev().take(36).rev() {
                lines.push(Line::from(line.clone()));
            }
        } else if let Some(session) = &self.selected_session {
            lines.push(Line::from(format!("历史会话：{}", session.id)));
            lines.push(Line::from(format!(
                "timeline：{}",
                session.timeline_path.display()
            )));
            lines.push(Line::from(""));
            if session.timeline_events.is_empty() {
                lines.push(Line::from("该会话还没有事件流记录。"));
            } else {
                for item in session.timeline_events.iter().rev().take(20).rev() {
                    lines.push(Line::from(format!(
                        "{}  {} / {}",
                        format_beijing(item.ts, "%H:%M:%S"),
                        item.title,
                        truncate(&item.detail, 88)
                    )));
                }
            }
        } else {
            lines.push(Line::from("这里显示执行流明细、Worker 焦点和技术细节。"));
        }

        Paragraph::new(lines)
            .block(Block::default().title("技术细节").borders(Borders::ALL))
            .wrap(Wrap { trim: false })
    }

    pub(super) fn run_readonly_widget(&self) -> Paragraph<'_> {
        let mut lines = Vec::new();
        if let Some(command) = &self.active_command {
            lines.push(Line::from(Span::styled(
                format!("{}：{}", command.action.label(), command.state.label()),
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            if matches!(command.action, ShellAction::Doctor)
                && let Some(report) = &self.last_doctor_report
            {
                lines.push(Line::from(format!(
                    "检查结论：{} / {}",
                    report.readiness.label(),
                    report.summary
                )));
                lines.push(Line::from(""));
            }
            for line in command.output.iter().rev().take(18).rev() {
                lines.push(Line::from(line.clone()));
            }
        } else {
            lines.push(Line::from("这里会展示只读检查结果。"));
            lines.push(Line::from(
                "可从开始页执行“检查环境”“校验配置”或“查看角色”。",
            ));
        }

        Paragraph::new(lines)
            .block(Block::default().title("检查结果").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    pub(super) fn run_summary_widget(&self) -> Paragraph<'_> {
        let mut lines = Vec::new();
        if let Some(summary) = self
            .runtime_state
            .as_ref()
            .and_then(|state| state.summary.as_ref())
            .or_else(|| {
                self.selected_session
                    .as_ref()
                    .and_then(|session| session.final_summary.as_ref())
            })
        {
            let deliverables = self
                .selected_session
                .as_ref()
                .map(existing_deliverables)
                .unwrap_or_default();
            let system_artifacts = self
                .selected_session
                .as_ref()
                .map(existing_system_artifacts)
                .unwrap_or_default();
            lines.push(Line::from(Span::styled(
                "最终结果",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(summary.overview.clone()));
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "结果：{}   Apply：{}   可信度：{}",
                summary.result_status.label(),
                summary.apply_status.label(),
                summary.trust_level.label()
            )));
            lines.push(Line::from(format!(
                "接收文件：{}",
                if summary.accepted_files.is_empty() {
                    "无".to_string()
                } else {
                    truncate(&summary.accepted_files.join("；"), 120)
                }
            )));
            lines.push(Line::from(format!(
                "人工复核：{}",
                if summary.manual_review_files.is_empty() {
                    "无".to_string()
                } else {
                    truncate(&summary.manual_review_files.join("；"), 120)
                }
            )));
            lines.push(Line::from(format!(
                "开放风险：{}",
                if summary.open_risks.is_empty() {
                    "无".to_string()
                } else {
                    truncate(&summary.open_risks.join("；"), 120)
                }
            )));
            if let Some(session) = self.selected_session.as_ref() {
                lines.push(Line::from(format!(
                    "目标目录状态：{} / {}",
                    delivery_status_label(session),
                    truncate(&delivery_status_detail(session), 120)
                )));
            }
            lines.push(Line::from(format!(
                "系统工件目录：{}",
                self.selected_session
                    .as_ref()
                    .map(|session| session.session_dir.display().to_string())
                    .unwrap_or_else(|| format!("{}/.codex-forge", self.project.display_target))
            )));
            if system_artifacts.is_empty() {
                lines.push(Line::from("系统工件：尚未落盘。"));
            } else {
                lines.push(Line::from("系统工件："));
                for path in system_artifacts {
                    lines.push(Line::from(format!("- {}", path.display())));
                }
            }
            if deliverables.is_empty() {
                let fallback = if let Some(session) = self.selected_session.as_ref() {
                    format!(
                        "用户结果件：当前会话尚未写入目标目录。{}",
                        truncate(&delivery_status_detail(session), 96)
                    )
                } else {
                    "用户结果件：当前会话尚未导出到仓库根目录。".to_string()
                };
                lines.push(Line::from(fallback));
            } else {
                lines.push(Line::from("用户结果件："));
                for path in deliverables {
                    lines.push(Line::from(format!("- {}", path.display())));
                }
            }
        } else if let Some(runtime_state) = &self.runtime_state {
            lines.push(Line::from("结果摘要尚未生成。"));
            lines.push(Line::from(format!(
                "当前阶段：{}",
                runtime_state.current_user_stage
            )));
            lines.push(Line::from(format!(
                "Apply：{}",
                runtime_state
                    .apply_status
                    .clone()
                    .unwrap_or_else(|| "等待".to_string())
            )));
            lines.push(Line::from(format!(
                "验证：{}",
                runtime_state
                    .verify_status
                    .clone()
                    .unwrap_or_else(|| "等待".to_string())
            )));
            lines.push(Line::from("可先切到“进度总览”或“执行视图”查看当前进展。"));
        } else if let Some(session) = &self.selected_session {
            lines.push(Line::from(format!("历史会话：{}", session.id)));
            lines.push(Line::from(format!("状态：{}", session.status.label())));
            lines.push(Line::from("这次还没有生成最终摘要。"));
        } else {
            lines.push(Line::from("这里会显示最后结果。"));
            lines.push(Line::from("先运行一次，或回放历史。"));
        }

        Paragraph::new(lines)
            .block(Block::default().title("最终结果").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    pub(super) fn history_left_widget(&self) -> List<'_> {
        let items = self
            .project
            .sessions
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let style = if index == self.history_index {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(format!("{}  ", item.created_at), style),
                        Span::raw(format!(
                            "{} / {} / {}",
                            item.mode_label,
                            item.stage_label,
                            if item.continuable {
                                "可继续"
                            } else {
                                "进行中"
                            }
                        )),
                    ]),
                    Line::from(truncate(&item.task, 52)),
                    Line::from(truncate(&item.summary, 52)),
                ])
            })
            .collect::<Vec<_>>();
        List::new(items).block(
            Block::default()
                .title("历史会话")
                .borders(Borders::ALL)
                .border_style(if self.history_focus == HistoryFocus::Sessions {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                }),
        )
    }

    pub(super) fn history_right_widget(&self) -> Paragraph<'_> {
        let lines = if let Some(session) = &self.selected_session {
            let next_step = if session.is_plan_session() {
                "下一步：右侧选“执行此方案”“回放过程”或“查看详情”。"
            } else {
                "下一步：右侧选“继续优化”“回放过程”或“查看详情”。"
            };
            let mut lines = vec![
                Line::from(format!("当前会话：{}", truncate(&session.task, 80))),
                Line::from(format!("状态：{}", session.status.label())),
                Line::from(format!("类型：{}", session.session_kind.label())),
                Line::from(format!(
                    "创建时间：{}",
                    format_beijing(session.created_at, "%m-%d %H:%M")
                )),
                Line::from(format!("交付物目录：{}", session.repo_root().display())),
                Line::from(format!(
                    "目标目录状态：{} / {}",
                    delivery_status_label(session),
                    truncate(&delivery_status_detail(session), 80)
                )),
                Line::from(""),
                Line::from(next_step),
                Line::from("详情页默认左侧是用户摘要，右侧才是技术细节。"),
            ];
            if let Some(plan) = &session.plan_todo {
                lines.push(Line::from(format!(
                    "方案摘要：{}",
                    truncate(&plan.summary, 96)
                )));
            }
            if let Some(summary) = &session.final_summary {
                lines.push(Line::from(format!(
                    "最终结论：{}",
                    truncate(&summary.overview, 96)
                )));
            } else if session.is_plan_session() {
                lines.push(Line::from("最终结论：这是方案会话；尚未进入代码落地阶段。"));
            }
            if session.is_run_session() {
                lines.push(Line::from(format!(
                    "继续模式：{}",
                    continue_mode_user_label(self.form.continue_mode)
                )));
                lines.push(Line::from(format!(
                    "继续反馈：{}",
                    if self.form.continue_feedback.trim().is_empty() {
                        "未填写；可以直接继续，也可以先在右侧动作区补充反馈。".to_string()
                    } else {
                        truncate(&self.form.continue_feedback, 100)
                    }
                )));
            }
            lines
        } else {
            vec![
                Line::from("先在左侧选中一个会话。"),
                Line::from("右侧动作区会显示继续优化、回放、查看详情和清理入口。"),
                Line::from("选中后可按 Enter 查看详情，也可用 e / v / z / x 快速操作。"),
            ]
        };
        Paragraph::new(lines)
            .block(Block::default().title("当前会话").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    pub(super) fn history_actions_widget(&self) -> List<'_> {
        let actions = available_history_actions(self.selected_session.as_ref());
        let items = actions
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let selected = self.history_focus == HistoryFocus::Actions
                    && index == self.history_action_index;
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let (title, detail) = self.history_action_line(*action);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<12}", title), style),
                    Span::raw(detail),
                ]))
            })
            .collect::<Vec<_>>();

        List::new(items).block(
            Block::default()
                .title("下一步")
                .borders(Borders::ALL)
                .border_style(if self.history_focus == HistoryFocus::Actions {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                }),
        )
    }

    pub(super) fn render_history_detail_popup(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let Some(detail) = &self.history_detail else {
            return;
        };
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(6),
            ])
            .split(area);

        let active_label = detail.active_tab.label();
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    format!(
                        "详情 / {} / {} / {}",
                        truncate(detail.session_id(), 28),
                        active_label,
                        detail.page_summary()
                    ),
                    Style::default()
                        .fg(Color::LightGreen)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from("这里能看到计划、过程和产物。"),
                Line::from("Tab/←→ 切页，↑↓ 滚动，PgUp/PgDn 翻页，Esc 关闭。"),
            ])
            .block(Block::default().title("历史详情").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
            sections[0],
        );
        frame.render_widget(self.history_detail_tabs_widget(area.width), sections[1]);
        let content_sections = Layout::default()
            .direction(if area.width < 120 {
                Direction::Vertical
            } else {
                Direction::Horizontal
            })
            .constraints(if area.width < 120 {
                vec![Constraint::Percentage(36), Constraint::Percentage(64)]
            } else {
                vec![Constraint::Percentage(38), Constraint::Percentage(62)]
            })
            .split(sections[2]);

        frame.render_widget(
            Paragraph::new(detail.summary.clone())
                .block(Block::default().title("用户摘要").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            content_sections[0],
        );
        frame.render_widget(
            Paragraph::new(detail.current_page_text())
                .block(Block::default().title("技术细节").borders(Borders::ALL))
                .wrap(Wrap { trim: false })
                .scroll((detail.scroll, 0)),
            content_sections[1],
        );
        frame.render_widget(
            Paragraph::new(history_detail_shortcuts_lines())
                .block(Block::default().title("快捷键").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            sections[3],
        );
    }

    pub(super) fn render_manual_review_popup(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let Some(review) = &self.manual_review else {
            return;
        };

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Min(12),
                Constraint::Length(5),
            ])
            .split(area);

        let header = vec![
            Line::from(Span::styled(
                format!(
                    "人工审查 / {} / {} / {}",
                    truncate(&review.session_id, 24),
                    review.diff_view.label(),
                    review
                        .selected_file()
                        .map(|item| item.status.label())
                        .unwrap_or("无文件")
                ),
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(
                review
                    .selected_file()
                    .map(|item| format!("当前文件：{}", item.path))
                    .unwrap_or_else(|| "当前没有可审查文件。".to_string()),
            ),
            Line::from("左侧选文件，右侧执行通过/修复/交付；`[` `]` 切换 diff 视图，`Esc` 关闭。"),
        ];
        frame.render_widget(
            Paragraph::new(header)
                .block(Block::default().title("人工审查").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            sections[0],
        );

        let content = Layout::default()
            .direction(if area.width < 150 {
                Direction::Vertical
            } else {
                Direction::Horizontal
            })
            .constraints(if area.width < 150 {
                vec![
                    Constraint::Length(10),
                    Constraint::Min(10),
                    Constraint::Length(9),
                ]
            } else {
                vec![
                    Constraint::Percentage(24),
                    Constraint::Percentage(56),
                    Constraint::Percentage(20),
                ]
            })
            .split(sections[1]);

        let file_items = review
            .state
            .files
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let selected =
                    review.focus == ManualReviewFocus::Files && index == review.file_index;
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(vec![
                    Line::from(Span::styled(truncate(&item.path, 40), style)),
                    Line::from(format!(
                        "{} / {}",
                        item.status.label(),
                        item.source_workers.join("、")
                    )),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(file_items).block(
                Block::default()
                    .title("待审文件")
                    .borders(Borders::ALL)
                    .border_style(if review.focus == ManualReviewFocus::Files {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    }),
            ),
            content[0],
        );

        frame.render_widget(
            Paragraph::new(build_manual_review_detail_text(
                self.project.target_dir.as_path(),
                &review.state,
                review.file_index,
                review.diff_view,
            ))
            .block(Block::default().title("Diff / 细节").borders(Borders::ALL))
            .wrap(Wrap { trim: false })
            .scroll((review.scroll, 0)),
            content[1],
        );

        let action_items = ManualReviewAction::all()
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let selected =
                    review.focus == ManualReviewFocus::Actions && index == review.action_index;
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let (title, detail) = self.manual_review_action_line(*action);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<10}", title), style),
                    Span::raw(detail),
                ]))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(action_items).block(
                Block::default()
                    .title("动作")
                    .borders(Borders::ALL)
                    .border_style(if review.focus == ManualReviewFocus::Actions {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    }),
            ),
            content[2],
        );

        frame.render_widget(
            Paragraph::new(vec![
                Line::from("快捷键：↑↓ 选文件/动作，←→ 切焦点，Enter 执行动作，a 通过，n 需修复，e 编辑问题，f 发起返修，d 交付已通过，r 刷新，PgUp/PgDn 滚动。"),
                Line::from(format!(
                    "审查状态：已通过 {} / 待修复 {} / 返修待复查 {}",
                    review
                        .state
                        .files
                        .iter()
                        .filter(|item| item.status == ManualReviewFileStatus::Approved)
                        .count(),
                    review
                        .state
                        .files
                        .iter()
                        .filter(|item| item.status == ManualReviewFileStatus::NeedsFix)
                        .count(),
                    review
                        .state
                        .files
                        .iter()
                        .filter(|item| item.status == ManualReviewFileStatus::FixedPendingReview)
                        .count(),
                )),
            ])
            .block(Block::default().title("提示").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
            sections[2],
        );
    }

    pub(super) fn manual_review_action_line(
        &self,
        action: ManualReviewAction,
    ) -> (&'static str, String) {
        match action {
            ManualReviewAction::Approve => {
                ("通过文件", "当前文件审查通过，可进入交付集合。".to_string())
            }
            ManualReviewAction::NeedsFix => {
                ("标记问题", "把当前文件标成需修复，并记录问题。".to_string())
            }
            ManualReviewAction::EditIssue => (
                "编辑问题",
                self.manual_review
                    .as_ref()
                    .and_then(|review| review.selected_file())
                    .and_then(|item| item.issue_summary.clone())
                    .unwrap_or_else(|| "补充这次返修的具体问题描述。".to_string()),
            ),
            ManualReviewAction::StartFix => (
                "发起返修",
                "只基于当前文件启动 Codex 返修子会话。".to_string(),
            ),
            ManualReviewAction::DeliverApproved => (
                "交付已通过",
                "只把已人工通过的文件交付到目标目录。".to_string(),
            ),
            ManualReviewAction::Close => ("关闭审查", "返回历史页。".to_string()),
        }
    }

    pub(super) fn current_manual_review_action(&self) -> ManualReviewAction {
        ManualReviewAction::all()[self
            .manual_review
            .as_ref()
            .map(|review| review.action_index.min(ManualReviewAction::all().len() - 1))
            .unwrap_or(0)]
    }

    pub(super) fn open_manual_review_popup_for(
        &mut self,
        session_id: &str,
        preferred_file: Option<&str>,
    ) -> Result<()> {
        let session = load_session(&self.project.target_dir, Some(session_id))?;
        if !can_open_manual_review(&session) {
            self.push_notice("当前会话没有需要人工审查的文件。");
            return Ok(());
        }
        let state = load_or_initialize_manual_review_state(&self.project.target_dir, &session)?;
        let file_index = preferred_file
            .or(state.selected_file.as_deref())
            .and_then(|path| state.files.iter().position(|item| item.path == path))
            .unwrap_or(0);
        self.selected_session = Some(session);
        if let Some(index) = self
            .project
            .sessions
            .iter()
            .position(|item| item.id == session_id)
        {
            self.history_index = index;
        }
        self.manual_review = Some(ManualReviewPopupState {
            session_id: session_id.to_string(),
            state,
            file_index,
            action_index: 0,
            focus: ManualReviewFocus::Files,
            diff_view: ManualReviewDiffView::Source,
            scroll: 0,
        });
        self.navigate_to(Route::History);
        Ok(())
    }

    pub(super) async fn handle_manual_review_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.manual_review.is_none() {
            return Ok(());
        }
        match key.code {
            KeyCode::Esc => {
                self.manual_review = None;
            }
            KeyCode::Tab | KeyCode::Left | KeyCode::Right
                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let review = self.manual_review.as_mut().expect("review exists");
                review.focus = match review.focus {
                    ManualReviewFocus::Files => ManualReviewFocus::Actions,
                    ManualReviewFocus::Actions => ManualReviewFocus::Files,
                };
            }
            KeyCode::Up => {
                let review = self.manual_review.as_mut().expect("review exists");
                match review.focus {
                    ManualReviewFocus::Files => {
                        let len = review.state.files.len().max(1);
                        review.file_index = cycle_index(review.file_index, len, false);
                        review.scroll = 0;
                    }
                    ManualReviewFocus::Actions => {
                        review.action_index = cycle_index(
                            review.action_index,
                            ManualReviewAction::all().len(),
                            false,
                        );
                    }
                }
            }
            KeyCode::Down => {
                let review = self.manual_review.as_mut().expect("review exists");
                match review.focus {
                    ManualReviewFocus::Files => {
                        let len = review.state.files.len().max(1);
                        review.file_index = cycle_index(review.file_index, len, true);
                        review.scroll = 0;
                    }
                    ManualReviewFocus::Actions => {
                        review.action_index =
                            cycle_index(review.action_index, ManualReviewAction::all().len(), true);
                    }
                }
            }
            KeyCode::PageUp => {
                let review = self.manual_review.as_mut().expect("review exists");
                review.scroll = review.scroll.saturating_sub(12);
            }
            KeyCode::PageDown => {
                let review = self.manual_review.as_mut().expect("review exists");
                review.scroll = review.scroll.saturating_add(12);
            }
            KeyCode::Char('[') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let review = self.manual_review.as_mut().expect("review exists");
                review.diff_view = match review.diff_view {
                    ManualReviewDiffView::Source => ManualReviewDiffView::Compare,
                    ManualReviewDiffView::LatestFix => ManualReviewDiffView::Source,
                    ManualReviewDiffView::Compare => ManualReviewDiffView::LatestFix,
                };
                review.scroll = 0;
            }
            KeyCode::Char(']') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let review = self.manual_review.as_mut().expect("review exists");
                review.diff_view = match review.diff_view {
                    ManualReviewDiffView::Source => ManualReviewDiffView::LatestFix,
                    ManualReviewDiffView::LatestFix => ManualReviewDiffView::Compare,
                    ManualReviewDiffView::Compare => ManualReviewDiffView::Source,
                };
                review.scroll = 0;
            }
            KeyCode::Char('a') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.approve_current_review_file()?;
            }
            KeyCode::Char('n') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.mark_current_review_file_needs_fix()?;
            }
            KeyCode::Char('e') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_editing(FormField::ReviewIssue);
            }
            KeyCode::Char('f') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_manual_review_fix().await?;
            }
            KeyCode::Char('d') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.deliver_manual_review_approved().await?;
            }
            KeyCode::Char('r') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let review = self.manual_review.as_ref().expect("review exists");
                let session_id = review.session_id.clone();
                let preferred = review.selected_file().map(|item| item.path.clone());
                self.open_manual_review_popup_for(&session_id, preferred.as_deref())?;
            }
            KeyCode::Enter => match self.current_manual_review_action() {
                ManualReviewAction::Approve => self.approve_current_review_file()?,
                ManualReviewAction::NeedsFix => self.mark_current_review_file_needs_fix()?,
                ManualReviewAction::EditIssue => self.start_editing(FormField::ReviewIssue),
                ManualReviewAction::StartFix => self.start_manual_review_fix().await?,
                ManualReviewAction::DeliverApproved => {
                    self.deliver_manual_review_approved().await?
                }
                ManualReviewAction::Close => self.manual_review = None,
            },
            _ => {}
        }
        Ok(())
    }

    pub(super) fn history_detail_tabs_widget(&self, width: u16) -> Tabs<'static> {
        let titles = history_detail_tab_titles(width)
            .into_iter()
            .map(Line::from)
            .collect::<Vec<_>>();
        let selected = self
            .history_detail
            .as_ref()
            .and_then(|detail| {
                HistoryDetailTab::all()
                    .iter()
                    .position(|item| *item == detail.active_tab)
            })
            .unwrap_or(0);
        Tabs::new(titles)
            .select(selected)
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .block(Block::default().borders(Borders::ALL).title("详情分页"))
    }

    pub(super) fn render_confirm_popup(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let Some(confirm) = &self.confirm_dialog else {
            return;
        };

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(4)])
            .split(area);
        let lines = confirm
            .lines
            .iter()
            .cloned()
            .map(Line::from)
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines)
                .block(
                    Block::default()
                        .title(confirm.title.as_str())
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: true }),
            sections[0],
        );
        frame.render_widget(
            Paragraph::new(confirm_shortcuts_lines(&confirm.action))
                .block(Block::default().title("快捷键").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            sections[1],
        );
    }

    pub(super) fn render_edit_popup(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let Some(edit) = &self.edit_state else {
            return;
        };
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Min(6),
                Constraint::Length(4),
            ])
            .split(area);

        let header_lines = vec![
            Line::from(Span::styled(
                format!("编辑字段：{}", edit.field.label()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "字符数：{}   光标：{}{}",
                edit.buffer.chars().count(),
                edit.cursor,
                if edit.field == FormField::Task && !edit.history_entries.is_empty() {
                    format!(
                        "   历史：{}/{}",
                        edit.history_index.map(|index| index + 1).unwrap_or(0),
                        edit.history_entries.len()
                    )
                } else {
                    String::new()
                }
            )),
            Line::from(edit_mode_summary(edit.field)),
        ];
        frame.render_widget(
            Paragraph::new(header_lines)
                .block(Block::default().title("编辑模式").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            sections[0],
        );

        let input_text = if edit.buffer.is_empty() {
            " ".to_string()
        } else {
            edit.buffer.clone()
        };
        frame.render_widget(
            Paragraph::new(input_text)
                .block(Block::default().title("内容").borders(Borders::ALL))
                .wrap(Wrap { trim: false }),
            sections[1],
        );
        frame.render_widget(
            Paragraph::new(
                edit_shortcuts_lines(edit.field)
                    .into_iter()
                    .chain(if edit.field == FormField::Task {
                        let lines = recent_task_history_lines(edit);
                        if lines.is_empty() {
                            vec![Line::from("当前目标目录还没有历史提示词。")]
                        } else {
                            let mut result = vec![Line::from(""), Line::from("历史提示词：")];
                            result.extend(lines);
                            result
                        }
                    } else {
                        Vec::new()
                    })
                    .collect::<Vec<_>>(),
            )
            .block(Block::default().title("快捷键").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
            sections[2],
        );

        let inner_width = sections[1].width.saturating_sub(2) as usize;
        let (cursor_row, cursor_col) =
            wrapped_cursor_row_col(&edit.buffer, edit.cursor, inner_width);
        let x = sections[1]
            .x
            .saturating_add(1)
            .saturating_add(cursor_col as u16)
            .min(sections[1].right().saturating_sub(2));
        let y = sections[1]
            .y
            .saturating_add(1)
            .saturating_add(cursor_row as u16)
            .min(sections[1].bottom().saturating_sub(2));
        frame.set_cursor_position(Position::new(x, y));
    }

    pub(super) fn start_action_line(&self, action: StartAction) -> (&'static str, String) {
        match action {
            StartAction::Doctor => ("检查环境", "先排掉明显问题。".to_string()),
            StartAction::Plan => ("先看方案", "先生成 todo、风险和执行图。".to_string()),
            StartAction::Run => ("开始运行", run_source_user_hint(&self.form)),
            StartAction::ConfigValidate => {
                ("校验配置", "确认配置、规则和角色集合是否有效。".to_string())
            }
            StartAction::AgentsList => ("查看角色", "看当前仓库有哪些角色和角色集合。".to_string()),
            StartAction::ToggleSettings => (
                if self.advanced_settings_open {
                    "收起设置"
                } else {
                    "高级设置"
                },
                if self.advanced_settings_open {
                    "回到默认模式。".to_string()
                } else {
                    "切换 Codex 思考强度等低频项。".to_string()
                },
            ),
        }
    }

    pub(super) fn history_action_line(&self, action: HistoryAction) -> (&'static str, String) {
        match action {
            HistoryAction::ExecutePlan => (
                "执行此方案",
                "复用当前方案会话，直接进入落地执行。".to_string(),
            ),
            HistoryAction::Continue => (
                "继续优化",
                "基于这一轮补充反馈，重新规划后继续执行。".to_string(),
            ),
            HistoryAction::EditFeedback => (
                "补充反馈",
                if self.form.continue_feedback.trim().is_empty() {
                    "可写，也可以留空直接继续。".to_string()
                } else {
                    truncate(&self.form.continue_feedback, 56)
                },
            ),
            HistoryAction::ContinueMode => (
                "继续模式",
                format!(
                    "当前：{}",
                    continue_mode_user_label(self.form.continue_mode)
                ),
            ),
            HistoryAction::ManualWrite => (
                "手动写入",
                "自动未写入时，把 accepted_files 直接写到目标目录。".to_string(),
            ),
            HistoryAction::Replay => ("回放过程", "重新看一遍关键过程。".to_string()),
            HistoryAction::Detail => ("查看详情", "看完整计划、过程和产物。".to_string()),
            HistoryAction::ResetSelected => (
                "一键重置",
                "回退这一轮自动提交，并抹掉对应历史。".to_string(),
            ),
            HistoryAction::CleanSelected => ("删除当前", "删除这一轮及其后续。".to_string()),
            HistoryAction::CleanAll => ("清空全部", "清掉这个仓库的全部历史。".to_string()),
            HistoryAction::BackToStart => ("回开始页", "回去改任务或重新开始。".to_string()),
        }
    }

    pub(super) fn run_action_line(&self, action: RunAction) -> (&'static str, String) {
        match action {
            RunAction::Back => (
                "返回上一级",
                if self.is_command_running() {
                    "只离开页面，不会停任务。".to_string()
                } else {
                    "回到上一个页面。".to_string()
                },
            ),
            RunAction::Stop => {
                let command = self.active_command.as_ref();
                (
                    if command.is_some_and(|item| item.state.is_running() && item.stop_requested) {
                        "停止请求已发出"
                    } else if command
                        .is_some_and(|item| item.state.is_running() && item.cancel_tx.is_some())
                    {
                        "停止当前动作"
                    } else {
                        "停止不可用"
                    },
                    if command.is_some_and(|item| item.state.is_running() && item.stop_requested) {
                        "系统正在等待安全收口完成。".to_string()
                    } else if command
                        .is_some_and(|item| item.state.is_running() && item.cancel_tx.is_some())
                    {
                        "安全停止，不是强杀。".to_string()
                    } else if command.is_some_and(|item| item.state.is_running()) {
                        "当前动作暂不支持安全停止。".to_string()
                    } else {
                        "现在没有可停的动作。".to_string()
                    },
                )
            }
            RunAction::ViewHistory => ("查看历史", "去看结果、回放或继续优化。".to_string()),
            RunAction::BackToStart => ("回开始页", "回去写任务或重新开始。".to_string()),
        }
    }
}
