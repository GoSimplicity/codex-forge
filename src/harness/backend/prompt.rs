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
        r#"{"assistant_message":"给用户或工具前的说明","tool_calls":[{"name":"tool_name","arguments":{}}],"subagent_calls":[{"kind":"explorer|implementer|reviewer|tester","task":"..."}],"final_response":false}"#,
    );
    rendered.push('\n');
    rendered.push_str("规则：\n");
    rendered.push_str("- 如果你已经能直接回答用户，就设置 final_response=true，并在 assistant_message 中给最终回复。\n");
    rendered.push_str("- 如果需要查文件、搜索、列目录、执行命令、写文件，就使用 tool_calls。\n");
    rendered.push_str("- 高风险工具会被要求审批，你不需要自己模拟审批结果。\n");
    rendered.push_str("- subagent_calls 只在确实需要分工时使用；避免无限派生。\n");
    rendered.push_str("- 不要返回未知工具名。\n");
    rendered.push_str(
        "- run_shell 参数优先使用 {\"command\":\"...\"}；兼容 cmd，但优先输出 command。\n",
    );
    rendered.push_str("- read_file 参数至少包含 path，可选 max_bytes。\n");
    rendered.push_str("- write_file 参数使用 path 与 content。\n\n");
    rendered.push_str(&format!("线程标题：{}\n", request.thread.title));
    rendered.push_str(&format!(
        "仓库根目录：{}\n",
        request.thread.repo_root.display()
    ));
    rendered.push_str(&format!("补充约束：{}\n", request.system_hint));
    rendered.push_str("\n可用工具：\n");
    for tool in request.tools {
        rendered.push_str(&format!(
            "- {}：{}；{}。\n",
            tool.name,
            tool.description,
            if tool.requires_approval {
                "默认需要审批"
            } else {
                "默认自动执行"
            }
        ));
    }
    rendered.push_str("\n历史消息：\n");
    for message in request.messages.iter().rev().take(16).rev() {
        let role = match message.role {
            HarnessMessageRole::User => "user",
            HarnessMessageRole::Assistant => "assistant",
            HarnessMessageRole::System => "system",
            HarnessMessageRole::Tool => "tool",
            HarnessMessageRole::Summary => "summary",
        };
        rendered.push_str(&format!("[{}] {}\n\n", role, message.content.trim()));
    }
    rendered
}
