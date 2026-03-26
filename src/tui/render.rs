use chrono::Utc;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::commands::format::{describe_event, first_line, role_label, status_label};
use crate::harness::types::{SubagentKind, SubagentRecord};
use crate::harness::{
    HarnessMessage, HarnessRunManifest, TaskNodeKind, TaskNodeRecord, TaskNodeStatus,
};

use super::app::TuiApp;
use super::data::build_live_output_detail;
use super::tabs::{BrowsePane, FocusMode};

impl TuiApp {
    pub(crate) fn render(&mut self, frame: &mut ratatui::Frame) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5),
                Constraint::Length(3),
                Constraint::Min(18),
                Constraint::Length(6),
                Constraint::Length(2),
            ])
            .split(frame.area());

        frame.render_widget(render_header(self), layout[0]);
        frame.render_widget(render_alert_bar(self), layout[1]);

        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(32),
                Constraint::Length(40),
                Constraint::Min(54),
            ])
            .split(layout[2]);

        frame.render_widget(
            List::new(thread_items(self, body[0].width)).block(panel_block(
                &format!("Threads · {}", self.threads.len()),
                matches!(self.focus, FocusMode::Browse)
                    && matches!(self.browse_pane, BrowsePane::Threads),
            )),
            body[0],
        );

        let middle = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(9),
                Constraint::Length(8),
                Constraint::Min(8),
            ])
            .split(body[1]);
        frame.render_widget(
            Paragraph::new(current_status_text(self))
                .block(panel_block(
                    "Control Tower",
                    matches!(self.focus, FocusMode::Browse)
                        && matches!(self.browse_pane, BrowsePane::Runs),
                ))
                .wrap(Wrap { trim: true }),
            middle[0],
        );
        frame.render_widget(
            List::new(run_items(self, middle[1].width)).block(panel_block(
                &format!("Runs · {}", self.runs.len()),
                matches!(self.focus, FocusMode::Browse)
                    && matches!(self.browse_pane, BrowsePane::Runs),
            )),
            middle[1],
        );
        frame.render_widget(
            List::new(task_node_items(self, middle[2].width)).block(panel_block(
                &format!("执行步骤 · {}", self.task_nodes.len()),
                matches!(self.focus, FocusMode::Browse)
                    && matches!(self.browse_pane, BrowsePane::Steps),
            )),
            middle[2],
        );

        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(14),
                Constraint::Length(10),
                Constraint::Length(8),
            ])
            .split(body[2]);
        self.detail_viewport_width = right[0].width;
        self.detail_viewport_height = right[0].height;
        render_primary_right_panel(frame, self, right[0]);
        frame.render_widget(
            List::new(live_stream_items(self, right[1].width)).block(panel_block("活动流", false)),
            right[1],
        );
        frame.render_widget(
            List::new(message_items(self, right[2].width)).block(panel_block("最近消息", false)),
            right[2],
        );

        frame.render_widget(
            render_composer(self)
                .block(panel_block(
                    &composer_title(self),
                    matches!(self.focus, FocusMode::Compose)
                        || matches!(self.browse_pane, BrowsePane::Composer),
                ))
                .wrap(Wrap { trim: false }),
            layout[3],
        );
        if matches!(self.focus, FocusMode::Compose) {
            frame.set_cursor_position(composer_cursor_position(layout[3], &self.composer));
        }
        frame.render_widget(render_footer(self), layout[4]);
    }
}

fn render_header<'a>(app: &'a TuiApp) -> Paragraph<'a> {
    let thread_label = app.selected_thread_id.as_deref().unwrap_or("-");
    let run = current_run(app);
    let agent_name = current_agent_name(app);
    let duration = run
        .map(format_run_duration)
        .unwrap_or_else(|| "-".to_string());
    let status = run
        .map(|run| status_label(run.status).to_string())
        .unwrap_or_else(|| "-".to_string());
    let activity = current_activity(app);
    let run_id = run
        .map(|run| short_id(&run.id, 18))
        .unwrap_or_else(|| "-".to_string());
    Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                " CODEX FORGE ",
                Style::default()
                    .fg(Color::White)
                    .bg(color_brand_bg())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                app.repo_root.display().to_string(),
                Style::default().fg(color_text_dim()),
            ),
        ]),
        Line::from(vec![
            chip(
                "模式",
                if matches!(app.focus, FocusMode::Compose) {
                    "编辑"
                } else {
                    "浏览"
                },
                color_chip_neutral(),
            ),
            Span::raw(" "),
            chip("焦点", pane_label(app.browse_pane), color_chip_neutral()),
            Span::raw(" "),
            chip(
                "状态",
                &status,
                run.map(|run| run_status_color(run.status))
                    .unwrap_or(color_status_pending()),
            ),
            Span::raw(" "),
            chip("Agent", &agent_name, color_chip_neutral()),
            Span::raw(" "),
            chip("执行模式", current_backend_name(app), color_chip_neutral()),
        ]),
        Line::from(vec![
            chip("Thread", &short_id(thread_label, 18), color_chip_neutral()),
            Span::raw(" "),
            chip("Run", &run_id, color_chip_neutral()),
            Span::raw(" "),
            chip("时长", &duration, color_chip_neutral()),
            Span::raw(" "),
            chip("进度", &node_progress_text(app), color_chip_neutral()),
        ]),
        Line::from(vec![
            Span::styled(
                "当前动作 ",
                Style::default()
                    .fg(color_accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(activity),
            Span::styled("  |  ", Style::default().fg(color_border())),
            Span::styled(primary_hint(app), Style::default().fg(color_text_dim())),
        ]),
        pane_tabs_line(app),
    ])
}

fn panel_block<'a>(title: &'a str, active: bool) -> Block<'a> {
    let title_line = if active {
        Line::from(vec![
            Span::styled(
                "● ",
                Style::default()
                    .fg(color_accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
        ])
    } else {
        Line::from(vec![
            Span::styled("○ ", Style::default().fg(color_border())),
            Span::raw(title),
        ])
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title_line)
        .border_style(Style::default().fg(color_border()));
    if active {
        block.border_style(
            Style::default()
                .fg(color_accent())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        block
    }
}

fn render_alert_bar<'a>(app: &'a TuiApp) -> Paragraph<'a> {
    let alert = current_alert(app);
    Paragraph::new(alert.text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(alert.title)
                .border_style(alert.style),
        )
        .style(alert.style)
        .wrap(Wrap { trim: true })
}

fn render_primary_right_panel(frame: &mut ratatui::Frame, app: &TuiApp, area: Rect) {
    let active = matches!(app.browse_pane, BrowsePane::Error | BrowsePane::Detail);
    let (title, body) = primary_panel_content(app);
    let scroll = effective_primary_panel_scroll(app, &body, area);
    if !app.approvals.is_empty() {
        frame.render_widget(
            Paragraph::new(body)
                .block(
                    panel_block(&title, active).border_style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                )
                .style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
                .scroll((scroll, 0))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    frame.render_widget(
        Paragraph::new(body)
            .block(panel_block(&title, active))
            .scroll((scroll, 0))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn thread_items(app: &TuiApp, width: u16) -> Vec<ListItem<'static>> {
    if app.threads.is_empty() {
        return vec![ListItem::new("还没有 thread，按 n 创建")];
    }

    app.threads
        .iter()
        .map(|thread| {
            let selected = app
                .selected_thread_id
                .as_ref()
                .is_some_and(|id| id == &thread.id);
            let status_color = thread
                .last_run_status
                .map(run_status_color)
                .unwrap_or(color_chip_neutral());
            let style = if selected {
                selected_item_style()
            } else {
                Style::default().fg(color_text_main())
            };
            let prefix = if selected { "▸ " } else { "  " };
            let mut lines = wrap_lines(&thread.title, inner_wrap_width(width, prefix.len()))
                .into_iter()
                .enumerate()
                .map(|(index, line)| {
                    Line::from(vec![
                        Span::styled(if index == 0 { prefix } else { "  " }, style),
                        Span::styled(line, style),
                    ])
                })
                .collect::<Vec<_>>();
            lines.push(Line::from(vec![
                Span::styled("  ● ", Style::default().fg(status_color)),
                Span::styled(
                    format!(
                        "{} msg · {} run · {}",
                        thread.message_count,
                        thread.run_count,
                        thread.last_run_status.map(status_label).unwrap_or("none")
                    ),
                    if selected {
                        style
                    } else {
                        Style::default().fg(color_text_dim())
                    },
                ),
            ]));
            ListItem::new(lines)
        })
        .collect()
}

fn current_status_text(app: &TuiApp) -> String {
    let Some(run) = current_run(app) else {
        return format!(
            "thread   {}\nrun      暂无\ndefault  {}\nmodel    {}\nfocus    {} / {}\ncomposer {}\nqueue    审批 {} · 产物 {}",
            selected_thread_title(app),
            configured_backend_summary(app),
            configured_model_summary(app),
            pane_label(app.browse_pane),
            if matches!(app.focus, FocusMode::Compose) {
                "编辑"
            } else {
                "浏览"
            },
            if app.composer.trim().is_empty() {
                "空"
            } else {
                "已保存"
            },
            app.approvals.len(),
            app.artifacts.len()
        );
    };

    let node = selected_node(app);
    let latest_event = latest_event_text(app);
    let progress = node_progress_text(app);
    let phase = current_phase_text(app);
    format!(
        "thread   {}\nrun      {} · {} · turns={}\ncurrent  {}\ndefault  {}\nmodel    {}\nphase    {}\nstep     {}\nprogress {}\nqueue    审批 {} · 产物 {} · 子代理 {}\nlatest   {}",
        selected_thread_title(app),
        status_label(run.status),
        format_run_duration(run),
        run.turn_count,
        current_run_backend_summary(run),
        configured_backend_summary(app),
        configured_model_summary(app),
        phase,
        node.map(|node| trim_line(node.title.as_str(), 36))
            .unwrap_or_else(|| "-".to_string()),
        progress,
        app.approvals.len(),
        app.artifacts.len(),
        app.subagents.len(),
        latest_event
    )
}

fn run_items(app: &TuiApp, width: u16) -> Vec<ListItem<'static>> {
    if app.runs.is_empty() {
        return vec![ListItem::new("当前 thread 还没有 run")];
    }

    app.runs
        .iter()
        .map(|run| {
            let selected = app.selected_run_id.as_ref().is_some_and(|id| id == &run.id);
            let style = if selected {
                selected_item_style()
            } else {
                Style::default().fg(color_text_main())
            };
            let prefix = if selected { "▸ " } else { "  " };
            let mut lines = wrap_lines(
                &short_id(&run.id, 28),
                inner_wrap_width(width, prefix.len()),
            )
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                Line::from(vec![
                    Span::styled(if index == 0 { prefix } else { "  " }, style),
                    Span::styled(line, style),
                ])
            })
            .collect::<Vec<_>>();
            lines.push(Line::from(format!(
                "  {} · {} · turns={}",
                status_label(run.status),
                format_run_duration(run),
                run.turn_count
            )));
            ListItem::new(lines)
        })
        .collect()
}

