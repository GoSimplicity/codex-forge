use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::model::{
    EnvironmentBlock, VerificationCommandResult, VerificationOverallStatus, VerificationReport,
    VerificationStatus, WorkerLocalVerification, WorkerResult, WorkerStatus,
};

pub async fn run_stage_verification(
    stage: &str,
    commands: &[String],
    workdir: &Path,
    output_dir: &Path,
) -> Result<Vec<VerificationCommandResult>> {
    fs::create_dir_all(output_dir)
        .with_context(|| format!("创建验证输出目录失败：{}", output_dir.display()))?;

    let mut results = Vec::new();
    for (index, command_line) in commands.iter().enumerate() {
        let stdout_path = output_dir.join(format!("{stage}-{index}.stdout.log"));
        let stderr_path = output_dir.join(format!("{stage}-{index}.stderr.log"));
        let output = Command::new("sh")
            .arg("-lc")
            .arg(command_line)
            .current_dir(workdir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .with_context(|| format!("执行验证命令失败：{command_line}"))?;

        fs::write(&stdout_path, &output.stdout)
            .with_context(|| format!("写入验证 stdout 失败：{}", stdout_path.display()))?;
        fs::write(&stderr_path, &output.stderr)
            .with_context(|| format!("写入验证 stderr 失败：{}", stderr_path.display()))?;

        let stdout_text = String::from_utf8_lossy(&output.stdout);
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        let environment_block =
            classify_environment_block(&stdout_text, &stderr_text).map(|(kind, evidence)| {
                EnvironmentBlock {
                    kind,
                    evidence,
                    fallback_used: false,
                }
            });
        let status = if output.status.success() {
            VerificationStatus::Passed
        } else if environment_block.is_some() {
            VerificationStatus::BlockedByEnvironment
        } else {
            VerificationStatus::Failed
        };

        results.push(VerificationCommandResult {
            stage: stage.to_string(),
            command: command_line.clone(),
            exit_code: output.status.code().unwrap_or(-1),
            status,
            stdout_path,
            stderr_path,
            capability: format!("{stage}: {command_line}"),
            environment_block,
        });
    }

    Ok(results)
}

pub fn build_verification_report(
    worker_results: &[WorkerResult],
    integration: Vec<VerificationCommandResult>,
    final_run: Vec<VerificationCommandResult>,
) -> VerificationReport {
    let worker_local = worker_results
        .iter()
        .filter_map(|result| {
            result
                .handoff
                .as_ref()
                .map(|handoff| WorkerLocalVerification {
                    agent_id: result.agent_id.clone(),
                    lines: handoff.verification.clone(),
                })
        })
        .collect::<Vec<_>>();

    let mut verified_capabilities = Vec::new();
    let mut failed_capabilities = Vec::new();
    let mut blocked_verifications = Vec::new();
    let mut fallback_verifications = Vec::new();

    for item in integration.iter().chain(final_run.iter()) {
        match item.status {
            VerificationStatus::Passed => verified_capabilities.push(item.capability.clone()),
            VerificationStatus::Failed => failed_capabilities.push(item.capability.clone()),
            VerificationStatus::BlockedByEnvironment => {
                blocked_verifications.push(item.capability.clone())
            }
            VerificationStatus::Skipped => {}
        }
        if item
            .environment_block
            .as_ref()
            .is_some_and(|block| block.fallback_used)
        {
            fallback_verifications.push(item.capability.clone());
        }
    }

    if integration.is_empty() && final_run.is_empty() {
        for result in worker_results
            .iter()
            .filter(|item| item.status == WorkerStatus::Failed)
        {
            let stdout = fs::read_to_string(&result.stdout_path).unwrap_or_default();
            let stderr = fs::read_to_string(&result.stderr_path).unwrap_or_default();
            let capability = format!("worker: {}", result.agent_id);
            if let Some((kind, _)) = classify_environment_block(&stdout, &stderr) {
                blocked_verifications.push(format!("{capability} ({kind})"));
            } else {
                failed_capabilities.push(capability);
            }
        }
    }

    let overall_status = if !failed_capabilities.is_empty() {
        VerificationOverallStatus::Failed
    } else if !blocked_verifications.is_empty() && !verified_capabilities.is_empty() {
        VerificationOverallStatus::Partial
    } else if !blocked_verifications.is_empty() {
        VerificationOverallStatus::Blocked
    } else {
        VerificationOverallStatus::Passed
    };

    VerificationReport {
        worker_local,
        integration,
        final_run,
        verified_capabilities,
        failed_capabilities,
        blocked_verifications,
        fallback_verifications,
        overall_status,
    }
}

pub fn extract_first_command(commands: &[String]) -> Option<String> {
    commands.first().and_then(|command| {
        command
            .split_whitespace()
            .next()
            .map(|item| item.to_string())
    })
}

pub fn verification_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("verification")
}

