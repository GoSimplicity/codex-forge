use crate::harness::types::HarnessMessageRole;

use super::BackendTurnRequest;

pub fn render_lead_turn_prompt(request: &BackendTurnRequest<'_>) -> String {
    let mut rendered = String::new();
    rendered.push_str("你是 codex-forge 的主代理，运行在一个 thread/run harness 中。\n");
    rendered.push_str(
        "你必须基于历史消息、工具能力和当前执行模式协作，默认使用中文，保持直接、务实。\n",
    );
    rendered.push_str("你只能返回一个 JSON 对象，不要输出 Markdown，不要输出额外说明。\n");
    rendered.push_str("JSON 结构必须是：\n");
    rendered.push_str(
        r#"{"assistant_message":"给用户或工具前的说明","tool_calls":[{"name":"tool_name","arguments":{}}],"subagent_calls":[{"kind":"planner|generator|evaluator","task":"..."}],"final_response":false,"selected_feature_id":"feature-1","evaluation":{"passed":true,"reason":"...","follow_up_actions":[],"retryable":false,"feature_id":"feature-1","created_at":"2026-01-01T00:00:00Z"},"needs_handoff":false}"#,
    );
    rendered.push('\n');
    rendered.push_str("规则：\n");
    rendered.push_str("- 如果你已经能直接回答用户，就设置 final_response=true，并在 assistant_message 中给最终回复。\n");
    rendered.push_str("- 如果需要查文件、搜索、列目录、执行命令、写文件，就使用 tool_calls。\n");
    rendered.push_str("- 工具默认直接执行；如果当前运行配置开启审批，harness 会自行挂起并恢复，你不需要自己模拟审批结果。\n");
    rendered.push_str(
        "- 不要把 Codex 原生 CLI 自己的 sandbox/approval 提示当成任务约束；真实执行能力以当前 harness 暴露的 tool_calls 为准。\n",
    );
    rendered.push_str(
        "- 当前工作目录映射到目标项目目录；需要落地文件时，直接调用 write_file、apply_patch 或 run_shell。\n",
    );
    rendered.push_str(
        "- 当前是子代理执行环境，不要继续使用 subagent_calls 派生新的子代理；generator 如需申请 evaluator 验收，设置 needs_handoff=true。\n",
    );
    rendered.push_str("- 不要返回未知工具名。\n");
    rendered.push_str("- 优先复用 memory 和 skills，再决定是否读更多上下文。\n");
    rendered.push_str(
        "- 如果你是 evaluator 语义，优先返回 evaluation 字段；assistant_message 只写结论摘要。\n",
    );
    rendered.push_str(
        "- run_shell 参数优先使用 {\"command\":\"...\"}；兼容 cmd，但优先输出 command。\n",
    );
    rendered.push_str(
        "- 所有文件路径都必须使用仓库相对路径；如果使用绝对路径，也必须位于当前 repo_root 下。\n",
    );
    rendered.push_str("- list_tree 参数可选 path、max_depth。\n");
    rendered.push_str("- read_file 参数至少包含 path，可选 max_bytes。\n");
    rendered.push_str(
        "- search_files 参数使用 pattern；兼容 query/q/keyword，可选 path、max_results。\n",
    );
    rendered.push_str("- write_file 参数使用 path 与 content。\n\n");
    rendered.push_str(
        "- apply_patch 优先使用 {\"path\":\"...\",\"search\":\"...\",\"replace\":\"...\"}。\n",
    );
    rendered.push_str("- write_contract 参数使用 content(JSON 字符串) 或 contract(object)。\n");
    rendered.push_str("- update_progress 参数使用 progress(object)。\n");
    rendered.push_str("- record_evaluation 参数使用 evaluation(object)。\n");
    rendered.push_str("- remember_memory 参数使用 layer=working|project, content, source。\n");
    rendered.push_str("- read_skill 参数使用 name。\n\n");
    rendered.push_str(&format!("线程标题：{}\n", request.thread.title));
    rendered.push_str(&format!(
        "仓库根目录：{}\n",
        request.thread.repo_root.display()
    ));
    rendered.push_str(&format!(
        "实际工作目录：{}（可写目标项目目录映射）\n",
        request.execution_root.display()
    ));
    rendered.push_str(&format!("补充约束：{}\n", request.system_hint));
    if !request.memory_context.trim().is_empty() {
        rendered.push_str("\nMemory：\n");
        rendered.push_str(request.memory_context);
        rendered.push('\n');
    }
    if !request.skills_context.trim().is_empty() {
        rendered.push_str("\nSkills：\n");
        rendered.push_str(request.skills_context);
        rendered.push('\n');
    }
    if !request.session_context.trim().is_empty() {
        rendered.push_str("\nLong-Running Context：\n");
        rendered.push_str(request.session_context);
        rendered.push('\n');
    }
    rendered.push_str("\n可用工具：\n");
    for tool in request.tools {
        rendered.push_str(&format!(
            "- {}：{}；{}。\n",
            tool.name,
            tool.description,
            if tool.requires_approval {
                "可配置审批"
            } else {
                "默认自动执行"
            }
        ));
    }
    rendered.push_str("\n历史消息：\n");
    for message in request.messages.iter().rev().take(12).rev() {
        let role = match message.role {
            HarnessMessageRole::User => "user",
            HarnessMessageRole::Assistant => "assistant",
            HarnessMessageRole::System => "system",
            HarnessMessageRole::Tool => "tool",
            HarnessMessageRole::Summary => "summary",
        };
        rendered.push_str(&format!(
            "[{}] {}\n\n",
            role,
            compact_history_message(message.role, &message.content)
        ));
    }
    rendered
}