fn task_node_items(app: &TuiApp, width: u16) -> Vec<ListItem<'static>> {
    if app.task_nodes.is_empty() {
        return vec![ListItem::new("当前 run 没有 task nodes")];
    }

    app.task_nodes
        .iter()
        .map(|node| {
            let selected = app
                .selected_task_node_id
                .as_ref()
                .is_some_and(|id| id == &node.id);
            let base_style = if selected {
                selected_item_style()
            } else if matches!(node.status, TaskNodeStatus::Failed) {
                Style::default()
                    .fg(color_status_failed())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color_text_main())
            };
            let prefix = status_glyph(node.status, selected);
            let title = format!("{} [{}]", node.title, task_status_label(node.status));
            let mut lines = wrap_lines(&title, inner_wrap_width(width, prefix.len()))
                .into_iter()
                .enumerate()
                .map(|(index, line)| {
                    Line::from(vec![
                        Span::styled(if index == 0 { prefix } else { "  " }, base_style),
                        Span::styled(line, base_style),
                    ])
                })
                .collect::<Vec<_>>();
            lines.push(Line::from(format!(
                "  {} · attempts={} · agent={}",
                task_kind_label(node.kind),
                node.attempt_count,
                node.last_subagent_id.as_deref().unwrap_or("main")
            )));
            if matches!(node.status, TaskNodeStatus::Failed) {
                let error_text = node
                    .error
                    .as_deref()
                    .or(node.output_summary.as_deref())
                    .unwrap_or("节点执行失败");
                for line in wrap_lines(&format!("错误: {error_text}"), inner_wrap_width(width, 2))
                {
                    lines.push(Line::styled(
                        format!("  {line}"),
                        Style::default().fg(color_status_failed()),
                    ));
                }
            }
            ListItem::new(lines)
        })
        .collect()
}

fn live_stream_items(app: &TuiApp, width: u16) -> Vec<ListItem<'static>> {
    let mut items = Vec::new();

    if let Some(subagent) = active_subagent(app) {
        items.push(list_item_wrapped(
            &format!(
                "{} agent | {} | {}",
                subagent_kind_label(subagent.kind),
                status_label(subagent.status),
                preview_text(subagent.task.as_str())
            ),
            width,
        ));
        if let Some(summary) = subagent.summary.as_deref() {
            items.push(list_item_wrapped(&preview_text(summary), width));
        }
    }

    let event_items = app
        .events
        .iter()
        .rev()
        .take(8)
        .map(|event| {
            list_item_wrapped(
                &format!(
                    "{} {}",
                    event.at.format("%H:%M:%S"),
                    describe_event(&event.payload)
                ),
                width,
            )
        })
        .collect::<Vec<_>>();
    items.extend(event_items);

    if items.is_empty() {
        vec![ListItem::new("当前没有实时活动")]
    } else {
        items
    }
}

fn message_items(app: &TuiApp, width: u16) -> Vec<ListItem<'static>> {
    let items = app
        .messages
        .iter()
        .rev()
        .take(6)
        .map(|message| {
            list_item_wrapped(
                &format!(
                    "[{}] {}",
                    role_label(message.role),
                    message_preview(message)
                ),
                width,
            )
        })
        .collect::<Vec<_>>();

    if items.is_empty() {
        vec![ListItem::new("当前没有消息")]
    } else {
        items
    }
}

fn list_item_wrapped(text: &str, width: u16) -> ListItem<'static> {
    ListItem::new(
        wrap_lines(text, inner_wrap_width(width, 0))
            .into_iter()
            .map(|line| {
                Line::from(vec![Span::styled(
                    line,
                    Style::default().fg(color_text_dim()),
                )])
            })
            .collect::<Vec<_>>(),
    )
}

fn effective_primary_panel_scroll(app: &TuiApp, body: &str, area: Rect) -> u16 {
    if should_follow_live_output_tail(app) {
        return paragraph_max_scroll(body, area.width, area.height);
    }
    clamp_paragraph_scroll(body, area, app.live_output_scroll)
}

fn should_follow_live_output_tail(app: &TuiApp) -> bool {
    matches!(app.browse_pane, BrowsePane::Detail)
        && app.approvals.is_empty()
        && matches!(
            app.detail_parent_pane,
            BrowsePane::Runs | BrowsePane::Detail | BrowsePane::Composer
        )
        && app.live_output_follow_latest
}

pub(super) fn paragraph_max_scroll(text: &str, width: u16, height: u16) -> u16 {
    let visible_lines = height.saturating_sub(2);
    if visible_lines == 0 {
        return 0;
    }
    let total_lines = wrap_lines(text, inner_wrap_width(width, 0)).len() as u16;
    total_lines.saturating_sub(visible_lines)
}

fn clamp_paragraph_scroll(text: &str, area: Rect, requested: u16) -> u16 {
    requested.min(paragraph_max_scroll(text, area.width, area.height))
}

fn inner_wrap_width(total_width: u16, prefix_width: usize) -> usize {
    usize::from(total_width.saturating_sub(2))
        .saturating_sub(prefix_width)
        .max(8)
}

fn wrap_lines(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for raw_line in text.lines() {
        let mut current = String::new();
        let mut current_width = 0usize;
        for ch in raw_line.chars() {
            let ch_width = unicode_width::UnicodeWidthChar::width(ch)
                .unwrap_or(0)
                .max(1);
            if current_width + ch_width > max_width && !current.is_empty() {
                lines.push(current);
                current = String::new();
                current_width = 0;
            }
            current.push(ch);
            current_width += ch_width;
        }
        if current.is_empty() {
            lines.push(String::new());
        } else {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

fn message_preview(message: &HarnessMessage) -> String {
    let preview = preview_text(&message.content);
    let extra_lines = message.content.lines().skip(1).count();
    if extra_lines == 0 {
        preview
    } else {
        format!("{preview} …(+{extra_lines} 行)")
    }
}

fn preview_text(text: &str) -> String {
    trim_line(
        &first_line(text)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        96,
    )
}

fn chip(label: impl Into<String>, value: impl Into<String>, bg: Color) -> Span<'static> {
    Span::styled(
        format!(" {} {} ", label.into(), value.into()),
        Style::default()
            .fg(contrast_fg(bg))
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    )
}

fn keycap(value: impl Into<String>) -> Span<'static> {
    Span::styled(
        format!(" {} ", value.into()),
        Style::default()
            .fg(Color::White)
            .bg(color_keycap_bg())
            .add_modifier(Modifier::BOLD),
    )
}

fn pane_tabs_line(app: &TuiApp) -> Line<'static> {
    let panes = [
        (BrowsePane::Threads, "Threads"),
        (BrowsePane::Runs, "Runs"),
        (BrowsePane::Steps, "Steps"),
        (BrowsePane::Error, "Error"),
        (BrowsePane::Detail, "Detail"),
        (BrowsePane::Composer, "Composer"),
    ];
    let pane_count = panes.len();
    let mut spans = Vec::new();
    for (index, (pane, label)) in panes.into_iter().enumerate() {
        let active = app.browse_pane == pane;
        spans.push(Span::styled(
            format!(" {} {} ", index + 1, label),
            if active {
                Style::default()
                    .fg(Color::White)
                    .bg(color_tab_active_bg())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color_text_dim())
            },
        ));
        if index + 1 < pane_count {
            spans.push(Span::raw(" "));
        }
    }
    Line::from(spans)
}

