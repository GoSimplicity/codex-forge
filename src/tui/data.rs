use anyhow::Result;

use crate::commands::format::status_label;
use crate::harness::{
    ApprovalStatus, ChatRequest, MemoryLayer, chat_once, resolve_approval_and_resume,
};
use crate::harness::{cancel_active_run, resume_run, retry_task_node_and_resume};
use crate::model::ThinkingMode;

use super::app::TuiApp;
use super::tabs::FocusMode;

impl TuiApp {
    pub(crate) fn refresh(&mut self) -> Result<()> {
        self.threads = self.store.list_threads()?;
        if self.threads.is_empty() {
            self.selected_thread_id = None;
            self.selected_run_id = None;
            self.selected_task_node_id = None;
            self.messages.clear();
            self.runs.clear();
            self.task_nodes.clear();
            self.events.clear();
            self.approvals.clear();
            self.artifacts.clear();
            self.working_memory.clear();
            self.project_memory.clear();
            return Ok(());
        }

        let selected = self
            .selected_thread_id
            .as_ref()
            .and_then(|id| self.threads.iter().position(|thread| thread.id == *id))
            .unwrap_or(0);
        self.selected_thread_id = Some(self.threads[selected].id.clone());
        self.load_selected_thread()
    }

    pub(crate) fn load_selected_thread(&mut self) -> Result<()> {
        let Some(thread_id) = self.selected_thread_id.clone() else {
            self.selected_run_id = None;
            self.selected_task_node_id = None;
            self.messages.clear();
            self.runs.clear();
            self.task_nodes.clear();
            self.events.clear();
            self.approvals.clear();
            self.artifacts.clear();
            self.working_memory.clear();
            self.project_memory.clear();
            return Ok(());
        };
        self.messages = self.store.list_messages(&thread_id)?;
        self.runs = self.store.list_runs(&thread_id)?;
        if self
            .selected_run_id
            .as_ref()
            .is_none_or(|id| !self.runs.iter().any(|run| &run.id == id))
        {
            self.selected_run_id = self.runs.first().map(|run| run.id.clone());
        }
        self.task_nodes = if let Some(run_id) = self.selected_run_id.as_deref() {
            let run = self.store.load_run(&thread_id, run_id)?;
            self.store.list_task_nodes(&run)?
        } else {
            Vec::new()
        };
        if self
            .selected_task_node_id
            .as_ref()
            .is_none_or(|id| !self.task_nodes.iter().any(|node| &node.id == id))
        {
            self.selected_task_node_id = self.task_nodes.first().map(|node| node.id.clone());
        }
        self.approvals = self.store.list_pending_approvals(Some(&thread_id))?;
        self.artifacts = self
            .store
            .list_artifacts(Some(&thread_id), self.selected_run_id.as_deref())?;
        self.working_memory = self
            .store
            .load_memory(&thread_id, MemoryLayer::Working)?
            .entries;
        self.project_memory = self
            .store
            .load_memory(&thread_id, MemoryLayer::Project)?
            .entries;
        self.events = if let Some(run_id) = self.selected_run_id.as_deref() {
            self.store.list_run_events(&thread_id, run_id)?
        } else if let Some(run) = self.runs.first() {
            self.store.list_run_events(&thread_id, &run.id)?
        } else {
            Vec::new()
        };
        Ok(())
    }

    pub(crate) async fn send_message(&mut self) -> Result<()> {
        let message = self.composer.trim().to_string();
        if message.is_empty() {
            self.status = "消息不能为空".to_string();
            return Ok(());
        }
        let thread_id = match self.selected_thread_id.clone() {
            Some(thread_id) => thread_id,
            None => self.store.create_thread(None)?.id,
        };
        self.status = format!("正在向 `{thread_id}` 发送消息...");
        let outcome = chat_once(
            &self.repo_root,
            &self.config,
            ChatRequest {
                thread_id: thread_id.clone(),
                message,
                model: self.config.backend.default_model.clone(),
                thinking_mode: ThinkingMode::Balanced,
            },
        )
        .await?;
        self.selected_thread_id = Some(thread_id.clone());
        self.composer.clear();
        self.focus = FocusMode::Browse;
        self.refresh()?;
        self.status = format!("thread `{thread_id}` 更新完成：{}", outcome.run.id);
        Ok(())
    }

    pub(crate) async fn approve_first_pending(&mut self) -> Result<()> {
        let Some(approval) = self.approvals.first().cloned() else {
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
        let Some(approval) = self.approvals.first().cloned() else {
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
        Ok(())
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

    pub(crate) fn cancel_selected_run(&mut self) -> Result<()> {
        let Some(thread_id) = self.selected_thread_id.clone() else {
            self.status = "当前没有选中 thread".to_string();
            return Ok(());
        };
        let Some(run_id) = self.selected_run_id.clone() else {
            self.status = "当前没有选中 run".to_string();
            return Ok(());
        };
        let run = cancel_active_run(&self.repo_root, &thread_id, &run_id)?;
        self.refresh()?;
        self.status = format!("已取消 run `{}`：{}", run.id, status_label(run.status));
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
}
