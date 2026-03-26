use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::fs::OpenOptions as TokioOpenOptions;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

use crate::harness::types::TurnEnvelope;
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

#[allow(clippy::too_many_arguments)]
pub async fn run_plain_once(
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
        match run_plain_attempt(
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
                    &format!("\n[attempt {attempt}/{attempts}] plain call failed: {error}\n"),
                )?;
                last_error = Some(error);
                if attempt == attempts {
                    break;
                }
                sleep(backoff_for(attempt)).await;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("未知 Codex 调用失败")))
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

    prepare_log_file(log_path)?;
    let mut child = command.spawn().context("执行 codex exec 文本调用失败")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Codex stdout 未成功捕获"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Codex stderr 未成功捕获"))?;

    let (tx, rx) = mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(read_stream(stdout, StreamSource::Stdout, tx.clone()));
    let stderr_task = tokio::spawn(read_stream(stderr, StreamSource::Stderr, tx.clone()));
    drop(tx);
    let logger_task = tokio::spawn(write_stream_log(log_path.to_path_buf(), rx));

    let status = match timeout(Duration::from_secs(timeout_secs.max(1)), child.wait()).await {
        Ok(result) => result.context("等待 codex exec 结束失败")?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            let captured = logger_task.await.context("写入实时日志任务失败")??;
            return recover_timeout_output(output_path, log_path, timeout_secs, &captured.stderr);
        }
    };

    stdout_task.await.context("读取 Codex stdout 任务失败")??;
    stderr_task.await.context("读取 Codex stderr 任务失败")??;
    let captured = logger_task.await.context("写入实时日志任务失败")??;

    if !status.success() {
        bail!(
            "{}",
            format_codex_failure("Codex 文本调用失败", &captured.stderr)
        );
    }

    read_output_file(output_path)
}

async fn run_plain_attempt(
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

    prepare_log_file(log_path)?;
    let mut child = command.spawn().context("执行 codex exec 自主调用失败")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Codex stdout 未成功捕获"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Codex stderr 未成功捕获"))?;

    let (tx, rx) = mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(read_stream(stdout, StreamSource::Stdout, tx.clone()));
    let stderr_task = tokio::spawn(read_stream(stderr, StreamSource::Stderr, tx.clone()));
    drop(tx);
    let logger_task = tokio::spawn(write_stream_log(log_path.to_path_buf(), rx));

    let status = match timeout(Duration::from_secs(timeout_secs.max(1)), child.wait()).await {
        Ok(result) => result.context("等待 codex exec 结束失败")?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            let captured = logger_task.await.context("写入实时日志任务失败")??;
            return recover_timeout_plain_output(
                output_path,
                log_path,
                timeout_secs,
                &captured.stderr,
            );
        }
    };

    stdout_task.await.context("读取 Codex stdout 任务失败")??;
    stderr_task.await.context("读取 Codex stderr 任务失败")??;
    let captured = logger_task.await.context("写入实时日志任务失败")??;

    if !status.success() {
        bail!(
            "{}",
            format_codex_failure("Codex 自主执行失败", &captured.stderr)
        );
    }

    read_output_file(output_path)
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

fn prepare_log_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建日志目录失败：{}", parent.display()))?;
    }
    fs::write(path, "").with_context(|| format!("初始化日志文件失败：{}", path.display()))
}

fn read_output_file(output_path: &Path) -> Result<String> {
    if !output_path.exists() {
        bail!(
            "Codex 调用成功但没有生成结果文件：{}",
            output_path.display()
        );
    }
    let content = fs::read_to_string(output_path)
        .with_context(|| format!("读取文本输出失败：{}", output_path.display()))?;
    if content.trim().is_empty() {
        bail!("Codex 生成了空结果文件：{}", output_path.display());
    }
    Ok(content)
}

fn recover_timeout_output(
    output_path: &Path,
    log_path: &Path,
    timeout_secs: u64,
    stderr: &str,
) -> Result<String> {
    let content = match fs::read_to_string(output_path) {
        Ok(content) if !content.trim().is_empty() => content,
        _ => {
            bail!(
                "{}",
                format_codex_failure(
                    &format!("Codex 文本调用超时（>{timeout_secs}s），且没有可恢复输出"),
                    stderr
                )
            );
        }
    };

    if !looks_like_turn_envelope(&content) {
        bail!(
            "{}",
            format_codex_failure(
                &format!(
                    "Codex 文本调用超时（>{timeout_secs}s），且结果文件不是合法 turn envelope"
                ),
                stderr
            )
        );
    }

    append_text(
        log_path,
        &format!(
            "\n[timeout recovery] Codex 文本调用超时（>{timeout_secs}s），但已从结果文件恢复结构化输出：{}\n",
            output_path.display()
        ),
    )?;
    Ok(content)
}