fn composer_title(app: &TuiApp) -> String {
    let state = if matches!(app.focus, FocusMode::Compose) {
        "编辑中"
    } else if app.composer.trim().is_empty() {
        "空"
    } else {
        "已保存"
    };
    format!(
        "Composer · {state} · {} chars",
        app.composer.chars().count()
    )
}

fn render_composer<'a>(app: &'a TuiApp) -> Paragraph<'a> {
    if app.composer.trim().is_empty() && !matches!(app.focus, FocusMode::Compose) {
        return Paragraph::new(vec![
            Line::from(vec![Span::styled(
                format!("当前新 run 默认：{}", configured_backend_summary(app)),
                Style::default().fg(color_text_main()),
            )]),
            Line::from(vec![Span::styled(
                format!("模型设置：{}", configured_model_summary(app)),
                Style::default().fg(color_text_dim()),
            )]),
            Line::from(vec![Span::styled(
                "浏览模式按 m 可切换执行模式；直接打字即可开始，或按 Enter 进入编辑模式。",
                Style::default()
                    .fg(color_border())
                    .add_modifier(Modifier::ITALIC),
            )]),
            Line::from(vec![Span::styled(
                "草稿保存后，在浏览模式下按 Enter 直接发送。",
                Style::default().fg(color_text_dim()),
            )]),
        ]);
    }
    Paragraph::new(
        app.composer
            .lines()
            .map(|line| {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(color_text_main()),
                ))
            })
            .collect::<Vec<_>>(),
    )
}

fn selected_item_style() -> Style {
    Style::default()
        .fg(Color::White)
        .bg(color_selected_bg())
        .add_modifier(Modifier::BOLD)
}

fn contrast_fg(bg: Color) -> Color {
    match bg {
        Color::Yellow | Color::Green | Color::Cyan | Color::Gray | Color::White => Color::Black,
        Color::Rgb(92, 103, 117) | Color::Rgb(80, 92, 106) => Color::White,
        _ => Color::White,
    }
}

fn status_glyph(status: TaskNodeStatus, selected: bool) -> &'static str {
    match status {
        TaskNodeStatus::Pending => "○ ",
        TaskNodeStatus::Ready => "◇ ",
        TaskNodeStatus::Running => "▶ ",
        TaskNodeStatus::WaitingForInput => "◆ ",
        TaskNodeStatus::Completed => {
            if selected {
                "▸ "
            } else {
                "● "
            }
        }
        TaskNodeStatus::Failed => "✕ ",
        TaskNodeStatus::Skipped => "· ",
    }
}

fn short_id(value: &str, max_chars: usize) -> String {
    trim_line(value, max_chars)
}

fn color_brand_bg() -> Color {
    Color::Rgb(72, 86, 101)
}

fn color_chip_neutral() -> Color {
    Color::Rgb(92, 103, 117)
}

fn color_keycap_bg() -> Color {
    Color::Rgb(84, 92, 101)
}

fn color_tab_active_bg() -> Color {
    Color::Rgb(80, 92, 106)
}

fn color_selected_bg() -> Color {
    Color::Rgb(70, 78, 89)
}

fn color_accent() -> Color {
    Color::Rgb(120, 150, 170)
}

fn color_border() -> Color {
    Color::Rgb(102, 108, 116)
}

fn color_text_main() -> Color {
    Color::Rgb(216, 220, 224)
}

fn color_text_dim() -> Color {
    Color::Rgb(154, 160, 168)
}

fn color_status_pending() -> Color {
    Color::Rgb(122, 128, 136)
}

fn color_status_failed() -> Color {
    Color::Rgb(178, 107, 102)
}

fn pane_label(pane: BrowsePane) -> &'static str {
    match pane {
        BrowsePane::Threads => "Threads",
        BrowsePane::Runs => "Runs",
        BrowsePane::Steps => "执行步骤",
        BrowsePane::Error => "错误视图",
        BrowsePane::Detail => "详情视图",
        BrowsePane::Composer => "Composer",
    }
}

fn primary_hint(app: &TuiApp) -> String {
    match app.focus {
        FocusMode::Compose => "输入草稿 | Enter 保存 | Esc 返回".to_string(),
        FocusMode::Browse if !app.approvals.is_empty() => {
            "当前优先处理审批：上下/jk 切换，Enter/a 通过，Backspace/x 拒绝 | m 切换执行模式"
                .to_string()
        }
        FocusMode::Browse if current_plan_wait_text(app).is_some() => {
            "计划已生成：Enter 确认继续；也可以在 Composer 输入反馈后重新生成计划 | m 切换执行模式"
                .to_string()
        }
        FocusMode::Browse if current_blocked_text(app).is_some() => {
            "当前运行在等待输入：若无审批，可按 s 恢复执行 | m 切换执行模式".to_string()
        }
        FocusMode::Browse if matches!(app.browse_pane, BrowsePane::Error) => {
            "错误视图：集中查看失败原因、错误内容与恢复动作；用上下方向键滚动 | m 切换执行模式"
                .to_string()
        }
        FocusMode::Browse if matches!(app.browse_pane, BrowsePane::Detail) => {
            if matches!(app.detail_parent_pane, BrowsePane::Runs) {
                "实时输出：内容完整显示，单行过长自动换行；用上下方向键滚动 | m 切换执行模式"
                    .to_string()
            } else if matches!(app.detail_parent_pane, BrowsePane::Error) {
                "错误详情：单行过长自动换行；用上下方向键滚动 | m 切换执行模式".to_string()
            } else {
                "详情视图：单行过长自动换行；用上下方向键滚动 | m 切换执行模式".to_string()
            }
        }
        FocusMode::Browse if app.pending_delete_thread_id.is_some() => {
            "正在确认删除当前 thread：Enter 删除，Esc 取消 | m 切换执行模式".to_string()
        }
        FocusMode::Browse if app.pending_send.is_some() => {
            "任务正在后台运行，界面会自动刷新，你可以继续浏览 | m 切换执行模式".to_string()
        }
        FocusMode::Browse if !app.composer.trim().is_empty() => {
            "草稿已保存，按 Enter 直接运行；若想修改，再按 Enter 继续编辑 | m 切换执行模式"
                .to_string()
        }
        FocusMode::Browse => {
            "用 Tab 或左右切换面板，用上下移动；直接输入或按 Enter 开始 | m 切换执行模式"
                .to_string()
        }
    }
}

struct AlertBanner<'a> {
    title: &'a str,
    text: String,
    style: Style,
}

fn banner_style(fg: Color, bg: Color) -> Style {
    Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD)
}

fn current_alert(app: &TuiApp) -> AlertBanner<'static> {
    if let Some(plan_wait) = current_plan_wait_text(app) {
        return AlertBanner {
            title: "等待计划确认",
            text: format!(
                "当前计划待确认：{} | 按 Enter 继续执行，或在 Composer 输入反馈后重新生成计划",
                plan_wait
            ),
            style: banner_style(Color::White, Color::Blue),
        };
    }

    if let Some(recoverable) = current_recoverable_failure_text(app) {
        return AlertBanner {
            title: "工具失败，流程继续",
            text: format!(
                "最近可恢复失败：{} | 下一步：{}",
                recoverable,
                current_next_action_text(app)
            ),
            style: banner_style(Color::Black, Color::Yellow),
        };
    }

    if let Some(error) = current_error_text(app) {
        return AlertBanner {
            title: "Run Failed",
            text: format!(
                "运行失败或节点异常：{} | 先看红色步骤和右侧输出，再决定下一步",
                error
            ),
            style: banner_style(Color::White, Color::Red),
        };
    }

    if let Some(approval) = app.approvals.first() {
        return AlertBanner {
            title: "审批等待",
            text: format!(
                "有 {} 条待处理审批，当前工具={}。现在默认就是在处理审批，可用上下切换",
                app.approvals.len(),
                app.selected_approval()
                    .map(|item| item.tool_name.as_str())
                    .unwrap_or(approval.tool_name.as_str())
            ),
            style: banner_style(Color::Black, Color::Yellow),
        };
    }

    if let Some(cancelled) = current_cancelled_text(app) {
        return AlertBanner {
            title: "已取消",
            text: format!(
                "当前 run 已取消：{} | 可切换其他 run 或重新发送消息",
                cancelled
            ),
            style: banner_style(Color::White, Color::DarkGray),
        };
    }

    if let Some(blocked) = current_blocked_text(app) {
        let title = if blocked.contains("审批") {
            "等待审批"
        } else if blocked.contains("人工") {
            "等待人工输入"
        } else {
            "阻塞"
        };
        return AlertBanner {
            title,
            text: format!(
                "当前运行被阻塞：{} | 下一步：{}",
                blocked,
                current_next_action_text(app)
            ),
            style: banner_style(Color::White, Color::Blue),
        };
    }

    AlertBanner {
        title: "状态",
        text: format!(
            "当前阶段={} | agent={} | 下一步={}",
            current_phase_text(app),
            current_agent_name(app),
            current_next_action_text(app)
        ),
        style: banner_style(Color::White, Color::Green),
    }
}

