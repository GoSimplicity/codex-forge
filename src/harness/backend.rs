use std::path::Path;

use anyhow::{Result, bail};
use crate::codex::{ensure_codex_available, run_text_once};
use crate::model::ThinkingMode;

use super::types::{
    BackendSubagentCall, HarnessMessage, HarnessMessageRole, HarnessThreadManifest, TurnEnvelope,
};

#[derive(Debug, Clone)]
pub struct BackendTurnRequest<'a> {
    pub thread: &'a HarnessThreadManifest,
    pub messages: &'a [HarnessMessage],
    pub thinking_mode: ThinkingMode,
    pub model: Option<&'a str>,
    pub tools: &'a [ToolDescriptor],
    pub system_hint: &'a str,
}

#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub requires_approval: bool,
}

pub trait AgentBackend {
    fn render_prompt(&self, request: &BackendTurnRequest<'_>) -> String;
    async fn execute_turn(
        &self,
        repo_root: &Path,
        request: &BackendTurnRequest<'_>,
        output_path: &Path,
        log_path: &Path,
    ) -> Result<TurnEnvelope>;
}

#[derive(Debug, Default)]
pub struct CodexBackend;

impl AgentBackend for CodexBackend {
    fn render_prompt(&self, request: &BackendTurnRequest<'_>) -> String {
        render_lead_turn_prompt(request)
    }

    async fn execute_turn(
        &self,
        repo_root: &Path,
        request: &BackendTurnRequest<'_>,
        output_path: &Path,
        log_path: &Path,
    ) -> Result<TurnEnvelope> {
        ensure_codex_available()?;
        let prompt = self.render_prompt(request);
        let raw = run_text_once(
            &prompt,
            repo_root,
            request.model,
            request.thinking_mode,
            output_path,
            log_path,
            1,
        )
        .await?;
        parse_turn_envelope(&raw)
    }
}

pub fn render_lead_turn_prompt(request: &BackendTurnRequest<'_>) -> String {
    let mut rendered = String::new();
    rendered.push_str("你是 codex-forge 的主代理，运行在一个 thread/run harness 中。\n");
    rendered.push_str("你必须基于历史消息、工具能力和当前执行模式协作，默认使用中文，保持直接、务实。\n");
    rendered.push_str("你只能返回一个 JSON 对象，不要输出 Markdown，不要输出额外说明。\n");
    rendered.push_str("JSON 结构必须是：\n");
    rendered.push_str(
        r#"{"assistant_message":"给用户或工具前的说明","tool_calls":[{"name":"tool_name","arguments":{}}],"subagent_calls":[{"kind":"explorer|implementer|reviewer|tester","task":"..."}],"final_response":false}"#,
    );
    rendered.push_str("\n");
    rendered.push_str("规则：\n");
    rendered.push_str("- 如果你已经能直接回答用户，就设置 final_response=true，并在 assistant_message 中给最终回复。\n");
    rendered.push_str("- 如果需要查文件、搜索、列目录、执行命令、写文件，就使用 tool_calls。\n");
    rendered.push_str("- 高风险工具会被要求审批，你不需要自己模拟审批结果。\n");
    rendered.push_str("- subagent_calls 只在确实需要分工时使用；避免无限派生。\n");
    rendered.push_str("- 不要返回未知工具名。\n\n");
    rendered.push_str(&format!("线程标题：{}\n", request.thread.title));
    rendered.push_str(&format!("仓库根目录：{}\n", request.thread.repo_root.display()));
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

pub fn parse_turn_envelope(raw: &str) -> Result<TurnEnvelope> {
    if let Ok(envelope) = serde_json::from_str::<TurnEnvelope>(raw) {
        return Ok(normalize_turn_envelope(envelope));
    }

    if let Some(json) = extract_json_object(raw)
        && let Ok(envelope) = serde_json::from_str::<TurnEnvelope>(&json)
    {
        return Ok(normalize_turn_envelope(envelope));
    }

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("backend 返回为空");
    }

    Ok(TurnEnvelope {
        assistant_message: Some(trimmed.to_string()),
        tool_calls: Vec::new(),
        subagent_calls: Vec::new(),
        final_response: true,
    })
}

fn normalize_turn_envelope(mut envelope: TurnEnvelope) -> TurnEnvelope {
    envelope.tool_calls.retain(|call| !call.name.trim().is_empty());
    envelope
        .subagent_calls
        .retain(|call: &BackendSubagentCall| !call.task.trim().is_empty());
    envelope.assistant_message = envelope
        .assistant_message
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    envelope
}

fn extract_json_object(text: &str) -> Option<String> {
    if let Some(block) = extract_fenced_json_block(text) {
        return Some(block);
    }

    let start = text.find('{')?;
    let slice = &text[start..];
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (index, ch) in slice.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(slice[..=index].trim().to_string());
                }
            }
            _ => {}
        }
    }

    None
}

fn extract_fenced_json_block(text: &str) -> Option<String> {
    let start = text.find("```json")?;
    let after = &text[start + "```json".len()..];
    let end = after.find("```")?;
    let block = after[..end].trim();
    if block.is_empty() {
        None
    } else {
        Some(block.to_string())
    }
}

pub fn built_in_tools() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: "list_tree",
            description: "列出仓库中的目录和文件概览",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "read_file",
            description: "读取单个文件内容",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "search_files",
            description: "按关键字搜索文件内容",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "run_shell",
            description: "在 Docker 沙箱中执行 shell 命令",
            requires_approval: true,
        },
        ToolDescriptor {
            name: "write_file",
            description: "在 Docker 工作区中写入文件",
            requires_approval: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::parse_turn_envelope;

    #[test]
    fn parses_raw_text_as_final_response() {
        let envelope = parse_turn_envelope("你好").expect("parse");
        assert!(envelope.final_response);
        assert_eq!(envelope.assistant_message.as_deref(), Some("你好"));
    }

    #[test]
    fn parses_json_object_inside_markdown() {
        let envelope = parse_turn_envelope(
            "```json\n{\"assistant_message\":\"先读文件\",\"tool_calls\":[{\"name\":\"read_file\",\"arguments\":{\"path\":\"README.md\"}}],\"final_response\":false}\n```",
        )
        .expect("parse");
        assert_eq!(envelope.tool_calls.len(), 1);
        assert!(!envelope.final_response);
    }
}
