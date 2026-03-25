use crate::harness::{
    ApprovalStatus, ArtifactKind, HarnessEvent, HarnessMessageRole, HarnessRunStatus,
};

pub fn describe_event(event: &HarnessEvent) -> String {
    match event {
        HarnessEvent::RunCreated { run_id, .. } => format!("创建 run `{run_id}`"),
        HarnessEvent::SandboxReady {
            image,
            container_name,
            ..
        } => format!("Docker 沙箱已启动：{container_name} ({image})"),
        HarnessEvent::RunStarted { run_id, .. } => format!("启动 run `{run_id}`"),
        HarnessEvent::BackendTurnCompleted { turn, .. } => format!("完成第 {turn} 个 turn"),
        HarnessEvent::MessageAppended {
            role, message_id, ..
        } => {
            format!("写入消息 `{message_id}`，角色={}", role_label(*role))
        }
        HarnessEvent::ToolCallPlanned {
            tool_call_id,
            tool_name,
            ..
        } => format!("计划工具 `{tool_name}`（{tool_call_id}）"),
        HarnessEvent::ToolCallCompleted {
            tool_call_id,
            status,
            ..
        } => format!(
            "工具 `{tool_call_id}` 执行结束：{}",
            tool_status_label(*status)
        ),
        HarnessEvent::ApprovalRequested {
            approval_id,
            tool_name,
            ..
        } => format!("等待审批 `{approval_id}`：{tool_name}"),
        HarnessEvent::ApprovalResolved {
            approval_id,
            status,
            ..
        } => format!(
            "审批 `{approval_id}` 已处理：{}",
            approval_status_label(*status)
        ),
        HarnessEvent::ArtifactCreated {
            artifact_id, label, ..
        } => {
            format!("生成 artifact `{artifact_id}`：{label}")
        }
        HarnessEvent::SubagentStarted {
            subagent_id, kind, ..
        } => format!("子代理 `{subagent_id}` 已启动：{:?}", kind),
        HarnessEvent::SubagentCompleted {
            subagent_id,
            status,
            ..
        } => format!("子代理 `{subagent_id}` 已结束：{}", status_label(*status)),
        HarnessEvent::RunCompleted { run_id, .. } => format!("run `{run_id}` 已完成"),
        HarnessEvent::RunFailed { run_id, error, .. } => {
            format!("run `{run_id}` 失败：{error}")
        }
    }
}

pub fn role_label(role: HarnessMessageRole) -> &'static str {
    match role {
        HarnessMessageRole::User => "user",
        HarnessMessageRole::Assistant => "assistant",
        HarnessMessageRole::System => "system",
        HarnessMessageRole::Tool => "tool",
        HarnessMessageRole::Summary => "summary",
    }
}

pub fn status_label(status: HarnessRunStatus) -> &'static str {
    match status {
        HarnessRunStatus::Pending => "pending",
        HarnessRunStatus::Running => "running",
        HarnessRunStatus::WaitingForInput => "waiting_for_input",
        HarnessRunStatus::Completed => "completed",
        HarnessRunStatus::Failed => "failed",
        HarnessRunStatus::Cancelled => "cancelled",
    }
}

pub fn tool_status_label(status: crate::harness::types::ToolCallStatus) -> &'static str {
    match status {
        crate::harness::types::ToolCallStatus::Pending => "pending",
        crate::harness::types::ToolCallStatus::PendingApproval => "pending_approval",
        crate::harness::types::ToolCallStatus::Running => "running",
        crate::harness::types::ToolCallStatus::Succeeded => "succeeded",
        crate::harness::types::ToolCallStatus::Failed => "failed",
        crate::harness::types::ToolCallStatus::Skipped => "skipped",
    }
}

pub fn approval_status_label(status: ApprovalStatus) -> &'static str {
    match status {
        ApprovalStatus::Pending => "pending",
        ApprovalStatus::Approved => "approved",
        ApprovalStatus::Denied => "denied",
    }
}

pub fn artifact_kind_label(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Text => "text",
        ArtifactKind::File => "file",
        ArtifactKind::ToolResult => "tool_result",
        ArtifactKind::SandboxLog => "sandbox_log",
        ArtifactKind::SandboxSnapshot => "sandbox_snapshot",
    }
}

pub fn first_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("空")
}