fn footer_text(app: &TuiApp) -> String {
    if !app.status.trim().is_empty() {
        return app.status.clone();
    }
    match app.focus {
        FocusMode::Compose => "正在编辑草稿。Enter 保存，Esc 返回。".to_string(),
        FocusMode::Browse => primary_hint(app),
    }
}

fn render_footer<'a>(app: &'a TuiApp) -> Paragraph<'a> {
    let approval_hint = if app.approvals.is_empty() {
        "  "
    } else {
        "  a/Enter 通过  x/Backspace 拒绝  "
    };
    let retry_hint = if current_error_text(app).is_some() {
        Some(vec![keycap("t"), Span::raw(" 重试失败步骤  ")])
    } else {
        None
    };
    let plan_hint = if current_plan_wait_text(app).is_some() {
        Some(vec![keycap("Enter"), Span::raw(" 确认计划  ")])
    } else {
        None
    };
    let mut first_line = vec![
        keycap("Tab"),
        Span::raw(" 切栏  "),
        keycap("↑↓"),
        Span::raw(" 导航  "),
        Span::raw(approval_hint),
    ];
    if let Some(items) = plan_hint {
        first_line.extend(items);
    }
    if let Some(items) = retry_hint {
        first_line.extend(items);
    }
    first_line.extend([
        keycap("Enter"),
        Span::raw(" 打开/发送  "),
        keycap("i"),
        Span::raw(" 编辑  "),
        keycap("n"),
        Span::raw(" 新线程  "),
        keycap("r"),
        Span::raw(" 刷新  "),
        keycap("m"),
        Span::raw(" 切模式  "),
        keycap("q"),
        Span::raw(" 退出"),
    ]);
    Paragraph::new(vec![
        Line::from(first_line),
        Line::from(vec![
            Span::styled(
                "状态 ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(footer_text(app), Style::default().fg(Color::Gray)),
        ]),
    ])
}

fn approval_card_text(app: &TuiApp) -> String {
    let Some(approval) = app.selected_approval() else {
        return "当前没有待审批项".to_string();
    };
    let node = approval.task_node_id.as_deref().unwrap_or("-");
    let args = trim_line(&approval.tool_call.arguments.to_string(), 160);
    format!(
        "待处理审批\n\n工具：{}\n节点：{}\n原因：{}\n参数：{}\n\n上/下 或 j/k：切换审批\nEnter 或 a：通过当前审批\nBackspace/Delete 或 x：拒绝当前审批",
        approval.tool_name, node, approval.reason, args
    )
}

fn primary_panel_content(app: &TuiApp) -> (String, String) {
    if !app.approvals.is_empty() {
        return (
            format!(
                "审批卡片 · {}/{}",
                app.selected_approval_index + 1,
                app.approvals.len()
            ),
            approval_card_text(app),
        );
    }

    if current_plan_wait_text(app).is_some()
        && !matches!(app.browse_pane, BrowsePane::Error | BrowsePane::Detail)
    {
        return ("计划确认".to_string(), plan_confirmation_card_text(app));
    }

    match app.browse_pane {
        BrowsePane::Error => ("错误详情".to_string(), error_detail_text(app)),
        BrowsePane::Detail => match app.detail_parent_pane {
            BrowsePane::Threads => (
                format!(
                    "Thread 详情 · {}",
                    trim_line(&selected_thread_title(app), 28)
                ),
                thread_detail_text(app),
            ),
            BrowsePane::Steps => ("步骤详情".to_string(), step_detail_text(app)),
            BrowsePane::Error => ("错误详情".to_string(), error_detail_text(app)),
            BrowsePane::Runs | BrowsePane::Detail | BrowsePane::Composer => {
                ("运行概览".to_string(), run_decision_summary_text(app))
            }
        },
        _ => (app.live_output_title.clone(), app.live_output_body.clone()),
    }
}

fn thread_detail_text(app: &TuiApp) -> String {
    let messages = app
        .messages
        .iter()
        .rev()
        .take(12)
        .rev()
        .map(|message| format!("[{}] {}", role_label(message.role), message.content))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "线程：{}\n消息数：{}\n运行数：{}\n审批：{} | 产物：{}\n\n最近消息\n----------------\n{}",
        selected_thread_title(app),
        app.messages.len(),
        app.runs.len(),
        app.approvals.len(),
        app.artifacts.len(),
        if messages.trim().is_empty() {
            "暂无消息".to_string()
        } else {
            messages
        }
    )
}

fn step_detail_text(app: &TuiApp) -> String {
    let Some(node) = selected_node(app) else {
        return "当前没有选中步骤".to_string();
    };
    let summary = node.output_summary.as_deref().unwrap_or("暂无摘要");
    let plan_action = if current_plan_wait_text(app).is_some() {
        "\n\n执行入口\n----------------\n按 Enter 继续执行当前计划；如果要修改计划，在 Composer 输入反馈后重新提交。"
    } else {
        ""
    };
    let error_section = node.error.as_deref().map_or_else(String::new, |error| {
        format!("\n\n错误\n----------------\n{error}")
    });
    let retry_hint = if matches!(node.status, TaskNodeStatus::Failed) {
        "\n\n手动操作\n----------------\n可按 t 手动重试这个失败步骤。"
    } else {
        ""
    };
    format!(
        "标题：{}\n状态：{}\n类型：{}\n尝试次数：{}\nagent：{}\n\n说明\n----------------\n{}\n\n摘要\n----------------\n{}{}{}{}",
        node.title,
        task_status_label(node.status),
        task_kind_label(node.kind),
        node.attempt_count,
        node.last_subagent_id.as_deref().unwrap_or("main"),
        node.instructions,
        summary,
        plan_action,
        error_section,
        retry_hint
    )
}

fn plan_confirmation_card_text(app: &TuiApp) -> String {
    let summary = app
        .current_contract
        .as_ref()
        .map(|contract| trim_line(contract.goal.as_str(), 96))
        .unwrap_or_else(|| "当前计划已完成检查".to_string());
    let next_step = app
        .current_progress
        .as_ref()
        .and_then(|progress| progress.next_step.as_deref())
        .map(|text| trim_line(text, 96))
        .unwrap_or_else(|| "选择下一个 feature".to_string());
    format!(
        "计划已生成，等待你确认。\n\n目标：{}\n下一步：{}\n\n操作\n----------------\n[Enter] 继续执行\n[i] 进入 Composer 编辑反馈\n[m] 仅切换后续新 run 的执行模式",
        summary, next_step
    )
}

fn error_detail_text(app: &TuiApp) -> String {
    let Some(error) = current_error_text(app) else {
        return "当前没有错误。\n\n如果后续 run 或步骤失败，这里会集中显示失败对象、原因、原始错误与恢复动作。"
            .to_string();
    };
    let failure_scope = selected_node(app)
        .map(|node| format!("步骤：{} ({})", node.title, task_status_label(node.status)))
        .or_else(|| {
            current_run(app).map(|run| {
                format!(
                    "Run：{} ({})",
                    short_id(&run.id, 24),
                    status_label(run.status)
                )
            })
        })
        .unwrap_or_else(|| "当前失败对象未知".to_string());
    let why = selected_node(app)
        .and_then(|node| node.output_summary.clone())
        .or_else(|| current_run(app).and_then(|run| run.summary.clone()))
        .unwrap_or_else(|| "没有记录到额外上下文，只拿到了错误文本".to_string());
    let related = latest_failure_event_text(app).unwrap_or_else(|| "暂无相关失败事件".to_string());
    let action = if current_plan_wait_text(app).is_some() {
        "下一步：先处理计划确认。按 Enter 确认继续，或在 Composer 输入反馈重新生成计划。"
            .to_string()
    } else if selected_node(app).is_some_and(|node| matches!(node.status, TaskNodeStatus::Failed)) {
        "下一步：先看错误原因与最近事件；确认后按 t 重试当前失败步骤。".to_string()
    } else {
        "下一步：先看错误原因与最近事件，再决定是否重新发送任务或切换 run。".to_string()
    };
    format!(
        "失败对象\n----------------\n{}\n\n为什么会失败\n----------------\n{}\n\n错误内容\n----------------\n{}\n\n最近相关事件\n----------------\n{}\n\n恢复动作\n----------------\n{}",
        failure_scope, why, error, related, action
    )
}