fn recover_timeout_plain_output(
    output_path: &Path,
    log_path: &Path,
    timeout_secs: u64,
    stderr: &str,
) -> Result<String> {
    let content = match fs::read_to_string(output_path) {
        Ok(content) if !content.trim().is_empty() => content,
        _ => {
            bail!(
                "{}",
                format_codex_failure(
                    &format!("Codex 自主执行超时（>{timeout_secs}s），且没有可恢复输出"),
                    stderr
                )
            );
        }
    };

    append_text(
        log_path,
        &format!(
            "\n[timeout recovery] Codex 自主执行超时（>{timeout_secs}s），但已从结果文件恢复输出：{}\n",
            output_path.display()
        ),
    )?;
    Ok(content)
}

fn looks_like_turn_envelope(content: &str) -> bool {
    let trimmed = content.trim();
    trimmed.starts_with('{') && serde_json::from_str::<TurnEnvelope>(trimmed).is_ok()
}

fn configure_codex_command(command: &mut Command, thinking_mode: ThinkingMode) {
    command.env("OTEL_SDK_DISABLED", "true");
    command.arg("-s").arg("workspace-write");
    command.arg("-a").arg("never");
    command.arg("-c").arg(format!(
        "model_reasoning_effort=\"{}\"",
        thinking_mode.codex_reasoning_effort()
    ));
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Mutex;

    use super::{
        configure_codex_command, format_codex_failure, looks_like_turn_envelope, run_text_once,
    };
    use crate::model::ThinkingMode;
    use once_cell::sync::Lazy;
    use tempfile::TempDir;
    use tokio::process::Command;

    static PATH_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    #[test]
    fn codex_command_uses_workspace_write_and_never_approval() {
        let mut command = Command::new("codex");
        configure_codex_command(&mut command, ThinkingMode::Balanced);
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();

        assert!(
            args.windows(2)
                .any(|pair| pair == ["-s", "workspace-write"])
        );
        assert!(args.windows(2).any(|pair| pair == ["-a", "never"]));
        assert!(args.iter().any(|arg| {
            arg == "model_reasoning_effort=\"medium\""
                || arg == "model_reasoning_effort=\"balanced\""
        }));
    }

    #[test]
    fn looks_like_turn_envelope_rejects_plain_text() {
        assert!(looks_like_turn_envelope(
            r#"{"assistant_message":"ok","tool_calls":[],"subagent_calls":[],"final_response":true}"#
        ));
        assert!(!looks_like_turn_envelope("普通文本回复"));
    }

    #[test]
    fn format_codex_failure_filters_known_startup_noise() {
        let stderr = "mcp startup: no servers\nwarning: Model metadata for `MiniMax-M2.7` not found.Defaulting to fallback metadata; this can degrade performance\nreal failure";
        assert_eq!(
            format_codex_failure("Codex 文本调用失败", stderr),
            "Codex 文本调用失败：real failure"
        );
    }

    #[tokio::test]
    async fn timeout_recovers_existing_structured_output() {
        let _guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = TempDir::new().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("mkdir");
        write_fake_codex(
            &bin_dir,
            r#"#!/bin/sh
set -eu
output=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    exec) shift ;;
    -C) shift 2 ;;
    -o) output="$2"; shift 2 ;;
    -m|-c) shift 2 ;;
    --skip-git-repo-check|--ephemeral) shift ;;
    --color) shift 2 ;;
    -s|-a) shift 2 ;;
    *) shift ;;
  esac
done
cat > "$output" <<'JSON'
{"assistant_message":"recover-ok","tool_calls":[],"subagent_calls":[],"final_response":true}
JSON
sleep 2
"#,
        );

        let output_path = dir.path().join("assistant.md");
        let log_path = dir.path().join("codex.log");
        let path_guard = set_fake_path(&bin_dir);

        let result = run_text_once(
            "demo",
            dir.path(),
            None,
            ThinkingMode::Balanced,
            1,
            &output_path,
            &log_path,
            1,
        )
        .await
        .expect("recover output");

        assert!(result.contains("recover-ok"));
        let log = fs::read_to_string(&log_path).expect("read log");
        assert!(log.contains("timeout recovery"));

        drop(path_guard);
    }

    #[tokio::test]
    async fn timeout_without_output_is_reported_clearly() {
        let _guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = TempDir::new().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("mkdir");
        write_fake_codex(
            &bin_dir,
            r#"#!/bin/sh
set -eu
sleep 2
"#,
        );

        let output_path = dir.path().join("assistant.md");
        let log_path = dir.path().join("codex.log");
        let path_guard = set_fake_path(&bin_dir);

        let error = run_text_once(
            "demo",
            dir.path(),
            None,
            ThinkingMode::Balanced,
            1,
            &output_path,
            &log_path,
            1,
        )
        .await
        .expect_err("should timeout");

        assert!(error.to_string().contains("且没有可恢复输出"));

        drop(path_guard);
    }

    #[tokio::test]
    async fn transient_failure_is_retried_before_returning() {
        let _guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = TempDir::new().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("mkdir");
        write_fake_codex(
            &bin_dir,
            r#"#!/bin/sh
set -eu
output=""
state_file="${CODEX_FAKE_STATE_FILE:-}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    exec) shift ;;
    -C) shift 2 ;;
    -o) output="$2"; shift 2 ;;
    -m|-c) shift 2 ;;
    --skip-git-repo-check|--ephemeral) shift ;;
    --color) shift 2 ;;
    -s|-a) shift 2 ;;
    *) shift ;;
  esac
