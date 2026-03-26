use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::config::BackendConfig;

use super::{AgentBackend, BackendTurnRequest, parse_turn_envelope, render_lead_turn_prompt};
use crate::harness::types::TurnEnvelope;

const OPENAI_COMPATIBLE_UNSUPPORTED_MODEL_STATUS: i64 = 2061;

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleBackend {
    config: BackendConfig,
    client: reqwest::Client,
}

impl OpenAiCompatibleBackend {
    pub fn new(config: BackendConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            client: reqwest::Client::builder()
                .build()
                .context("初始化 openai-compatible HTTP 客户端失败")?,
        })
    }

    fn endpoint(&self) -> String {
        chat_completions_endpoint(self.config.base_url.as_deref().unwrap_or_default())
    }

    fn resolved_model<'a>(&'a self, request: &'a BackendTurnRequest<'_>) -> Option<&'a str> {
        request.model.or(self.config.model.as_deref())
    }

    async fn request_completion(
        &self,
        endpoint: &str,
        key: &str,
        prompt: &str,
        model: &str,
        timeout_secs: u64,
        log_path: &Path,
    ) -> Result<Value> {
        let payload = json!({
            "model": model,
            "messages": [
                {
                    "role": "user",
                    "content": prompt,
                }
            ],
            "response_format": {
                "type": "json_object"
            }
        });

        append_log(
            log_path,
            &format!(
                "[request] POST {}\n{}\n",
                endpoint,
                serde_json::to_string_pretty(&payload).context("序列化 openai 请求体失败")?
            ),
        )?;

        let response = self
            .client
            .post(endpoint)
            .bearer_auth(key)
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("调用 openai-compatible 接口失败：{}", endpoint))?;

        let status = response.status();
        let raw = response
            .text()
            .await
            .context("读取 openai-compatible 响应失败")?;

        append_log(log_path, &format!("[response] status={status}\n{raw}\n"))?;

        if !status.is_success() {
            bail!(
                "openai-compatible 接口调用失败：HTTP {}{}",
                status.as_u16(),
                summarize_response_body(&raw)
            );
        }

        serde_json::from_str(&raw).context("解析 openai-compatible 响应 JSON 失败")
    }
}

impl AgentBackend for OpenAiCompatibleBackend {
    fn render_prompt(&self, request: &BackendTurnRequest<'_>) -> String {
        render_lead_turn_prompt(request)
    }

    async fn execute_turn(
        &self,
        _execution_root: &Path,
        request: &BackendTurnRequest<'_>,
        output_path: &Path,
        log_path: &Path,
    ) -> Result<TurnEnvelope> {
        let prompt = self.render_prompt(request);
        let model = self
            .resolved_model(request)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("openai-compatible backend 缺少可用 model"))?;
        let key = self
            .config
            .key
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("openai-compatible backend 缺少 key"))?;

        prepare_log_file(log_path)?;
        let endpoint = self.endpoint();
        let value = self
            .request_completion(
                &endpoint,
                key,
                &prompt,
                model,
                request.timeout_secs,
                log_path,
            )
            .await?;
        if let Some(api_error) = extract_api_error_details(&value) {
            if let Some(fallback_model) = fallback_model_for_unsupported_model(model, &api_error) {
                append_log(
                    log_path,
                    &format!(
                        "[fallback] 模型 `{}` 当前套餐不可用，自动回退到 `{}`\n",
                        model, fallback_model
                    ),
                )?;
                let fallback_value = self
                    .request_completion(
                        &endpoint,
                        key,
                        &prompt,
                        &fallback_model,
                        request.timeout_secs,
                        log_path,
                    )
                    .await?;
                if let Some(fallback_error) = extract_api_error(&fallback_value) {
                    bail!(
                        "openai-compatible 接口返回业务错误：主模型 `{}` 失败（{}）；回退模型 `{}` 仍失败（{}）",
                        model,
                        api_error.message,
                        fallback_model,
                        fallback_error
                    );
                }
                let content = extract_message_content(&fallback_value).ok_or_else(|| {
                    anyhow!("openai-compatible 响应缺少 choices[0].message.content")
                })?;
                fs::write(output_path, &content)
                    .with_context(|| format!("写入 backend 输出失败：{}", output_path.display()))?;
                return parse_turn_envelope(&content);
            }
            bail!("openai-compatible 接口返回业务错误：{}", api_error.message);
        }
        let content = extract_message_content(&value)
            .ok_or_else(|| anyhow!("openai-compatible 响应缺少 choices[0].message.content"))?;
        fs::write(output_path, &content)
            .with_context(|| format!("写入 backend 输出失败：{}", output_path.display()))?;
        parse_turn_envelope(&content)
    }
}

fn prepare_log_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建日志目录失败：{}", parent.display()))?;
    }
    fs::write(path, "").with_context(|| format!("初始化日志文件失败：{}", path.display()))
}

fn append_log(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建日志目录失败：{}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("打开日志文件失败：{}", path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("写入日志失败：{}", path.display()))
}