fn run_decision_summary_text(app: &TuiApp) -> String {
    let Some(run) = current_run(app) else {
        return "当前没有 run".to_string();
    };
    let decisions = app
        .current_progress
        .as_ref()
        .map(|progress| {
            if progress.decisions.is_empty() {
                "无".to_string()
            } else {
                progress
                    .decisions
                    .iter()
                    .rev()
                    .take(5)
                    .rev()
                    .map(|item| format!("- {item}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        })
        .unwrap_or_else(|| "无".to_string());
    format!(
        "阶段：{}\n状态：{}\n当前 agent：{}\n当前节点：{}\n下一步：{}\n最近可恢复失败：{}\n阻塞原因：{}\n\n最近决策\n----------------\n{}\n\n实时输出\n----------------\n{}",
        current_phase_text(app),
        status_label(run.status),
        current_agent_name(app),
        selected_node(app)
            .map(|node| node.title.as_str())
            .unwrap_or("-"),
        current_next_action_text(app),
        current_recoverable_failure_text(app).unwrap_or_else(|| "-".to_string()),
        current_blocked_text(app).unwrap_or_else(|| "-".to_string()),
        decisions,
        build_live_output_detail(current_run(app), &app.subagents).1
    )
}

fn current_run(app: &TuiApp) -> Option<&HarnessRunManifest> {
    app.selected_run_id
        .as_ref()
        .and_then(|id| app.runs.iter().find(|run| &run.id == id))
        .or_else(|| app.runs.first())
}

fn selected_node(app: &TuiApp) -> Option<&TaskNodeRecord> {
    app.selected_task_node_id
        .as_ref()
        .and_then(|id| app.task_nodes.iter().find(|node| &node.id == id))
        .or_else(|| {
            current_run(app)
                .and_then(|run| run.active_task_node_id.as_ref())
                .and_then(|id| app.task_nodes.iter().find(|node| &node.id == id))
        })
}

fn active_subagent(app: &TuiApp) -> Option<&SubagentRecord> {
    app.subagents
        .iter()
        .filter(|subagent| {
            matches!(
                subagent.status,
                crate::harness::HarnessRunStatus::Running
                    | crate::harness::HarnessRunStatus::WaitingForInput
            )
        })
        .max_by_key(|subagent| subagent.updated_at)
        .or_else(|| {
            app.subagents
                .iter()
                .max_by_key(|subagent| subagent.updated_at)
        })
}

fn current_agent_name(app: &TuiApp) -> String {
    active_subagent(app)
        .map(|subagent| format!("{} agent", subagent_kind_label(subagent.kind)))
        .unwrap_or_else(|| "主代理".to_string())
}

fn current_backend_name(app: &TuiApp) -> &'static str {
    app.config.backend.provider.display_name()
}

fn current_run_backend_summary(run: &HarnessRunManifest) -> String {
    format!(
        "当前 run {} · {}",
        agent_backend_display_name(run.backend),
        run.execution_kind.display_name()
    )
}

fn configured_backend_summary(app: &TuiApp) -> String {
    format!(
        "新 run 默认 {}{} · 按 m 切到 {}",
        current_backend_name(app),
        if matches!(
            app.config.backend.provider,
            crate::config::BackendProvider::Codex
        ) {
            "（默认）"
        } else {
            ""
        },
        app.config.backend.provider.next().display_name()
    )
}

fn configured_model_summary(app: &TuiApp) -> String {
    let configured_model = app
        .config
        .backend
        .model
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    match app.config.backend.provider {
        crate::config::BackendProvider::Codex => "Codex CLI 内置模型".to_string(),
        crate::config::BackendProvider::OpenAiCompatible => format!(
            "OpenAI 兼容接口 · model={}",
            trim_line(configured_model.unwrap_or("未配置"), 24)
        ),
    }
}

fn agent_backend_display_name(backend: crate::harness::types::AgentBackendKind) -> &'static str {
    match backend {
        crate::harness::types::AgentBackendKind::Codex => "Codex",
        crate::harness::types::AgentBackendKind::OpenAiCompatible => "OpenAI Compatible",
    }
}

fn current_activity(app: &TuiApp) -> String {
    if app.pending_delete_thread_id.is_some() {
        return "等待删除确认".to_string();
    }
    if !app.approvals.is_empty() {
        return "等待审批确认".to_string();
    }
    if matches!(app.focus, FocusMode::Compose) {
        return "正在编辑草稿".to_string();
    }
    if let Some(subagent) = active_subagent(app) {
        if let Some(summary) = subagent.summary.as_deref() {
            return trim_line(summary, 60);
        }
        return trim_line(subagent.task.as_str(), 60);
    }
    if let Some(node) = selected_node(app) {
        if let Some(summary) = node.output_summary.as_deref() {
            return trim_line(summary, 60);
        }
        return trim_line(node.title.as_str(), 60);
    }
    if current_run(app).is_some_and(|run| run.execution_kind.is_autonomous_codex()) {
        return current_run(app)
            .and_then(|run| run.summary.as_deref())
            .map(|summary| trim_line(summary, 60))
            .unwrap_or_else(|| "Codex 正在自主执行".to_string());
    }
    latest_event_text(app)
}

fn latest_event_text(app: &TuiApp) -> String {
    app.events
        .last()
        .map(|event| trim_line(&describe_event(&event.payload), 60))
        .unwrap_or_else(|| "暂无事件".to_string())
}

fn current_error_text(app: &TuiApp) -> Option<String> {
    selected_node(app)
        .and_then(|node| node.error.as_deref())
        .map(|text| trim_line(text, 96))
        .or_else(|| {
            current_run(app)
                .and_then(|run| run.last_error.as_deref())
                .map(|text| trim_line(text, 96))
        })
        .or_else(|| {
            current_run(app).and_then(|run| {
                matches!(run.status, crate::harness::HarnessRunStatus::Failed)
                    .then(|| trim_line(run.summary.as_deref().unwrap_or("run 已失败"), 96))
            })
        })
}

fn current_cancelled_text(app: &TuiApp) -> Option<String> {
    current_run(app)
        .filter(|run| matches!(run.status, crate::harness::HarnessRunStatus::Cancelled))
        .map(|run| trim_line(run.summary.as_deref().unwrap_or("当前 run 已取消"), 96))
}

fn current_blocked_text(app: &TuiApp) -> Option<String> {
    app.current_progress
        .as_ref()
        .and_then(|progress| progress.blocking_reason.as_deref())
        .filter(|text| !text.trim().is_empty())
        .map(|text| trim_line(text, 96))
        .or_else(|| {
            current_run(app)
                .filter(|run| {
                    matches!(
                        run.status,
                        crate::harness::HarnessRunStatus::WaitingForInput
                    )
                })
                .and_then(|run| run.blocked_reason.as_deref())
                .filter(|text| !text.trim().is_empty())
                .map(|text| trim_line(text, 96))
        })
}

fn current_recoverable_failure_text(app: &TuiApp) -> Option<String> {
    app.current_progress
        .as_ref()
        .and_then(|progress| progress.latest_recoverable_failure.as_deref())
        .filter(|text| !text.trim().is_empty())
        .map(|text| trim_line(text, 96))
}

fn current_phase_text(app: &TuiApp) -> String {
    if current_run(app).is_some_and(|run| run.execution_kind.is_autonomous_codex()) {
        return "自主执行".to_string();
    }
    app.current_progress
        .as_ref()
        .and_then(|progress| progress.current_phase.clone())
        .unwrap_or_else(|| infer_phase_from_node(app).to_string())
}

fn current_next_action_text(app: &TuiApp) -> String {
    if current_run(app).is_some_and(|run| run.execution_kind.is_autonomous_codex()) {
        return current_run(app)
            .and_then(|run| run.blocked_reason.clone())
            .unwrap_or_else(|| "等待 Codex 完成当前任务".to_string());
    }
    app.current_progress
        .as_ref()
        .and_then(|progress| progress.next_step.clone())
        .or_else(|| {
            selected_node(app).map(|node| match node.kind {
                TaskNodeKind::Plan
                | TaskNodeKind::BuildExecutionContract
                | TaskNodeKind::PlanReview
                | TaskNodeKind::Explore => "继续补齐计划或事实".to_string(),
                TaskNodeKind::SelectNextFeature | TaskNodeKind::ExecuteFeature => {
                    "继续推进当前 feature".to_string()
                }
                TaskNodeKind::EvaluateFeature => "按 done_when 评估当前 feature".to_string(),
                TaskNodeKind::CheckpointProgress | TaskNodeKind::FinalizeDelivery => {
                    "生成 checkpoint 或最终交付".to_string()
                }
                _ => "继续当前步骤".to_string(),
            })
        })
        .unwrap_or_else(|| "-".to_string())
}

fn current_plan_wait_text(app: &TuiApp) -> Option<String> {
    let run = current_run(app)?;
    let node = selected_node(app)?;
    if current_phase_text(app) != "计划"
        || !matches!(
            run.status,
            crate::harness::HarnessRunStatus::WaitingForInput
        )
        || !matches!(node.kind, TaskNodeKind::PlanReview)
        || !matches!(node.status, TaskNodeStatus::WaitingForInput)
    {
        return None;
    }
    Some(trim_line(
        app.current_progress
            .as_ref()
            .and_then(|progress| progress.next_step.clone())
            .unwrap_or_else(|| node.title.clone())
            .as_str(),
        96,
    ))
}

fn latest_failure_event_text(app: &TuiApp) -> Option<String> {
    app.events
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            crate::harness::HarnessEvent::TaskNodeFailed { .. }
            | crate::harness::HarnessEvent::RunFailed { .. }
            | crate::harness::HarnessEvent::EvidenceInsufficient { .. } => Some(format!(
                "{} {}",
                event.at.format("%H:%M:%S"),
                describe_event(&event.payload)
            )),
            _ => None,
        })
}

fn infer_phase_from_node(app: &TuiApp) -> &'static str {
    if current_run(app).is_some_and(|run| run.execution_kind.is_autonomous_codex()) {
        return "自主执行";
    }
    match selected_node(app).map(|node| node.kind) {
        Some(
            TaskNodeKind::Plan
            | TaskNodeKind::BuildExecutionContract
            | TaskNodeKind::PlanReview
            | TaskNodeKind::Explore,
        ) => "计划",
        Some(TaskNodeKind::SelectNextFeature | TaskNodeKind::ExecuteFeature) => "执行",
        Some(TaskNodeKind::EvaluateFeature) => "评估",
        Some(TaskNodeKind::CheckpointProgress | TaskNodeKind::FinalizeDelivery) => "交付",
        _ => "-",
    }
}

