use serde_json::{Map, Value};

use crate::harness::types::ToolCallRequest;

pub fn canonical_tool_name(name: &str) -> &str {
    match name {
        "exec_command" => "run_shell",
        other => other,
    }
}

pub fn normalize_tool_call(call: &ToolCallRequest) -> ToolCallRequest {
    let mut normalized = call.clone();
    normalized.name = canonical_tool_name(&call.name).to_string();

    if normalized.name == "run_shell"
        && normalized.arguments.get("command").is_none()
        && let Some(cmd) = normalized.arguments.get("cmd").cloned()
    {
        match normalized.arguments.as_object_mut() {
            Some(arguments) => {
                arguments.insert("command".to_string(), cmd);
            }
            None => {
                let mut arguments = Map::new();
                arguments.insert("command".to_string(), cmd);
                normalized.arguments = Value::Object(arguments);
            }
        }
    }

    normalized
}

pub fn tool_requires_approval(name: &str) -> bool {
    matches!(
        canonical_tool_name(name),
        "run_shell" | "write_file" | "apply_patch"
    )
}

pub fn approval_reason(name: &str) -> &'static str {
    match canonical_tool_name(name) {
        "run_shell" => "执行 shell 命令会修改 Docker 沙箱内工作区或产生副作用",
        "write_file" => "写文件会修改 Docker 沙箱内工作区内容",
        "apply_patch" => "apply_patch 会按补丁方式修改 Docker 沙箱内工作区文件",
        _ => "该工具默认需要人工确认",
    }
}

#[cfg(test)]
mod tests {
    use crate::harness::types::ToolCallRequest;

    use super::{canonical_tool_name, normalize_tool_call, tool_requires_approval};

    #[test]
    fn exec_command_is_normalized_to_run_shell() {
        let call = ToolCallRequest {
            name: "exec_command".to_string(),
            arguments: serde_json::json!({"cmd":"cat README.md","max_output_tokens":6000}),
        };

        let normalized = normalize_tool_call(&call);
        assert_eq!(normalized.name, "run_shell");
        assert_eq!(
            normalized
                .arguments
                .get("command")
                .and_then(|value| value.as_str()),
            Some("cat README.md")
        );
        assert!(tool_requires_approval(&normalized.name));
        assert_eq!(canonical_tool_name("exec_command"), "run_shell");
    }
}
