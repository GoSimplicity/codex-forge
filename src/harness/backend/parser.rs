use anyhow::{Result, bail};

use crate::harness::types::{BackendSubagentCall, TurnEnvelope};

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
        state_update: None,
        selected_feature_id: None,
        evaluation: None,
        needs_handoff: false,
    })
}

fn normalize_turn_envelope(mut envelope: TurnEnvelope) -> TurnEnvelope {
    envelope
        .tool_calls
        .retain(|call| !call.name.trim().is_empty());
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