fn node_progress_text(app: &TuiApp) -> String {
    if current_run(app).is_some_and(|run| run.execution_kind.is_autonomous_codex()) {
        return "自主执行".to_string();
    }
    if app.task_nodes.is_empty() {
        return "0/0".to_string();
    }
    let total = app.task_nodes.len();
    let completed = app
        .task_nodes
        .iter()
        .filter(|node| {
            matches!(
                node.status,
                TaskNodeStatus::Completed | TaskNodeStatus::Skipped
            )
        })
        .count();
    let running = app
        .task_nodes
        .iter()
        .filter(|node| matches!(node.status, TaskNodeStatus::Running))
        .count();
    let failed = app
        .task_nodes
        .iter()
        .filter(|node| matches!(node.status, TaskNodeStatus::Failed))
        .count();
    format!("{completed}/{total} 完成 | running={running} | failed={failed}")
}

fn selected_thread_title(app: &TuiApp) -> String {
    app.selected_thread_id
        .as_ref()
        .and_then(|id| app.threads.iter().find(|thread| &thread.id == id))
        .map(|thread| thread.title.clone())
        .unwrap_or_else(|| "-".to_string())
}

fn format_run_duration(run: &HarnessRunManifest) -> String {
    let end = if matches!(
        run.status,
        crate::harness::HarnessRunStatus::Completed
            | crate::harness::HarnessRunStatus::Failed
            | crate::harness::HarnessRunStatus::Cancelled
    ) {
        run.updated_at
    } else {
        Utc::now()
    };
    format_duration(run.created_at, end)
}

fn format_duration(start: chrono::DateTime<Utc>, end: chrono::DateTime<Utc>) -> String {
    let seconds = (end - start).num_seconds().max(0);
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m {secs:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {secs:02}s")
    } else {
        format!("{secs}s")
    }
}

fn trim_line(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let shortened = trimmed
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    format!("{shortened}…")
}

fn task_status_label(status: TaskNodeStatus) -> &'static str {
    match status {
        TaskNodeStatus::Pending => "pending",
        TaskNodeStatus::Ready => "ready",
        TaskNodeStatus::Running => "running",
        TaskNodeStatus::WaitingForInput => "waiting",
        TaskNodeStatus::Completed => "completed",
        TaskNodeStatus::Failed => "failed",
        TaskNodeStatus::Skipped => "skipped",
    }
}

fn task_kind_label(kind: TaskNodeKind) -> &'static str {
    match kind {
        TaskNodeKind::Plan => "plan",
        TaskNodeKind::Initialize => "initialize",
        TaskNodeKind::BuildExecutionContract => "contract",
        TaskNodeKind::PlanReview => "plan_review",
        TaskNodeKind::SelectNextFeature => "select_feature",
        TaskNodeKind::ExecuteFeature => "execute",
        TaskNodeKind::EvaluateFeature => "evaluate",
        TaskNodeKind::CheckpointProgress => "checkpoint",
        TaskNodeKind::FinalizeDelivery => "finalize",
        TaskNodeKind::Explore => "explore",
        TaskNodeKind::Implement => "implement",
        TaskNodeKind::Review => "review",
        TaskNodeKind::Test => "test",
        TaskNodeKind::Summarize => "summarize",
        TaskNodeKind::ApprovalGate => "approval",
    }
}

fn subagent_kind_label(kind: SubagentKind) -> &'static str {
    match kind {
        SubagentKind::Planner => "planner",
        SubagentKind::Generator => "builder",
        SubagentKind::Evaluator => "reviewer",
    }
}

fn run_status_color(status: crate::harness::HarnessRunStatus) -> Color {
    match status {
        crate::harness::HarnessRunStatus::Pending => Color::DarkGray,
        crate::harness::HarnessRunStatus::Running => Color::Cyan,
        crate::harness::HarnessRunStatus::WaitingForInput => Color::Yellow,
        crate::harness::HarnessRunStatus::Completed => Color::Green,
        crate::harness::HarnessRunStatus::Failed => Color::Red,
        crate::harness::HarnessRunStatus::Cancelled => Color::Gray,
    }
}

fn composer_cursor_position(area: Rect, content: &str) -> Position {
    let inner_width = area.width.saturating_sub(2);
    let inner_height = area.height.saturating_sub(2);
    if inner_width == 0 || inner_height == 0 {
        return Position::new(area.x, area.y);
    }

    let content_width = content.width();
    let row = (content_width / usize::from(inner_width))
        .min(usize::from(inner_height.saturating_sub(1))) as u16;
    let col = (content_width % usize::from(inner_width))
        .min(usize::from(inner_width.saturating_sub(1))) as u16;
    Position::new(area.x + 1 + col, area.y + 1 + row)
}

#[cfg(test)]
mod tests {
    use super::{
        composer_cursor_position, configured_model_summary, current_alert, current_backend_name,
        current_blocked_text, footer_text, format_duration, plan_confirmation_card_text,
        primary_hint, primary_panel_content,
    };
    use crate::config::{AppConfig, BackendProvider};
    use crate::harness::HarnessStore;
    use crate::harness::types::{
        AgentBackendKind, ExecutionContract, HarnessRunManifest, HarnessRunStatus, ProgressLedger,
        RunExecutionKind, TaskNodeKind, TaskNodeRecord, TaskNodeStatus,
    };
    use crate::model::ThinkingMode;
    use chrono::{Duration, Utc};
    use ratatui::layout::{Position, Rect};
    use ratatui::style::Color;
    use std::time::Instant;
    use tempfile::TempDir;

    use crate::tui::app::TuiApp;
    use crate::tui::tabs::{BrowsePane, FocusMode};

    #[test]
    fn composer_cursor_starts_at_inner_origin() {
        assert_eq!(
            composer_cursor_position(Rect::new(10, 5, 20, 5), ""),
            Position::new(11, 6)
        );
    }

    #[test]
    fn composer_cursor_wraps_to_next_line() {
        assert_eq!(
            composer_cursor_position(Rect::new(0, 0, 6, 5), "abcd"),
            Position::new(1, 2)
        );
    }

    #[test]
    fn duration_formats_hours_minutes_and_seconds() {
        let start = Utc::now();
        let end = start + Duration::seconds(3665);
        assert_eq!(format_duration(start, end), "1h 01m 05s");
    }

    #[test]
    fn cancelled_run_is_not_treated_as_blocked() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().to_path_buf();
        let store = HarnessStore::new(&repo_root, BackendProvider::Codex);
        let run_dir = repo_root.join("run");
        let run = HarnessRunManifest {
            id: "run-1".to_string(),
            thread_id: "thread-1".to_string(),
            status: HarnessRunStatus::Cancelled,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            model: None,
            thinking_mode: ThinkingMode::Balanced,
            backend: AgentBackendKind::Codex,
            execution_kind: RunExecutionKind::AutonomousCodex,
            turn_count: 1,
            summary: Some("当前 run 已取消".to_string()),
            last_error: None,
            blocked_reason: Some("用户取消当前 run".to_string()),
            run_dir: run_dir.clone(),
            events_path: run_dir.join("events.jsonl"),
            output_path: run_dir.join("assistant.md"),
            log_path: run_dir.join("codex.log"),
            tool_calls_path: run_dir.join("tool-calls.jsonl"),
            approvals_path: run_dir.join("approvals.jsonl"),
            artifacts_path: run_dir.join("artifacts.jsonl"),
            subagents_path: run_dir.join("subagents.jsonl"),
            task_graph_path: run_dir.join("task-graph.json"),
            task_nodes_path: run_dir.join("task-nodes.jsonl"),
            evaluation_log_path: run_dir.join("evaluations.jsonl"),
            bootstrap_path: run_dir.join("session-bootstrap.md"),
            active_task_node_id: None,
            sandbox: None,
        };
        let app = TuiApp {
            repo_root,
            store,
            config: AppConfig::default(),
            threads: Vec::new(),
            selected_thread_id: Some("thread-1".to_string()),
            selected_run_id: Some("run-1".to_string()),
            selected_task_node_id: None,
            messages: Vec::new(),
            runs: vec![run],
            task_nodes: Vec::new(),
            events: Vec::new(),
            approvals: Vec::new(),
            selected_approval_index: 0,
            artifacts: Vec::new(),
            subagents: Vec::new(),
            current_contract: None,
            current_progress: None,
            working_memory: Vec::new(),
            project_memory: Vec::new(),
            focus: FocusMode::Browse,
            browse_pane: BrowsePane::Runs,
            detail_parent_pane: BrowsePane::Runs,
            composer: String::new(),
            pending_send: None,
            pending_delete_thread_id: None,
            last_refresh_at: Instant::now(),
            live_output_title: "实时输出".to_string(),
            live_output_body: String::new(),
            live_output_scroll: 0,
            live_output_follow_latest: true,
            detail_viewport_width: 0,
            detail_viewport_height: 0,
            status: String::new(),
        };