done
if [ ! -f "$state_file" ]; then
  printf first > "$state_file"
  echo "temporary upstream failure" >&2
  exit 1
fi
cat > "$output" <<'JSON'
{"assistant_message":"retry-ok","tool_calls":[],"subagent_calls":[],"final_response":true}
JSON
"#,
        );

        let output_path = dir.path().join("assistant.md");
        let log_path = dir.path().join("codex.log");
        let state_path = dir.path().join("attempt-state");
        let path_guard = set_fake_path(&bin_dir);
        unsafe {
            std::env::set_var("CODEX_FAKE_STATE_FILE", &state_path);
        }

        let result = run_text_once(
            "demo",
            dir.path(),
            None,
            ThinkingMode::Balanced,
            5,
            &output_path,
            &log_path,
            2,
        )
        .await
        .expect("retry should recover");

        assert!(result.contains("retry-ok"));
        assert_eq!(
            fs::read_to_string(&state_path).expect("read state"),
            "first"
        );

        unsafe {
            std::env::remove_var("CODEX_FAKE_STATE_FILE");
        }
        drop(path_guard);
    }

    fn write_fake_codex(bin_dir: &Path, body: &str) {
        let path = bin_dir.join("codex");
        fs::write(&path, body).expect("write codex");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).expect("meta").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).expect("chmod");
        }
    }

    fn set_fake_path(bin_dir: &Path) -> PathGuard {
        let original = std::env::var_os("PATH");
        unsafe {
            std::env::set_var(
                "PATH",
                format!(
                    "{}:{}",
                    bin_dir.display(),
                    original
                        .as_deref()
                        .and_then(|value| value.to_str())
                        .unwrap_or("")
                ),
            );
        }
        PathGuard { original }
    }

    struct PathGuard {
        original: Option<std::ffi::OsString>,
    }

    impl Drop for PathGuard {
        fn drop(&mut self) {
            unsafe {
                match self.original.take() {
                    Some(path) => std::env::set_var("PATH", path),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
    }
}

fn format_codex_failure(prefix: &str, stderr: &str) -> String {
    let trimmed = sanitize_codex_stderr(stderr);
    if trimmed.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}：{trimmed}")
    }
}

fn sanitize_codex_stderr(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|line| !is_ignorable_codex_stderr_line(line))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn is_ignorable_codex_stderr_line(line: &str) -> bool {
    let lower = line.trim().to_lowercase();
    lower.starts_with("mcp startup: no servers")
        || (lower.contains("model metadata for")
            && lower.contains("defaulting to fallback metadata"))
}

fn backoff_for(attempt: usize) -> Duration {
    let millis = 300u64.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1) as u32));
    Duration::from_millis(millis.min(2_400))
}

#[derive(Debug, Clone, Copy)]
enum StreamSource {
    Stdout,
    Stderr,
}

struct StreamChunk {
    source: StreamSource,
    text: String,
}

struct StreamCapture {
    stderr: String,
}

async fn read_stream<R>(
    mut reader: R,
    source: StreamSource,
    tx: mpsc::UnboundedSender<StreamChunk>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0u8; 4096];
    loop {
        let bytes = reader.read(&mut buffer).await.context("读取进程输出失败")?;
        if bytes == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buffer[..bytes]).to_string();
        if tx.send(StreamChunk { source, text }).is_err() {
            break;
        }
    }
    Ok(())
}

async fn write_stream_log(
    path: PathBuf,
    mut rx: mpsc::UnboundedReceiver<StreamChunk>,
) -> Result<StreamCapture> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("创建日志目录失败：{}", parent.display()))?;
    }
    let mut file = TokioOpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .await
        .with_context(|| format!("打开日志文件失败：{}", path.display()))?;

    let mut stderr = String::new();
    let mut wrote_stderr_header = false;

    while let Some(chunk) = rx.recv().await {
        match chunk.source {
            StreamSource::Stdout => {
                file.write_all(chunk.text.as_bytes())
                    .await
                    .with_context(|| format!("写入 stdout 日志失败：{}", path.display()))?;
            }
            StreamSource::Stderr => {
                stderr.push_str(&chunk.text);
                if !wrote_stderr_header {
                    file.write_all(b"\n--- STDERR ---\n")
                        .await
                        .with_context(|| format!("写入 stderr 标题失败：{}", path.display()))?;
                    wrote_stderr_header = true;
                }
                file.write_all(chunk.text.as_bytes())
                    .await
                    .with_context(|| format!("写入 stderr 日志失败：{}", path.display()))?;
            }
        }
        file.flush()
            .await
            .with_context(|| format!("刷新日志文件失败：{}", path.display()))?;
    }

    Ok(StreamCapture { stderr })
}
