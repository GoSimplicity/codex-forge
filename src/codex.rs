use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::process::Command;
use tokio::time::{sleep, timeout};

use crate::model::ThinkingMode;

pub fn ensure_codex_available() -> Result<String> {
    let path = which::which("codex").context("未找到 `codex` 命令，请先确认 Codex CLI 已安装")?;
    Ok(path.display().to_string())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_text_once(
    prompt: &str,
    cwd: &Path,
    model: Option<&str>,
    thinking_mode: ThinkingMode,
    timeout_secs: u64,
    output_path: &Path,
    log_path: &Path,
    max_retries: usize,
) -> Result<String> {
    let attempts = max_retries.max(1);
    let mut last_error = None;

    for attempt in 1..=attempts {
        match run_text_attempt(
            prompt,
            cwd,
            model,
            thinking_mode,
            timeout_secs,
            output_path,
            log_path,
        )
        .await
        {
            Ok(content) => return Ok(content),
            Err(error) => {
                append_text(
                    log_path,
                    &format!("\n[attempt {attempt}/{attempts}] text call failed: {error}\n"),
                )?;
                last_error = Some(error);
                if attempt == attempts {
                    break;
                }
                sleep(backoff_for(attempt)).await;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("未知文本调用失败")))
}

async fn run_text_attempt(
    prompt: &str,
    cwd: &Path,
    model: Option<&str>,
    thinking_mode: ThinkingMode,
    timeout_secs: u64,
    output_path: &Path,
    log_path: &Path,
) -> Result<String> {
    let mut command = Command::new("codex");
    configure_codex_command(&mut command, thinking_mode);
    command
        .arg("exec")
        .arg("--skip-git-repo-check")
        .arg("--ephemeral")
        .arg("--color")
        .arg("never")
        .arg("-C")
        .arg(cwd)
        .arg("-o")
        .arg(output_path);

    if let Some(model) = model {
        command.arg("-m").arg(model);
    }

    command
        .kill_on_drop(true)
        .arg(prompt)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = timeout(Duration::from_secs(timeout_secs.max(1)), command.output())
        .await
        .map_err(|_| anyhow!("Codex 文本调用超时（>{timeout_secs}s）"))?
        .context("执行 codex exec 文本调用失败")?;

    let mut combined = Vec::new();
    combined.extend_from_slice(&output.stdout);
    combined.extend_from_slice(b"\n--- STDERR ---\n");
    combined.extend_from_slice(&output.stderr);
    fs::write(log_path, combined)
        .with_context(|| format!("写入 Codex 文本调用日志失败：{}", log_path.display()))?;

    if !output.status.success() {
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{}",
            format_codex_failure("Codex 文本调用失败", &stderr_text)
        );
    }

    if !output_path.exists() {
        bail!(
            "Codex 调用成功但没有生成结果文件：{}",
            output_path.display()
        );
    }

    fs::read_to_string(output_path)
        .with_context(|| format!("读取文本输出失败：{}", output_path.display()))
}

fn append_text(path: &Path, text: &str) -> Result<()> {
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
        .with_context(|| format!("追加日志失败：{}", path.display()))
}

fn configure_codex_command(command: &mut Command, thinking_mode: ThinkingMode) {
    command.env("OTEL_SDK_DISABLED", "true");
    command.arg("-c").arg(format!(
        "model_reasoning_effort=\"{}\"",
        thinking_mode.codex_reasoning_effort()
    ));
}

fn format_codex_failure(prefix: &str, stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}：{trimmed}")
    }
}

fn backoff_for(attempt: usize) -> Duration {
    let millis = 300u64.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1) as u32));
    Duration::from_millis(millis.min(2_400))
}