        assert_eq!(current_blocked_text(&app), None);
    }

    #[test]
    fn error_alert_uses_white_text_on_red_background() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().to_path_buf();
        let store = HarnessStore::new(&repo_root, BackendProvider::Codex);
        let run_dir = repo_root.join("run");
        let run = HarnessRunManifest {
            id: "run-1".to_string(),
            thread_id: "thread-1".to_string(),
            status: HarnessRunStatus::Failed,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            model: None,
            thinking_mode: ThinkingMode::Balanced,
            backend: AgentBackendKind::Codex,
            execution_kind: RunExecutionKind::AutonomousCodex,
            turn_count: 1,
            summary: Some("run 已失败".to_string()),
            last_error: Some("boom".to_string()),
            blocked_reason: None,
            run_dir: run_dir.clone(),
            events_path: run_dir.join("events.jsonl"),
            output_path: run_dir.join("assistant.md"),
            log_path: run_dir.join("codex.log"),
            tool_calls_path: run_dir.join("tool-calls.jsonl"),
            approvals_path: run_dir.join("approvals.jsonl"),
            artifacts_path: run_dir.join("artifacts.jsonl"),
            subagents_path: run_dir.join("subagents.jsonl"),
            task_graph_path: run_dir.join("task-graph.json"),
            task_nodes_path: run_dir.join("task-nodes.jsonl"),
            evaluation_log_path: run_dir.join("evaluations.jsonl"),
            bootstrap_path: run_dir.join("session-bootstrap.md"),
            active_task_node_id: None,
            sandbox: None,
        };
        let app = TuiApp {
            repo_root,
            store,
            config: AppConfig::default(),
            threads: Vec::new(),
            selected_thread_id: Some("thread-1".to_string()),
            selected_run_id: Some("run-1".to_string()),
            selected_task_node_id: None,
            messages: Vec::new(),
            runs: vec![run],
            task_nodes: Vec::new(),
            events: Vec::new(),
            approvals: Vec::new(),
            selected_approval_index: 0,
            artifacts: Vec::new(),
            subagents: Vec::new(),
            current_contract: None,
            current_progress: None,
            working_memory: Vec::new(),
            project_memory: Vec::new(),
            focus: FocusMode::Browse,
            browse_pane: BrowsePane::Runs,
            detail_parent_pane: BrowsePane::Runs,
            composer: String::new(),
            pending_send: None,
            pending_delete_thread_id: None,
            last_refresh_at: Instant::now(),
            live_output_title: "实时输出".to_string(),
            live_output_body: String::new(),
            live_output_scroll: 0,
            live_output_follow_latest: true,
            detail_viewport_width: 0,
            detail_viewport_height: 0,
            status: String::new(),
        };

        let alert = current_alert(&app);
        assert_eq!(alert.title, "Run Failed");
        assert_eq!(alert.style.fg, Some(Color::White));
        assert_eq!(alert.style.bg, Some(Color::Red));
    }

    #[test]
    fn cancelled_alert_uses_white_text_on_dark_background() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().to_path_buf();
        let store = HarnessStore::new(&repo_root, BackendProvider::Codex);
        let run_dir = repo_root.join("run");
        let run = HarnessRunManifest {
            id: "run-1".to_string(),
            thread_id: "thread-1".to_string(),
            status: HarnessRunStatus::Cancelled,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            model: None,
            thinking_mode: ThinkingMode::Balanced,
            backend: AgentBackendKind::Codex,
            execution_kind: RunExecutionKind::AutonomousCodex,
            turn_count: 1,
            summary: Some("当前 run 已取消".to_string()),
            last_error: None,
            blocked_reason: None,
            run_dir: run_dir.clone(),
            events_path: run_dir.join("events.jsonl"),
            output_path: run_dir.join("assistant.md"),
            log_path: run_dir.join("codex.log"),
            tool_calls_path: run_dir.join("tool-calls.jsonl"),
            approvals_path: run_dir.join("approvals.jsonl"),
            artifacts_path: run_dir.join("artifacts.jsonl"),
            subagents_path: run_dir.join("subagents.jsonl"),
            task_graph_path: run_dir.join("task-graph.json"),
            task_nodes_path: run_dir.join("task-nodes.jsonl"),
            evaluation_log_path: run_dir.join("evaluations.jsonl"),
            bootstrap_path: run_dir.join("session-bootstrap.md"),
            active_task_node_id: None,
            sandbox: None,
        };
        let app = TuiApp {
            repo_root,
            store,
            config: AppConfig::default(),
            threads: Vec::new(),
            selected_thread_id: Some("thread-1".to_string()),
            selected_run_id: Some("run-1".to_string()),
            selected_task_node_id: None,
            messages: Vec::new(),
            runs: vec![run],
            task_nodes: Vec::new(),
            events: Vec::new(),
            approvals: Vec::new(),
            selected_approval_index: 0,
            artifacts: Vec::new(),
            subagents: Vec::new(),
            current_contract: None,
            current_progress: None,
            working_memory: Vec::new(),
            project_memory: Vec::new(),
            focus: FocusMode::Browse,
            browse_pane: BrowsePane::Runs,
            detail_parent_pane: BrowsePane::Runs,
            composer: String::new(),
            pending_send: None,
            pending_delete_thread_id: None,
            last_refresh_at: Instant::now(),
            live_output_title: "实时输出".to_string(),
            live_output_body: String::new(),
            live_output_scroll: 0,
            live_output_follow_latest: true,
            detail_viewport_width: 0,
            detail_viewport_height: 0,
            status: String::new(),
        };

        let alert = current_alert(&app);
        assert_eq!(alert.title, "已取消");
        assert_eq!(alert.style.fg, Some(Color::White));
        assert_eq!(alert.style.bg, Some(Color::DarkGray));
    }

    #[test]
    fn default_status_alert_uses_white_text_on_green_background() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().to_path_buf();
        let store = HarnessStore::new(&repo_root, BackendProvider::Codex);
        let app = TuiApp {
            repo_root,
            store,
            config: AppConfig::default(),
            threads: Vec::new(),
            selected_thread_id: None,
            selected_run_id: None,
            selected_task_node_id: None,
            messages: Vec::new(),
            runs: Vec::new(),
            task_nodes: Vec::new(),
            events: Vec::new(),
            approvals: Vec::new(),
            selected_approval_index: 0,
            artifacts: Vec::new(),
            subagents: Vec::new(),
            current_contract: None,
            current_progress: None,
            working_memory: Vec::new(),
            project_memory: Vec::new(),
            focus: FocusMode::Browse,
            browse_pane: BrowsePane::Threads,
            detail_parent_pane: BrowsePane::Runs,
            composer: String::new(),
            pending_send: None,
            pending_delete_thread_id: None,
            last_refresh_at: Instant::now(),
            live_output_title: "实时输出".to_string(),
            live_output_body: String::new(),
            live_output_scroll: 0,
            live_output_follow_latest: true,
            detail_viewport_width: 0,
            detail_viewport_height: 0,
            status: String::new(),
        };

        let alert = current_alert(&app);
        assert_eq!(alert.title, "状态");
        assert_eq!(alert.style.fg, Some(Color::White));
        assert_eq!(alert.style.bg, Some(Color::Green));
    }

    #[test]
    fn browse_hint_mentions_backend_switch_shortcut() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().to_path_buf();
        let store = HarnessStore::new(&repo_root, BackendProvider::Codex);
        let app = TuiApp {
            repo_root,
            store,
            config: AppConfig::default(),
            threads: Vec::new(),
            selected_thread_id: None,
            selected_run_id: None,
            selected_task_node_id: None,
            messages: Vec::new(),
            runs: Vec::new(),
            task_nodes: Vec::new(),
            events: Vec::new(),
            approvals: Vec::new(),
            selected_approval_index: 0,
            artifacts: Vec::new(),
            subagents: Vec::new(),
            current_contract: None,
            current_progress: None,
            working_memory: Vec::new(),
            project_memory: Vec::new(),
            focus: FocusMode::Browse,
            browse_pane: BrowsePane::Threads,
            detail_parent_pane: BrowsePane::Runs,
            composer: String::new(),
            pending_send: None,
            pending_delete_thread_id: None,
            last_refresh_at: Instant::now(),
            live_output_title: "实时输出".to_string(),
            live_output_body: String::new(),
            live_output_scroll: 0,
            live_output_follow_latest: true,
            detail_viewport_width: 0,
            detail_viewport_height: 0,
            status: String::new(),
        };

        assert!(primary_hint(&app).contains("m 切换执行模式"));
    }

    #[test]
    fn current_backend_name_uses_user_facing_label() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().to_path_buf();
        let store = HarnessStore::new(&repo_root, BackendProvider::Codex);
        let mut config = AppConfig::default();
        config.backend.provider = BackendProvider::OpenAiCompatible;
        let app = TuiApp {
            repo_root,
            store,
            config,
            threads: Vec::new(),
            selected_thread_id: None,
            selected_run_id: None,
            selected_task_node_id: None,
            messages: Vec::new(),
            runs: Vec::new(),
            task_nodes: Vec::new(),
            events: Vec::new(),
            approvals: Vec::new(),
            selected_approval_index: 0,
            artifacts: Vec::new(),
            subagents: Vec::new(),
            current_contract: None,
            current_progress: None,
            working_memory: Vec::new(),
            project_memory: Vec::new(),
            focus: FocusMode::Browse,
            browse_pane: BrowsePane::Threads,
            detail_parent_pane: BrowsePane::Runs,
            composer: String::new(),
            pending_send: None,
            pending_delete_thread_id: None,
            last_refresh_at: Instant::now(),
            live_output_title: "实时输出".to_string(),
            live_output_body: String::new(),
            live_output_scroll: 0,
            live_output_follow_latest: true,
            detail_viewport_width: 0,
            detail_viewport_height: 0,
            status: String::new(),
        };

        assert_eq!(current_backend_name(&app), "OpenAI Compatible");
    }

    #[test]
    fn footer_prefers_runtime_status_message() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().to_path_buf();
        let store = HarnessStore::new(&repo_root, BackendProvider::Codex);
        let mut app = TuiApp {
            repo_root,
            store,
            config: AppConfig::default(),
            threads: Vec::new(),
            selected_thread_id: None,
            selected_run_id: None,
            selected_task_node_id: None,
            messages: Vec::new(),
            runs: Vec::new(),
            task_nodes: Vec::new(),
            events: Vec::new(),
            approvals: Vec::new(),
            selected_approval_index: 0,
            artifacts: Vec::new(),
            subagents: Vec::new(),
            current_contract: None,
            current_progress: None,
            working_memory: Vec::new(),
            project_memory: Vec::new(),
            focus: FocusMode::Browse,
            browse_pane: BrowsePane::Threads,
            detail_parent_pane: BrowsePane::Runs,
            composer: String::new(),
            pending_send: None,
            pending_delete_thread_id: None,
            last_refresh_at: Instant::now(),
            live_output_title: "实时输出".to_string(),
            live_output_body: String::new(),
            live_output_scroll: 0,
            live_output_follow_latest: true,
            detail_viewport_width: 0,
            detail_viewport_height: 0,
            status: String::new(),
        };
        app.status = "刚刚刷新完成".to_string();
        assert_eq!(footer_text(&app), "刚刚刷新完成");
    }

    #[test]
    fn configured_model_summary_hides_openai_model_in_codex_mode() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().to_path_buf();
        let store = HarnessStore::new(&repo_root, BackendProvider::Codex);
        let mut config = AppConfig::default();
        config.backend.provider = BackendProvider::Codex;
        config.backend.model = Some("MiniMax-M2.7".to_string());
        let app = TuiApp {
            repo_root,
            store,
            config,
            threads: Vec::new(),
            selected_thread_id: None,
            selected_run_id: None,
            selected_task_node_id: None,
            messages: Vec::new(),
            runs: Vec::new(),
            task_nodes: Vec::new(),
            events: Vec::new(),
            approvals: Vec::new(),
            selected_approval_index: 0,
            artifacts: Vec::new(),
            subagents: Vec::new(),
            current_contract: None,
            current_progress: None,
            working_memory: Vec::new(),
            project_memory: Vec::new(),
            focus: FocusMode::Browse,
            browse_pane: BrowsePane::Threads,
            detail_parent_pane: BrowsePane::Runs,
            composer: String::new(),
            pending_send: None,
            pending_delete_thread_id: None,
            last_refresh_at: Instant::now(),
            live_output_title: "实时输出".to_string(),
            live_output_body: String::new(),
            live_output_scroll: 0,
            live_output_follow_latest: true,
            detail_viewport_width: 0,
            detail_viewport_height: 0,
            status: String::new(),
        };

        assert_eq!(configured_model_summary(&app), "Codex CLI 内置模型");
    }

    #[test]
    fn plan_wait_uses_explicit_confirmation_card() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().to_path_buf();
        let store = HarnessStore::new(&repo_root, BackendProvider::Codex);
        let run_dir = repo_root.join("run");
        let run = HarnessRunManifest {
            id: "run-1".to_string(),
            thread_id: "thread-1".to_string(),
            status: HarnessRunStatus::WaitingForInput,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            model: None,
            thinking_mode: ThinkingMode::Balanced,
            backend: AgentBackendKind::Codex,
            execution_kind: RunExecutionKind::Orchestrated,
            turn_count: 2,
            summary: Some("计划已生成，等待用户确认".to_string()),
            last_error: None,
            blocked_reason: Some("计划已生成，等待用户确认或补充反馈".to_string()),
            run_dir: run_dir.clone(),
            events_path: run_dir.join("events.jsonl"),
            output_path: run_dir.join("assistant.md"),
            log_path: run_dir.join("codex.log"),
            tool_calls_path: run_dir.join("tool-calls.jsonl"),
            approvals_path: run_dir.join("approvals.jsonl"),
            artifacts_path: run_dir.join("artifacts.jsonl"),
            subagents_path: run_dir.join("subagents.jsonl"),
            task_graph_path: run_dir.join("task-graph.json"),
            task_nodes_path: run_dir.join("task-nodes.jsonl"),
            evaluation_log_path: run_dir.join("evaluations.jsonl"),
            bootstrap_path: run_dir.join("session-bootstrap.md"),
            active_task_node_id: Some("node-1".to_string()),
            sandbox: None,
        };
        let node = TaskNodeRecord {
            id: "node-1".to_string(),
            graph_id: "graph-1".to_string(),
            thread_id: "thread-1".to_string(),
            run_id: "run-1".to_string(),
            kind: TaskNodeKind::PlanReview,
            title: "审查执行计划".to_string(),
            instructions: "检查计划摘要并等待确认".to_string(),
            depends_on: Vec::new(),
            position: 0,
            status: TaskNodeStatus::WaitingForInput,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            started_at: None,
            completed_at: None,
            output_summary: Some("等待操作：按 Enter 确认计划继续执行".to_string()),
            error: None,
            last_subagent_id: None,
            attempt_count: 0,
            feature_id: None,
        };
        let app = TuiApp {
            repo_root,
            store,
            config: AppConfig::default(),
            threads: Vec::new(),
            selected_thread_id: Some("thread-1".to_string()),
            selected_run_id: Some("run-1".to_string()),
            selected_task_node_id: Some("node-1".to_string()),
            messages: Vec::new(),
            runs: vec![run],
            task_nodes: vec![node],
            events: Vec::new(),
            approvals: Vec::new(),
            selected_approval_index: 0,
            artifacts: Vec::new(),
            subagents: Vec::new(),
            current_contract: Some(ExecutionContract {
                goal: "修复 Codex 模式串线".to_string(),
                non_goals: Vec::new(),
                constraints: Vec::new(),
                ordered_features: Vec::new(),
                global_acceptance: Vec::new(),
                delivery_notes: Vec::new(),
                updated_at: Utc::now(),
            }),
            current_progress: Some(ProgressLedger {
                goal: "修复 Codex 模式串线".to_string(),
                current_phase: Some("计划".to_string()),
                completed_features: Vec::new(),
                current_feature: None,
                latest_recoverable_failure: None,
                blocking_reason: Some("等待用户确认计划或补充反馈".to_string()),
                known_failures: Vec::new(),
                decisions: vec!["计划已生成，等待用户确认".to_string()],
                open_questions: Vec::new(),
                next_step: Some(
                    "按 Enter 确认计划，或在 Composer 提交反馈后重新生成计划".to_string(),
                ),
                updated_at: Utc::now(),
            }),
            working_memory: Vec::new(),
            project_memory: Vec::new(),
            focus: FocusMode::Browse,
            browse_pane: BrowsePane::Runs,
            detail_parent_pane: BrowsePane::Runs,
            composer: String::new(),
            pending_send: None,
            pending_delete_thread_id: None,
            last_refresh_at: Instant::now(),
            live_output_title: "实时输出".to_string(),
            live_output_body: String::new(),
            live_output_scroll: 0,
            live_output_follow_latest: true,
            detail_viewport_width: 0,
            detail_viewport_height: 0,
            status: String::new(),
        };

        let (title, body) = primary_panel_content(&app);
        assert_eq!(title, "计划确认");
        assert!(body.contains("[Enter] 继续执行"));
        assert!(plan_confirmation_card_text(&app).contains("[i] 进入 Composer"));
    }
}