fn summarize_response_body(raw: &str) -> String {
    let line = raw
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    if line.is_empty() {
        String::new()
    } else {
        format!("，响应：{}", truncate_text(line, 200))
    }
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn extract_message_content(value: &Value) -> Option<String> {
    let content = value
        .get("choices")?
        .as_array()?
        .first()?
        .get("message")?
        .get("content")?;

    if let Some(text) = content.as_str() {
        let trimmed = text.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_string());
    }

    let parts = content.as_array()?;
    let mut text = String::new();
    for part in parts {
        if let Some(item) = part.get("text").and_then(Value::as_str) {
            text.push_str(item);
            continue;
        }
        if let Some(item) = part
            .get("content")
            .and_then(|value| value.get("text"))
            .and_then(Value::as_str)
        {
            text.push_str(item);
        }
    }
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn extract_api_error(value: &Value) -> Option<String> {
    extract_api_error_details(value).map(|details| details.message)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApiErrorDetails {
    message: String,
    status_code: Option<i64>,
    status_msg: Option<String>,
}

fn extract_api_error_details(value: &Value) -> Option<ApiErrorDetails> {
    if let Some(message) = value
        .get("error")
        .and_then(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
        })
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        return Some(ApiErrorDetails {
            message: message.to_string(),
            status_code: None,
            status_msg: None,
        });
    }

    let base_resp = value.get("base_resp")?;
    let status_code = base_resp
        .get("status_code")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    if status_code == 0 {
        return None;
    }

    let status_msg = base_resp
        .get("status_msg")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .unwrap_or("未知错误");

    Some(ApiErrorDetails {
        message: format!("status_code={status_code}, status_msg={status_msg}"),
        status_code: Some(status_code),
        status_msg: Some(status_msg.to_string()),
    })
}

fn fallback_model_for_unsupported_model(
    requested_model: &str,
    api_error: &ApiErrorDetails,
) -> Option<String> {
    let status_code = api_error.status_code?;
    if status_code != OPENAI_COMPATIBLE_UNSUPPORTED_MODEL_STATUS {
        return None;
    }
    let status_msg = api_error.status_msg.as_deref()?.to_ascii_lowercase();
    if !status_msg.contains("not support model") {
        return None;
    }
    let fallback = requested_model.strip_suffix("-highspeed")?.trim();
    (!fallback.is_empty() && fallback != requested_model).then(|| fallback.to_string())
}

fn chat_completions_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ApiErrorDetails, OPENAI_COMPATIBLE_UNSUPPORTED_MODEL_STATUS, chat_completions_endpoint,
        extract_api_error, extract_message_content, fallback_model_for_unsupported_model,
    };

    #[test]
    fn extracts_string_or_array_message_content() {
        let string_content = serde_json::json!({
            "choices": [{"message": {"content": "{\"final_response\":true}"}}]
        });
        assert_eq!(
            extract_message_content(&string_content).as_deref(),
            Some("{\"final_response\":true}")
        );

        let array_content = serde_json::json!({
            "choices": [{"message": {"content": [{"type":"text","text":"{\"assistant_message\":\"ok\",\"tool_calls\":[],\"subagent_calls\":[],\"final_response\":true}"}]}}]
        });
        assert!(
            extract_message_content(&array_content)
                .unwrap()
                .contains("\"final_response\":true")
        );
    }

    #[test]
    fn endpoint_builder_accepts_root_or_full_endpoint() {
        assert_eq!(
            chat_completions_endpoint("https://example.com/v1"),
            "https://example.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_endpoint("https://example.com/v1/"),
            "https://example.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_endpoint("https://example.com/v1/chat/completions"),
            "https://example.com/v1/chat/completions"
        );
    }

    #[test]
    fn extracts_error_message_from_standard_error_object() {
        let body = serde_json::json!({
            "error": {"message": "invalid api key"}
        });
        assert_eq!(extract_api_error(&body).as_deref(), Some("invalid api key"));
    }

    #[test]
    fn extracts_error_message_from_base_resp() {
        let body = serde_json::json!({
            "base_resp": {
                "status_code": 2061,
                "status_msg": "model not supported"
            },
            "choices": null
        });
        assert_eq!(
            extract_api_error(&body).as_deref(),
            Some("status_code=2061, status_msg=model not supported")
        );
    }

    #[test]
    fn falls_back_from_highspeed_model_when_plan_does_not_support_it() {
        let error = ApiErrorDetails {
            message: "status_code=2061, status_msg=your current token plan not support model, MiniMax-M2.7-highspeed".to_string(),
            status_code: Some(OPENAI_COMPATIBLE_UNSUPPORTED_MODEL_STATUS),
            status_msg: Some(
                "your current token plan not support model, MiniMax-M2.7-highspeed".to_string(),
            ),
        };

        assert_eq!(
            fallback_model_for_unsupported_model("MiniMax-M2.7-highspeed", &error).as_deref(),
            Some("MiniMax-M2.7")
        );
    }

    #[test]
    fn does_not_fall_back_for_non_highspeed_model() {
        let error = ApiErrorDetails {
            message: "status_code=2061, status_msg=your current token plan not support model, MiniMax-M2.7".to_string(),
            status_code: Some(OPENAI_COMPATIBLE_UNSUPPORTED_MODEL_STATUS),
            status_msg: Some("your current token plan not support model, MiniMax-M2.7".to_string()),
        };

        assert_eq!(
            fallback_model_for_unsupported_model("MiniMax-M2.7", &error),
            None
        );
    }
}