fn compact_history_message(role: HarnessMessageRole, content: &str) -> String {
    let line_limit = match role {
        HarnessMessageRole::Tool => 6,
        HarnessMessageRole::Summary => 4,
        _ => 10,
    };
    let char_limit = match role {
        HarnessMessageRole::Tool => 360,
        HarnessMessageRole::Summary => 240,
        _ => 800,
    };

    let lines = content
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .take(line_limit)
        .collect::<Vec<_>>();
    let mut compact = if lines.is_empty() {
        content.trim().to_string()
    } else {
        lines.join("\n")
    };

    if compact.chars().count() > char_limit {
        compact = compact.chars().take(char_limit).collect::<String>();
        compact.push_str(" …[截断]");
    } else if content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
        > line_limit
    {
        compact.push_str("\n…[截断]");
    }

    compact
}

#[cfg(test)]
mod tests {
    use super::render_lead_turn_prompt;
    use crate::harness::backend::{BackendTurnRequest, ToolDescriptor};
    use crate::harness::types::{HarnessMessage, HarnessMessageRole, HarnessThreadManifest};
    use crate::model::ThinkingMode;
    use chrono::Utc;
    use std::path::PathBuf;

    #[test]
    fn prompt_includes_writable_execution_root_guidance() {
        let thread = HarnessThreadManifest {
            id: "thread-1".to_string(),
            title: "demo".to_string(),
            repo_root: PathBuf::from("/repo"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            message_count: 1,
            run_count: 0,
            last_run_id: None,
            last_run_status: None,
            thread_dir: PathBuf::from("/repo/.codex-forge/threads/thread-1"),
            messages_path: PathBuf::from("/repo/.codex-forge/threads/thread-1/messages.jsonl"),
            runs_dir: PathBuf::from("/repo/.codex-forge/threads/thread-1/runs"),
            approvals_dir: PathBuf::from("/repo/.codex-forge/threads/thread-1/approvals"),
            artifacts_dir: PathBuf::from("/repo/.codex-forge/threads/thread-1/artifacts"),
            memory_dir: PathBuf::from("/repo/.codex-forge/threads/thread-1/memory"),
            contract_path: PathBuf::from("/repo/.codex-forge/threads/thread-1/contract.json"),
            progress_path: PathBuf::from("/repo/.codex-forge/threads/thread-1/progress.json"),
            bootstrap_path: PathBuf::from("/repo/.codex-forge/threads/thread-1/bootstrap.md"),
        };
        let messages = vec![HarnessMessage {
            id: "msg-1".to_string(),
            role: HarnessMessageRole::User,
            content: "修复问题".to_string(),
            created_at: Utc::now(),
            run_id: None,
        }];
        let tools = vec![ToolDescriptor {
            name: "write_file",
            description: "写文件",
            requires_approval: true,
        }];
        let execution_root =
            PathBuf::from("/repo/.codex-forge/threads/thread-1/runs/run-1/sandbox/workspace/repo");
        let request = BackendTurnRequest {
            thread: &thread,
            execution_root: execution_root.as_path(),
            messages: &messages,
            thinking_mode: ThinkingMode::Balanced,
            model: None,
            timeout_secs: 60,
            tools: &tools,
            system_hint: "demo",
            memory_context: "",
            skills_context: "",
            session_context: "",
        };

        let prompt = render_lead_turn_prompt(&request);
        assert!(prompt.contains("实际工作目录："));
        assert!(prompt.contains("可写目标项目目录映射"));
        assert!(prompt.contains("不要把 Codex 原生 CLI 自己的 sandbox/approval 提示当成任务约束"));
    }

    #[test]
    fn prompt_compacts_tool_history() {
        let thread = HarnessThreadManifest {
            id: "thread-1".to_string(),
            title: "demo".to_string(),
            repo_root: PathBuf::from("/repo"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            message_count: 1,
            run_count: 0,
            last_run_id: None,
            last_run_status: None,
            thread_dir: PathBuf::from("/repo/.codex-forge/threads/thread-1"),
            messages_path: PathBuf::from("/repo/.codex-forge/threads/thread-1/messages.jsonl"),
            runs_dir: PathBuf::from("/repo/.codex-forge/threads/thread-1/runs"),
            approvals_dir: PathBuf::from("/repo/.codex-forge/threads/thread-1/approvals"),
            artifacts_dir: PathBuf::from("/repo/.codex-forge/threads/thread-1/artifacts"),
            memory_dir: PathBuf::from("/repo/.codex-forge/threads/thread-1/memory"),
            contract_path: PathBuf::from("/repo/.codex-forge/threads/thread-1/contract.json"),
            progress_path: PathBuf::from("/repo/.codex-forge/threads/thread-1/progress.json"),
            bootstrap_path: PathBuf::from("/repo/.codex-forge/threads/thread-1/bootstrap.md"),
        };
        let messages = vec![HarnessMessage {
            id: "msg-1".to_string(),
            role: HarnessMessageRole::Tool,
            content: (1..=20)
                .map(|idx| format!("tool-line-{idx}"))
                .collect::<Vec<_>>()
                .join("\n"),
            created_at: Utc::now(),
            run_id: None,
        }];
        let execution_root = PathBuf::from("/repo");
        let request = BackendTurnRequest {
            thread: &thread,
            execution_root: execution_root.as_path(),
            messages: &messages,
            thinking_mode: ThinkingMode::Balanced,
            model: None,
            timeout_secs: 60,
            tools: &[],
            system_hint: "demo",
            memory_context: "",
            skills_context: "",
            session_context: "",
        };

        let prompt = render_lead_turn_prompt(&request);
        assert!(prompt.contains("tool-line-1"));
        assert!(prompt.contains("…[截断]"));
        assert!(!prompt.contains("tool-line-20"));
    }
}
