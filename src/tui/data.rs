use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::commands::format::status_label;
use crate::config::{BackendProvider, LoadedGlobalConfig, set_global_backend_provider};
use crate::harness::{
    ApprovalStatus, ChatRequest, HarnessRunStatus, MemoryLayer, chat_once,
    resolve_approval_and_resume,
};
use crate::harness::{resume_run, retry_task_node_and_resume};
use crate::model::ThinkingMode;

use super::app::PendingSend;
use super::app::TuiApp;
use super::render::paragraph_max_scroll;
use super::tabs::{BrowsePane, FocusMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveOutputMode {
    Preview,
    Detail,
}

#[derive(Debug, Clone)]
struct FileObservation {
    exists: bool,
    bytes: u64,
    modified_at: Option<DateTime<Utc>>,
}

impl TuiApp {
    pub(crate) fn refresh(&mut self) -> Result<()> {
        self.threads = self.store.list_threads()?;
        if self.threads.is_empty() {
            self.clear_selected_thread_state();
            self.selected_thread_id = None;
            self.pending_delete_thread_id = None;
            self.last_refresh_at = std::time::Instant::now();
            return Ok(());
        }

        let selected = self
            .selected_thread_id
            .as_ref()
            .and_then(|id| self.threads.iter().position(|thread| thread.id == *id))
            .unwrap_or(0);
        self.selected_thread_id = Some(self.threads[selected].id.clone());
        if self.pending_delete_thread_id.as_deref() != self.selected_thread_id.as_deref() {
            self.pending_delete_thread_id = None;
        }
        let result = self.load_selected_thread();
        self.last_refresh_at = std::time::Instant::now();
        result
    }

    pub(crate) fn load_selected_thread(&mut self) -> Result<()> {
        let Some(thread_id) = self.selected_thread_id.clone() else {
            self.clear_selected_thread_state();
            return Ok(());
        };
        self.messages = self.store.list_messages(&thread_id).unwrap_or_default();
        self.runs = self.store.list_runs(&thread_id).unwrap_or_default();
        if self
            .selected_run_id
            .as_ref()
            .is_none_or(|id| !self.runs.iter().any(|run| &run.id == id))
        {
            self.selected_run_id = self.runs.first().map(|run| run.id.clone());
        }
        let selected_run = self
            .selected_run_id
            .as_deref()
            .and_then(|run_id| self.store.load_run(&thread_id, run_id).ok())
            .or_else(|| {
                self.runs.first().and_then(|run| {
                    self.selected_run_id = Some(run.id.clone());
                    self.store.load_run(&thread_id, &run.id).ok()
                })
            });
        self.task_nodes = selected_run
            .as_ref()
            .and_then(|run| self.store.list_task_nodes(run).ok())
            .unwrap_or_default();
        self.subagents = selected_run
            .as_ref()
            .and_then(|run| self.store.list_subagents(run).ok())
            .unwrap_or_default();
        if self
            .selected_task_node_id
            .as_ref()
            .is_none_or(|id| !self.task_nodes.iter().any(|node| &node.id == id))
        {
            self.selected_task_node_id = self
                .selected_run_id
                .as_ref()
                .and_then(|run_id| {
                    self.runs
                        .iter()
                        .find(|run| &run.id == run_id)
                        .and_then(|run| run.active_task_node_id.clone())
                })
                .or_else(|| preferred_task_node_id(&self.task_nodes));
        }
        self.approvals = self
            .store
            .list_pending_approvals(Some(&thread_id))
            .unwrap_or_default();
        if self.approvals.is_empty() {
            self.selected_approval_index = 0;
        } else if self.selected_approval_index >= self.approvals.len() {
            self.selected_approval_index = self.approvals.len().saturating_sub(1);
        }
        self.artifacts = self
            .store
            .list_artifacts(Some(&thread_id), self.selected_run_id.as_deref())
            .unwrap_or_default();
        self.current_contract = self.store.load_execution_contract(&thread_id).ok();
        self.current_progress = self.store.load_progress_ledger(&thread_id).ok();
        self.working_memory = self
            .store
            .load_memory(&thread_id, MemoryLayer::Working)?
            .entries;
        self.project_memory = self
            .store
            .load_memory(&thread_id, MemoryLayer::Project)?
            .entries;
        self.events = selected_run
            .as_ref()
            .and_then(|run| self.store.list_run_events(&thread_id, &run.id).ok())
            .or_else(|| {
                self.runs
                    .first()
                    .and_then(|run| self.store.list_run_events(&thread_id, &run.id).ok())
            })
            .unwrap_or_default();
        let (title, body) = build_live_output_preview(selected_run.as_ref(), &self.subagents);
        self.live_output_title = title;
        self.live_output_body = body;
        Ok(())
    }

    pub(crate) fn maybe_auto_refresh(&mut self) -> Result<()> {
        if !self.should_auto_refresh() {
            return Ok(());
        }
        if self.last_refresh_at.elapsed() < Duration::from_millis(500) {
            return Ok(());
        }
        self.refresh()
    }

    fn should_auto_refresh(&self) -> bool {
        if self.pending_send.is_some() {
            return true;
        }
        self.runs.iter().any(|run| {
            matches!(
                run.status,
                HarnessRunStatus::Pending
                    | HarnessRunStatus::Running
                    | HarnessRunStatus::WaitingForInput
            )
        })
    }

    pub(crate) async fn send_message(&mut self) -> Result<()> {
        if self.pending_send.is_some() {
            self.status = "已有草稿在后台运行，请等待完成后再发送".to_string();
            return Ok(());
        }
        let message = self.composer.trim().to_string();
        if message.is_empty() {
            self.status = "消息不能为空".to_string();
            return Ok(());
        }
        let thread_id = match self.selected_thread_id.clone() {
            Some(thread_id) => thread_id,
            None => self.store.create_thread(None)?.id,
        };
        let repo_root = self.repo_root.clone();
        let config = self.config.clone();
        let request = ChatRequest {
            thread_id: thread_id.clone(),
            message,
            model: self.config.backend.model.clone(),
            thinking_mode: ThinkingMode::Balanced,
        };
        self.selected_thread_id = Some(thread_id.clone());
        self.focus = FocusMode::Browse;
        self.pending_delete_thread_id = None;
        self.pending_send = Some(PendingSend {
            thread_id: thread_id.clone(),
            handle: tokio::spawn(async move { chat_once(&repo_root, &config, request).await }),
        });
        self.refresh()?;
        self.status =
            format!("草稿已提交到 `{thread_id}`，后台开始运行；你可以继续浏览，按 r 查看最新进度");
        Ok(())
    }

    pub(crate) fn cycle_backend_provider(&mut self) {
        self.cycle_backend_provider_with(set_global_backend_provider);
    }

    pub(crate) async fn poll_background_tasks(&mut self) -> Result<()> {
        let Some(pending) = self.pending_send.as_ref() else {
            return Ok(());
        };
        if !pending.handle.is_finished() {
            return Ok(());
        }

        let pending = self.pending_send.take().expect("pending send should exist");
        let thread_id = pending.thread_id.clone();
        match pending.handle.await.context("后台运行任务异常退出")? {
            Ok(outcome) => {
                self.selected_thread_id = Some(thread_id.clone());
                self.composer.clear();
                self.refresh()?;
                self.status = format!("thread `{thread_id}` 运行完成：{}", outcome.run.id);
            }
            Err(error) => {
                self.selected_thread_id = Some(thread_id.clone());
                self.refresh()?;
                self.status = format!("thread `{thread_id}` 运行失败：{error:#}");
            }
        }
        Ok(())
    }

    pub(crate) async fn approve_first_pending(&mut self) -> Result<()> {
        let Some(approval) = self.selected_approval().cloned() else {
            self.status = "当前没有待处理审批".to_string();
            return Ok(());
        };
        let run = resolve_approval_and_resume(
            &self.repo_root,
            &self.config,
            &approval.thread_id,
            &approval.id,
            ApprovalStatus::Approved,
        )
        .await?;
        self.refresh()?;
        self.status = format!(
            "已通过审批 `{}`，run 状态：{}",
            approval.id,
            status_label(run.status)
        );
        Ok(())
    }

    pub(crate) async fn deny_first_pending(&mut self) -> Result<()> {
        let Some(approval) = self.selected_approval().cloned() else {
            self.status = "当前没有待处理审批".to_string();
            return Ok(());
        };
        let run = resolve_approval_and_resume(
            &self.repo_root,
            &self.config,
            &approval.thread_id,
            &approval.id,
            ApprovalStatus::Denied,
        )
        .await?;
        self.refresh()?;
        self.status = format!(
            "已拒绝审批 `{}`，run 状态：{}",
            approval.id,
            status_label(run.status)
        );
        Ok(())
    }

    pub(crate) fn select_next_approval(&mut self) {
        if self.approvals.is_empty() {
            return;
        }
        self.selected_approval_index = (self.selected_approval_index + 1) % self.approvals.len();
    }

    pub(crate) fn select_prev_approval(&mut self) {
        if self.approvals.is_empty() {
            return;
        }
        self.selected_approval_index = if self.selected_approval_index == 0 {
            self.approvals.len().saturating_sub(1)
        } else {
            self.selected_approval_index.saturating_sub(1)
        };
    }

    pub(crate) fn selected_approval(&self) -> Option<&crate::harness::ApprovalRecord> {
        self.approvals.get(self.selected_approval_index)
    }

    pub(crate) fn focus_next_pane(&mut self) {
        let current = self.browse_pane;
        self.browse_pane = self.browse_pane.next();
        if matches!(self.browse_pane, BrowsePane::Detail)
            && matches!(
                current,
                BrowsePane::Threads | BrowsePane::Runs | BrowsePane::Steps
            )
        {
            self.detail_parent_pane = current;
            self.reset_detail_position_for_parent();
        }
        self.status = format!("已切换到{}", pane_label(self.browse_pane));
    }

    pub(crate) fn focus_prev_pane(&mut self) {
        let current = self.browse_pane;
        self.browse_pane = self.browse_pane.prev();
        if matches!(self.browse_pane, BrowsePane::Detail) && matches!(current, BrowsePane::Composer)
        {
            self.reset_detail_position_for_parent();
        }
        self.status = format!("已切换到{}", pane_label(self.browse_pane));
    }

    pub(crate) fn enter_detail(&mut self) {
        self.detail_parent_pane = match self.browse_pane {
            BrowsePane::Threads | BrowsePane::Runs | BrowsePane::Steps => self.browse_pane,
            BrowsePane::Detail => self.detail_parent_pane,
            BrowsePane::Composer => BrowsePane::Runs,
        };
        self.browse_pane = BrowsePane::Detail;
        self.reset_detail_position_for_parent();
        self.status = if matches!(self.detail_parent_pane, BrowsePane::Runs) {
            "已进入实时输出详情；默认跟随最新输出，手动滚动后会固定当前位置，Esc 返回".to_string()
        } else {
            "已进入详情视图，可上下滚动，Esc 返回".to_string()
        };
    }

    pub(crate) fn exit_detail(&mut self) {
        self.browse_pane = self.detail_parent_pane;
        self.status = format!("返回{}", pane_label(self.browse_pane));
    }

    pub(crate) fn scroll_detail_down(&mut self) {
        if self.detail_targets_live_output() {
            if self.live_output_follow_latest {
                return;
            }
            let next = self.live_output_scroll.saturating_add(1);
            let max_scroll = self.current_live_output_max_scroll();
            if next >= max_scroll {
                self.reset_live_output_position();
                self.status = "已回到底部，继续跟随最新输出".to_string();
            } else {
                self.live_output_scroll = next;
            }
            return;
        }
        self.live_output_scroll = self.live_output_scroll.saturating_add(1);
    }

    pub(crate) fn scroll_detail_up(&mut self) {
        if self.detail_targets_live_output() {
            let current = if self.live_output_follow_latest {
                self.current_live_output_max_scroll()
            } else {
                self.live_output_scroll
            };
            self.live_output_follow_latest = false;
            self.live_output_scroll = current.saturating_sub(1);
            return;
        }
        self.live_output_scroll = self.live_output_scroll.saturating_sub(1);
    }

    pub(crate) fn select_next_in_focus(&mut self) -> Result<()> {
        match self.browse_pane {
            BrowsePane::Threads => self.select_next(),
            BrowsePane::Runs => self.select_next_run(),
            BrowsePane::Steps => self.select_next_task_node(),
            BrowsePane::Detail => {
                self.scroll_detail_down();
                Ok(())
            }
            BrowsePane::Composer => Ok(()),
        }
    }

    pub(crate) fn select_prev_in_focus(&mut self) -> Result<()> {
        match self.browse_pane {
            BrowsePane::Threads => self.select_prev(),
            BrowsePane::Runs => self.select_prev_run(),
            BrowsePane::Steps => self.select_prev_task_node(),
            BrowsePane::Detail => {
                self.scroll_detail_up();
                Ok(())
            }
            BrowsePane::Composer => Ok(()),
        }
    }

    pub(crate) fn select_next(&mut self) -> Result<()> {
        if self.threads.is_empty() {
            return Ok(());
        }
        let current = self
            .selected_thread_id
            .as_ref()
            .and_then(|id| self.threads.iter().position(|thread| thread.id == *id))
            .unwrap_or(0);
        let next = (current + 1).min(self.threads.len().saturating_sub(1));
        self.selected_thread_id = Some(self.threads[next].id.clone());
        self.selected_run_id = None;
        self.selected_task_node_id = None;
        self.pending_delete_thread_id = None;
        self.reset_live_output_position();
        self.load_selected_thread()
    }

    pub(crate) fn select_prev(&mut self) -> Result<()> {
        if self.threads.is_empty() {
            return Ok(());
        }
        let current = self
            .selected_thread_id
            .as_ref()
            .and_then(|id| self.threads.iter().position(|thread| thread.id == *id))
            .unwrap_or(0);
        let next = current.saturating_sub(1);
        self.selected_thread_id = Some(self.threads[next].id.clone());
        self.selected_run_id = None;
        self.selected_task_node_id = None;
        self.pending_delete_thread_id = None;
        self.reset_live_output_position();
        self.load_selected_thread()
    }

    pub(crate) fn select_next_run(&mut self) -> Result<()> {
        if self.runs.is_empty() {
            return Ok(());
        }
        let current = self
            .selected_run_id
            .as_ref()
            .and_then(|id| self.runs.iter().position(|run| run.id == *id))
            .unwrap_or(0);
        let next = (current + 1).min(self.runs.len().saturating_sub(1));
        self.selected_run_id = Some(self.runs[next].id.clone());
        self.selected_task_node_id = None;
        self.reset_live_output_position();
        self.load_selected_thread()
    }

    pub(crate) fn select_prev_run(&mut self) -> Result<()> {
        if self.runs.is_empty() {
            return Ok(());
        }
        let current = self
            .selected_run_id
            .as_ref()
            .and_then(|id| self.runs.iter().position(|run| run.id == *id))
            .unwrap_or(0);
        let next = current.saturating_sub(1);
        self.selected_run_id = Some(self.runs[next].id.clone());
        self.selected_task_node_id = None;
        self.reset_live_output_position();
        self.load_selected_thread()
    }

    pub(crate) fn select_next_task_node(&mut self) -> Result<()> {
        if self.task_nodes.is_empty() {
            return Ok(());
        }
        let current = self
            .selected_task_node_id
            .as_ref()
            .and_then(|id| self.task_nodes.iter().position(|node| node.id == *id))
            .unwrap_or(0);
        let next = (current + 1).min(self.task_nodes.len().saturating_sub(1));
        self.selected_task_node_id = Some(self.task_nodes[next].id.clone());
        self.reset_standard_detail_position();
        Ok(())
    }

    pub(crate) fn select_prev_task_node(&mut self) -> Result<()> {
        if self.task_nodes.is_empty() {
            return Ok(());
        }
        let current = self
            .selected_task_node_id
            .as_ref()
            .and_then(|id| self.task_nodes.iter().position(|node| node.id == *id))
            .unwrap_or(0);
        let next = current.saturating_sub(1);
        self.selected_task_node_id = Some(self.task_nodes[next].id.clone());
        self.reset_standard_detail_position();
        Ok(())
    }

    fn reset_live_output_position(&mut self) {
        self.live_output_scroll = 0;
        self.live_output_follow_latest = true;
    }

    fn clear_selected_thread_state(&mut self) {
        self.selected_run_id = None;
        self.selected_task_node_id = None;
        self.messages.clear();
        self.runs.clear();
        self.task_nodes.clear();
        self.events.clear();
        self.approvals.clear();
        self.selected_approval_index = 0;
        self.artifacts.clear();
        self.subagents.clear();
        self.current_contract = None;
        self.current_progress = None;
        self.working_memory.clear();
        self.project_memory.clear();
        self.live_output_title = "实时输出".to_string();
        self.live_output_body = "当前没有运行内容".to_string();
        self.reset_live_output_position();
    }

    fn reset_standard_detail_position(&mut self) {
        self.live_output_scroll = 0;
        self.live_output_follow_latest = false;
    }

    fn reset_detail_position_for_parent(&mut self) {
        if self.detail_targets_live_output() {
            self.reset_live_output_position();
        } else {
            self.reset_standard_detail_position();
        }
    }

    fn detail_targets_live_output(&self) -> bool {
        matches!(
            self.detail_parent_pane,
            BrowsePane::Runs | BrowsePane::Detail | BrowsePane::Composer
        )
    }

    fn current_run_manifest(&self) -> Option<&crate::harness::HarnessRunManifest> {
        self.selected_run_id
            .as_ref()
            .and_then(|id| self.runs.iter().find(|run| &run.id == id))
            .or_else(|| self.runs.first())
    }

    fn current_live_output_max_scroll(&self) -> u16 {
        if self.detail_viewport_width == 0 || self.detail_viewport_height == 0 {
            return 0;
        }
        let (_, body) = build_live_output_detail(self.current_run_manifest(), &self.subagents);
        paragraph_max_scroll(
            &body,
            self.detail_viewport_width,
            self.detail_viewport_height,
        )
    }

    pub(crate) async fn resume_selected_run(&mut self) -> Result<()> {
        let Some(thread_id) = self.selected_thread_id.clone() else {
            self.status = "当前没有选中 thread".to_string();
            return Ok(());
        };
        let Some(run_id) = self.selected_run_id.clone() else {
            self.status = "当前没有选中 run".to_string();
            return Ok(());
        };
        let run = resume_run(&self.repo_root, &self.config, &thread_id, &run_id).await?;
        self.refresh()?;
        self.status = format!("已恢复 run `{}`：{}", run.id, status_label(run.status));
        Ok(())
    }

    pub(crate) async fn retry_selected_task_node(&mut self) -> Result<()> {
        let Some(thread_id) = self.selected_thread_id.clone() else {
            self.status = "当前没有选中 thread".to_string();
            return Ok(());
        };
        let Some(run_id) = self.selected_run_id.clone() else {
            self.status = "当前没有选中 run".to_string();
            return Ok(());
        };
        let Some(task_node_id) = self.selected_task_node_id.clone() else {
            self.status = "当前没有选中节点".to_string();
            return Ok(());
        };
        let run = retry_task_node_and_resume(
            &self.repo_root,
            &self.config,
            &thread_id,
            &run_id,
            &task_node_id,
        )
        .await?;
        self.refresh()?;
        self.status = format!(
            "已重试节点 `{}`，run `{}` 状态：{}",
            task_node_id,
            run.id,
            status_label(run.status)
        );
        Ok(())
    }

    pub(crate) fn delete_selected_thread(&mut self) -> Result<()> {
        let Some(thread_id) = self.selected_thread_id.clone() else {
            self.status = "当前没有选中 thread".to_string();
            return Ok(());
        };
        if self
            .pending_send
            .as_ref()
            .is_some_and(|pending| pending.thread_id == thread_id)
        {
            self.status = "当前 thread 正在后台运行，不能删除".to_string();
            return Ok(());
        }
        if self.runs.iter().any(|run| {
            matches!(
                run.status,
                HarnessRunStatus::Pending
                    | HarnessRunStatus::Running
                    | HarnessRunStatus::WaitingForInput
            )
        }) {
            self.pending_delete_thread_id = None;
            self.status = "当前 thread 仍有活动 run，请先处理或取消后再删除".to_string();
            return Ok(());
        }
        if self.pending_delete_thread_id.as_deref() != Some(thread_id.as_str()) {
            self.pending_delete_thread_id = Some(thread_id.clone());
            self.status = format!("按 Enter 删除 thread `{thread_id}`，按 Esc 取消");
            return Ok(());
        }

        let current = self
            .selected_thread_id
            .as_ref()
            .and_then(|id| self.threads.iter().position(|thread| thread.id == *id))
            .unwrap_or(0);
        let fallback_selected = self
            .threads
            .iter()
            .enumerate()
            .find(|(index, thread)| *index != current && thread.id != thread_id)
            .map(|(_, thread)| thread.id.clone());

        self.store.delete_thread(&thread_id)?;
        self.pending_delete_thread_id = None;
        self.selected_thread_id = fallback_selected;
        self.selected_run_id = None;
        self.selected_task_node_id = None;
        self.refresh()?;
        self.status = format!("已删除 thread `{thread_id}`");
        Ok(())
    }

    fn cycle_backend_provider_with<F>(&mut self, persist: F)
    where
        F: FnOnce(BackendProvider) -> Result<LoadedGlobalConfig>,
    {
        let next = self.config.backend.provider.next();
        match persist(next) {
            Ok(loaded) => {
                self.config.backend = loaded.value.backend;
                let carry_over_note = if self.pending_send.is_some() {
                    "；当前后台 run 保持原 Backend，后续新 run 才会使用新配置"
                } else {
                    ""
                };
                self.status = format!(
                    "默认 Backend 已切换为 {}，并写入 {}{}",
                    self.config.backend.provider.display_name(),
                    loaded.path.display(),
                    carry_over_note
                );
            }
            Err(error) => {
                self.status = format!("切换 Backend 失败：{error:#}");
            }
        }
    }
}

fn build_live_output_preview(
    run: Option<&crate::harness::HarnessRunManifest>,
    subagents: &[crate::harness::types::SubagentRecord],
) -> (String, String) {
    build_live_output(run, subagents, LiveOutputMode::Preview)
}

pub(crate) fn build_live_output_detail(
    run: Option<&crate::harness::HarnessRunManifest>,
    subagents: &[crate::harness::types::SubagentRecord],
) -> (String, String) {
    build_live_output(run, subagents, LiveOutputMode::Detail)
}

fn build_live_output(
    run: Option<&crate::harness::HarnessRunManifest>,
    subagents: &[crate::harness::types::SubagentRecord],
    mode: LiveOutputMode,
) -> (String, String) {
    let Some(run) = run else {
        return ("实时输出".to_string(), "等待运行...".to_string());
    };

    let active_subagent = subagents
        .iter()
        .filter(|subagent| {
            matches!(
                subagent.status,
                HarnessRunStatus::Running | HarnessRunStatus::WaitingForInput
            )
        })
        .max_by_key(|subagent| subagent.updated_at)
        .or_else(|| subagents.iter().max_by_key(|subagent| subagent.updated_at));

    let (title, log_path, output_path) = if let Some(subagent) = active_subagent {
        (
            format!("实时输出 · {}", subagent_label(subagent)),
            subagent.log_path.as_path(),
            subagent.output_path.as_path(),
        )
    } else {
        (
            "实时输出 · 主代理".to_string(),
            run.log_path.as_path(),
            run.output_path.as_path(),
        )
    };

    let log_observation = observe_file(log_path);
    let output_observation = observe_file(output_path);
    let latest_visible_at = latest_visible_output_at(&log_observation, &output_observation);
    let latest_internal_at = active_subagent
        .map(|subagent| subagent.updated_at.max(run.updated_at))
        .unwrap_or(run.updated_at);

    let (log_text, output_text) = match mode {
        LiveOutputMode::Preview => (read_full(log_path), read_full(output_path)),
        LiveOutputMode::Detail => (read_full(log_path), read_full(output_path)),
    };
    let observation = render_live_output_observation(
        run,
        active_subagent,
        &log_observation,
        &output_observation,
        latest_visible_at,
        latest_internal_at,
    );
    let content = match (output_text.trim().is_empty(), log_text.trim().is_empty()) {
        (false, false) => format!(
            "最新输出:\n{}\n\n运行日志:\n{}",
            output_text.trim(),
            log_text.trim()
        ),
        (false, true) => format!("最新输出:\n{}", output_text.trim()),
        (true, false) => format!("运行日志:\n{}", log_text.trim()),
        (true, true) => "运行已开始，等待输出...".to_string(),
    };
    (title, format!("{observation}\n\n{content}"))
}

fn observe_file(path: &Path) -> FileObservation {
    let Ok(metadata) = fs::metadata(path) else {
        return FileObservation {
            exists: false,
            bytes: 0,
            modified_at: None,
        };
    };
    let modified_at = metadata.modified().ok().map(DateTime::<Utc>::from);
    FileObservation {
        exists: true,
        bytes: metadata.len(),
        modified_at,
    }
}

fn latest_visible_output_at(
    log_observation: &FileObservation,
    output_observation: &FileObservation,
) -> Option<DateTime<Utc>> {
    match (log_observation.modified_at, output_observation.modified_at) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn render_live_output_observation(
    run: &crate::harness::HarnessRunManifest,
    active_subagent: Option<&crate::harness::types::SubagentRecord>,
    log_observation: &FileObservation,
    output_observation: &FileObservation,
    latest_visible_at: Option<DateTime<Utc>>,
    latest_internal_at: DateTime<Utc>,
) -> String {
    let latest_visible_age = latest_visible_at
        .map(format_relative_time)
        .unwrap_or_else(|| "尚无".to_string());
    let internal_age = format_relative_time(latest_internal_at);
    let diagnostic = live_output_diagnostic(run, latest_visible_at);
    let actor = active_subagent
        .map(subagent_label)
        .unwrap_or_else(|| "主代理".to_string());

    format!(
        "观测\n----------------\n状态：{}\n执行者：{}\n最近可见输出：{}\n最近内部活动：{}\n日志文件：{}\n输出文件：{}\n诊断：{}",
        status_label(run.status),
        actor,
        latest_visible_age,
        internal_age,
        describe_file_observation(log_observation),
        describe_file_observation(output_observation),
        diagnostic,
    )
}

fn live_output_diagnostic(
    run: &crate::harness::HarnessRunManifest,
    latest_visible_at: Option<DateTime<Utc>>,
) -> String {
    let is_active = matches!(
        run.status,
        HarnessRunStatus::Pending | HarnessRunStatus::Running | HarnessRunStatus::WaitingForInput
    );
    if !is_active {
        return "当前 run 已结束，可结合最新输出和事件流回看结果。".to_string();
    }

    let now = Utc::now();
    let visible_idle_secs = latest_visible_at
        .map(|value| (now - value).num_seconds().max(0))
        .unwrap_or_else(|| (now - run.created_at).num_seconds().max(0));

    match latest_visible_at {
        None if visible_idle_secs >= 45 => format!(
            "运行已持续 {}，但仍没有任何日志或输出文件更新；很可能卡在外部命令、模型等待或其他阻塞点。",
            format_elapsed_seconds(visible_idle_secs)
        ),
        None if visible_idle_secs >= 15 => format!(
            "运行已持续 {}，但还没有看到第一条日志或输出；可能仍在等待，也可能已经卡住。",
            format_elapsed_seconds(visible_idle_secs)
        ),
        None => "运行中，等待第一条日志或输出写入。".to_string(),
        Some(_) if visible_idle_secs >= 45 => format!(
            "最近 {} 没有新的可见输出；如果事件流和状态也不再变化，基本可以按疑似卡住处理。",
            format_elapsed_seconds(visible_idle_secs)
        ),
        Some(_) if visible_idle_secs >= 15 => format!(
            "最近 {} 没有新的可见输出；可能是在执行长命令，也可能已经进入停滞。",
            format_elapsed_seconds(visible_idle_secs)
        ),
        Some(_) => "最近仍有可见输出写入，当前更像是在正常推进。".to_string(),
    }
}

fn describe_file_observation(observation: &FileObservation) -> String {
    if !observation.exists {
        return "未创建".to_string();
    }
    let modified = observation
        .modified_at
        .map(format_relative_time)
        .unwrap_or_else(|| "时间未知".to_string());
    format!("{} · {}", format_bytes(observation.bytes), modified)
}

fn format_relative_time(value: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - value).num_seconds().max(0);
    if seconds <= 1 {
        "刚刚".to_string()
    } else {
        format!("{}前", format_elapsed_seconds(seconds))
    }
}

fn format_elapsed_seconds(seconds: i64) -> String {
    if seconds >= 3600 {
        format!(
            "{}h {:02}m {:02}s",
            seconds / 3600,
            (seconds % 3600) / 60,
            seconds % 60
        )
    } else if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else if bytes as f64 >= KB {
        format!("{:.1} KB", bytes as f64 / KB)
    } else {
        format!("{bytes} B")
    }
}

fn preferred_task_node_id(task_nodes: &[crate::harness::types::TaskNodeRecord]) -> Option<String> {
    task_nodes
        .iter()
        .filter(|node| !matches!(node.status, crate::harness::types::TaskNodeStatus::Failed))
        .max_by(|left, right| {
            left.updated_at
                .cmp(&right.updated_at)
                .then(left.position.cmp(&right.position))
        })
        .or_else(|| {
            task_nodes.iter().max_by(|left, right| {
                left.updated_at
                    .cmp(&right.updated_at)
                    .then(left.position.cmp(&right.position))
            })
        })
        .map(|node| node.id.clone())
}

fn read_full(path: &Path) -> String {
    let Ok(content) = fs::read_to_string(path) else {
        return String::new();
    };
    content
}

fn subagent_label(subagent: &crate::harness::types::SubagentRecord) -> String {
    let kind = match subagent.kind {
        crate::harness::types::SubagentKind::Planner => "planner",
        crate::harness::types::SubagentKind::Generator => "builder",
        crate::harness::types::SubagentKind::Evaluator => "reviewer",
    };
    format!("{kind} agent")
}

fn pane_label(pane: BrowsePane) -> &'static str {
    match pane {
        BrowsePane::Threads => "Threads",
        BrowsePane::Runs => "Runs",
        BrowsePane::Steps => "执行步骤",
        BrowsePane::Detail => "详情视图",
        BrowsePane::Composer => "Composer",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::Instant;

    use super::{build_live_output_detail, build_live_output_preview, preferred_task_node_id};
    use anyhow::anyhow;
    use chrono::{Duration, Utc};
    use tempfile::TempDir;

    use crate::config::{
        AppConfig, BackendConfig, BackendProvider, GlobalConfig, LoadedGlobalConfig,
    };
    use crate::harness::HarnessStore;
    use crate::harness::types::{
        AgentBackendKind, HarnessEvent, HarnessMessageRole, HarnessRunManifest, HarnessRunStatus,
        TaskNodeKind, TaskNodeRecord, TaskNodeStatus,
    };
    use crate::model::ThinkingMode;
    use crate::tui::app::TuiApp;
    use crate::tui::tabs::{BrowsePane, FocusMode};

    fn make_task_node(
        id: &str,
        status: TaskNodeStatus,
        position: usize,
        updated_at: chrono::DateTime<Utc>,
    ) -> TaskNodeRecord {
        TaskNodeRecord {
            id: id.to_string(),
            graph_id: "graph-1".to_string(),
            thread_id: "thread-1".to_string(),
            run_id: "run-1".to_string(),
            kind: TaskNodeKind::Implement,
            title: id.to_string(),
            instructions: String::new(),
            depends_on: Vec::new(),
            position,
            status,
            created_at: updated_at,
            updated_at,
            started_at: None,
            completed_at: None,
            output_summary: None,
            error: None,
            last_subagent_id: None,
            attempt_count: 0,
            feature_id: None,
        }
    }

    fn make_test_app(provider: BackendProvider) -> TuiApp {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.keep();
        let store = HarnessStore::new(&repo_root);
        let mut config = AppConfig::default();
        config.backend.provider = provider;
        TuiApp {
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
            live_output_body: "等待运行...".to_string(),
            live_output_scroll: 0,
            live_output_follow_latest: true,
            detail_viewport_width: 0,
            detail_viewport_height: 0,
            status: String::new(),
        }
    }

    #[test]
    fn preferred_task_node_skips_stale_failed_node_by_default() {
        let now = Utc::now();
        let failed = make_task_node("failed", TaskNodeStatus::Failed, 0, now);
        let completed = make_task_node(
            "completed",
            TaskNodeStatus::Completed,
            1,
            now + Duration::seconds(1),
        );

        assert_eq!(
            preferred_task_node_id(&[failed, completed]),
            Some("completed".to_string())
        );
    }

    #[test]
    fn preferred_task_node_falls_back_to_latest_failed_node() {
        let now = Utc::now();
        let older = make_task_node("older", TaskNodeStatus::Failed, 0, now);
        let newer = make_task_node(
            "newer",
            TaskNodeStatus::Failed,
            1,
            now + Duration::seconds(1),
        );

        assert_eq!(
            preferred_task_node_id(&[older, newer]),
            Some("newer".to_string())
        );
    }

    #[test]
    fn live_output_preview_and_detail_keep_full_history() {
        let dir = TempDir::new().expect("tempdir");
        let run_dir = dir.path().join("run");
        fs::create_dir_all(&run_dir).expect("mkdir");
        let output_path = run_dir.join("assistant.md");
        let log_path = run_dir.join("codex.log");
        let log_body = (1..=40)
            .map(|idx| format!("line-{idx:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&log_path, &log_body).expect("write log");
        fs::write(&output_path, "final output\n").expect("write output");

        let run = HarnessRunManifest {
            id: "run-1".to_string(),
            thread_id: "thread-1".to_string(),
            status: HarnessRunStatus::Running,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            model: None,
            thinking_mode: ThinkingMode::Balanced,
            backend: AgentBackendKind::Codex,
            turn_count: 1,
            summary: None,
            last_error: None,
            blocked_reason: None,
            run_dir: run_dir.clone(),
            events_path: run_dir.join("events.jsonl"),
            output_path,
            log_path,
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

        let (_preview_title, preview_body) = build_live_output_preview(Some(&run), &[]);
        let (_detail_title, detail_body) = build_live_output_detail(Some(&run), &[]);

        assert!(preview_body.contains("观测"));
        assert!(preview_body.contains("最近可见输出"));
        assert!(preview_body.contains("line-001"));
        assert!(preview_body.contains("line-040"));
        assert!(detail_body.contains("line-001"));
        assert!(detail_body.contains("line-040"));
    }

    #[test]
    fn live_output_warns_when_running_too_long_without_visible_output() {
        let dir = TempDir::new().expect("tempdir");
        let run_dir = dir.path().join("run");
        fs::create_dir_all(&run_dir).expect("mkdir");
        let run = HarnessRunManifest {
            id: "run-1".to_string(),
            thread_id: "thread-1".to_string(),
            status: HarnessRunStatus::Running,
            created_at: Utc::now() - Duration::seconds(20),
            updated_at: Utc::now() - Duration::seconds(20),
            model: None,
            thinking_mode: ThinkingMode::Balanced,
            backend: AgentBackendKind::Codex,
            turn_count: 1,
            summary: None,
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

        let (_title, body) = build_live_output_preview(Some(&run), &[]);

        assert!(body.contains("还没有看到第一条日志或输出"));
        assert!(body.contains("未创建"));
    }

    #[test]
    fn cycling_backend_updates_runtime_config_and_status() {
        let mut app = make_test_app(BackendProvider::Codex);

        app.cycle_backend_provider_with(|provider| {
            Ok(LoadedGlobalConfig {
                path: PathBuf::from("/tmp/.codex-forge/config.toml"),
                value: GlobalConfig {
                    backend: BackendConfig {
                        provider,
                        key: Some("sk-demo".to_string()),
                        base_url: Some("https://example.com/v1".to_string()),
                        model: Some("demo-model".to_string()),
                        turn_timeout_secs: 600,
                    },
                },
            })
        });

        assert_eq!(
            app.config.backend.provider,
            BackendProvider::OpenAiCompatible
        );
        assert!(app.status.contains("OpenAI Compatible"), "{}", app.status);
        assert!(
            app.status.contains("/tmp/.codex-forge/config.toml"),
            "{}",
            app.status
        );
    }

    #[test]
    fn cycling_backend_failure_keeps_existing_provider() {
        let mut app = make_test_app(BackendProvider::Codex);

        app.cycle_backend_provider_with(|_| Err(anyhow!("backend.key 不能为空")));

        assert_eq!(app.config.backend.provider, BackendProvider::Codex);
        assert!(app.status.contains("切换 Backend 失败"), "{}", app.status);
        assert!(
            app.status.contains("backend.key 不能为空"),
            "{}",
            app.status
        );
    }

    #[test]
    fn entering_run_detail_starts_with_scrollable_offset() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().to_path_buf();
        let store = HarnessStore::new(&repo_root);
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
            browse_pane: BrowsePane::Runs,
            detail_parent_pane: BrowsePane::Runs,
            composer: String::new(),
            pending_send: None,
            pending_delete_thread_id: None,
            last_refresh_at: Instant::now(),
            live_output_title: "实时输出".to_string(),
            live_output_body: "line-1\nline-2\nline-3".to_string(),
            live_output_scroll: 7,
            live_output_follow_latest: false,
            detail_viewport_width: 0,
            detail_viewport_height: 0,
            status: String::new(),
        };

        app.enter_detail();

        assert_eq!(app.browse_pane, BrowsePane::Detail);
        assert_eq!(app.live_output_scroll, 0);
        assert!(app.live_output_follow_latest);
    }

    #[test]
    fn load_selected_thread_keeps_messages_and_events_when_output_files_missing() {
        let dir = TempDir::new().expect("tempdir");
        let store = HarnessStore::new(dir.path());
        let thread = store.create_thread(Some("demo")).expect("thread");
        store
            .append_message(
                &thread.id,
                HarnessMessageRole::User,
                "hello".to_string(),
                None,
            )
            .expect("message");
        let run = store
            .create_run(
                &thread.id,
                None,
                ThinkingMode::Balanced,
                AgentBackendKind::Codex,
            )
            .expect("run");
        store
            .append_run_event(
                &thread.id,
                &run.id,
                HarnessEvent::RunStarted {
                    thread_id: thread.id.clone(),
                    run_id: run.id.clone(),
                },
            )
            .expect("event");

        let mut app = TuiApp::new(dir.path().to_path_buf(), Some(thread.id.clone())).expect("app");
        app.refresh().expect("refresh");

        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.events.len(), 1);
        assert!(app.live_output_body.contains("等待输出"));
    }
}