fn classify_environment_block(stdout: &str, stderr: &str) -> Option<(String, String)> {
    let combined = format!("{stdout}\n{stderr}").to_lowercase();
    let mapping: &[(&str, &[&str])] = &[
        (
            "network_access",
            &[
                "temporary failure in name resolution",
                "could not resolve",
                "network is unreachable",
                "connection timed out",
                "error sending request for url",
                "stream disconnected before completion",
            ],
        ),
        (
            "port_binding",
            &[
                "permission denied",
                "address already in use",
                "operation not permitted",
                "listen tcp",
            ],
        ),
        (
            "filesystem_permission",
            &[
                "read-only file system",
                "permission denied",
                "operation not permitted",
            ],
        ),
        (
            "worktree_state",
            &[
                "ambiguous argument 'head'",
                "unknown revision or path",
                "not a valid object name",
                "bad revision 'head'",
                "does not have any commits yet",
                "unborn branch",
            ],
        ),
        (
            "lock_contention",
            &[
                "resource temporarily unavailable",
                "could not get lock",
                "database is locked",
                "waiting for file lock",
            ],
        ),
    ];

    for (kind, needles) in mapping {
        if let Some(found) = needles.iter().find(|needle| combined.contains(**needle)) {
            return Some((kind.to_string(), found.to_string()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{build_verification_report, classify_environment_block};
    use crate::model::{VerificationOverallStatus, WorkerResult, WorkerStatus};
    use std::fs;
    use tempfile::TempDir;

    fn sample_worker_result(
        tempdir: &TempDir,
        status: WorkerStatus,
        stdout: &str,
        stderr: &str,
    ) -> WorkerResult {
        let stdout_path = tempdir.path().join("worker.stdout.log");
        let stderr_path = tempdir.path().join("worker.stderr.log");
        fs::write(&stdout_path, stdout).expect("write stdout");
        fs::write(&stderr_path, stderr).expect("write stderr");

        WorkerResult {
            agent_id: "architect-1".to_string(),
            role: "architect".to_string(),
            task_title: "执行图拆解".to_string(),
            status,
            exit_code: Some(1),
            attempts: 1,
            diagnostic_summary: None,
            final_message: String::new(),
            summary: None,
            changed_files: Vec::new(),
            worktree_path: tempdir.path().join("worktree"),
            prompt_path: tempdir.path().join("prompt.md"),
            stdout_path,
            stderr_path,
            events_path: tempdir.path().join("events.jsonl"),
            final_output_path: tempdir.path().join("final.md"),
            diff_path: None,
            git_status_path: None,
            handoff_path: None,
            handoff: None,
            error: Some("worker failed".to_string()),
        }
    }

    #[test]
    fn marks_worker_network_failure_as_blocked_when_verification_never_started() {
        let tempdir = TempDir::new().expect("tempdir");
        let worker = sample_worker_result(
            &tempdir,
            WorkerStatus::Failed,
            "stream disconnected before completion: error sending request for url (https://example.com)",
            "WARNING: proceeding, even though we could not update PATH: Operation not permitted (os error 1)",
        );

        let report = build_verification_report(&[worker], Vec::new(), Vec::new());

        assert_eq!(report.overall_status, VerificationOverallStatus::Blocked);
        assert_eq!(
            report.blocked_verifications,
            vec!["worker: architect-1 (network_access)".to_string()]
        );
        assert!(report.failed_capabilities.is_empty());
    }

    #[test]
    fn marks_worker_non_environment_failure_as_failed_when_verification_never_started() {
        let tempdir = TempDir::new().expect("tempdir");
        let worker = sample_worker_result(
            &tempdir,
            WorkerStatus::Failed,
            "",
            "panic: unexpected parse failure",
        );

        let report = build_verification_report(&[worker], Vec::new(), Vec::new());

        assert_eq!(report.overall_status, VerificationOverallStatus::Failed);
        assert_eq!(
            report.failed_capabilities,
            vec!["worker: architect-1".to_string()]
        );
        assert!(report.blocked_verifications.is_empty());
    }

    #[test]
    fn ignores_generic_unborn_word_when_not_a_git_state_error() {
        let block = classify_environment_block(
            "test unborn_repo_materializes_source_context_for_workers ... FAILED",
            "error: unknown option `orphan`",
        );

        assert!(block.is_none());
    }

    #[test]
    fn marks_actual_unborn_head_error_as_worktree_state() {
        let block = classify_environment_block(
            "",
            "fatal: your current branch 'main' does not have any commits yet",
        );

        assert_eq!(
            block,
            Some((
                "worktree_state".to_string(),
                "does not have any commits yet".to_string()
            ))
        );
    }
}
