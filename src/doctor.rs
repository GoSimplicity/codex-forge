use std::path::Path;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::config::LoadedProjectConfig;
use crate::model::{ApplyMode, CheckStatus, DoctorCheck, DoctorReport};
use crate::repo::discover_repo;
use crate::verify::extract_first_command;
use crate::worktree::{WorktreeManager, git_is_clean};

pub async fn run_doctor(
    target_dir: &Path,
    project_config: &LoadedProjectConfig,
    apply_mode: Option<ApplyMode>,
) -> Result<DoctorReport> {
    let repo = discover_repo(target_dir)?;
    let effective_apply_mode = apply_mode.unwrap_or(project_config.settings.apply_mode);
    let mut checks = Vec::new();

    checks.push(check_codex().await);
    checks.push(check_git_repo(&repo.repo_root).await?);
    checks.push(check_worktree(&repo.repo_root).await);
    checks.push(check_config(project_config));
    checks.push(check_verification_commands(&project_config.settings.verification_commands).await);

    if matches!(effective_apply_mode, ApplyMode::AutoSafe) {
        let clean = git_is_clean(&repo.repo_root).await?;
        checks.push(DoctorCheck {
            name: "目标工作区状态".to_string(),
            status: if clean {
                CheckStatus::Passed
            } else {
                CheckStatus::Failed
            },
            detail: if clean {
                "目标工作区干净，适合自动应用。".to_string()
            } else {
                "目标工作区存在未提交改动，auto-safe 同步前建议先清理。".to_string()
            },
        });
    }

    let ok = checks
        .iter()
        .all(|check| check.status != CheckStatus::Failed);
    Ok(DoctorReport { checks, ok })
}

async fn check_codex() -> DoctorCheck {
    match which::which("codex") {
        Ok(path) => DoctorCheck {
            name: "Codex CLI".to_string(),
            status: CheckStatus::Passed,
            detail: format!("找到 codex：{}", path.display()),
        },
        Err(error) => DoctorCheck {
            name: "Codex CLI".to_string(),
            status: CheckStatus::Failed,
            detail: format!("未找到 codex：{error}"),
        },
    }
}

async fn check_git_repo(repo_root: &Path) -> Result<DoctorCheck> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .await
        .context("执行 git rev-parse 失败")?;

    Ok(DoctorCheck {
        name: "Git 仓库".to_string(),
        status: if output.status.success() {
            CheckStatus::Passed
        } else {
            CheckStatus::Failed
        },
        detail: if output.status.success() {
            format!("仓库根目录：{}", repo_root.display())
        } else {
            String::from_utf8_lossy(&output.stderr).trim().to_string()
        },
    })
}

async fn check_worktree(repo_root: &Path) -> DoctorCheck {
    let manager = match WorktreeManager::new(repo_root, "doctor") {
        Ok(manager) => manager,
        Err(error) => {
            return DoctorCheck {
                name: "git worktree".to_string(),
                status: CheckStatus::Failed,
                detail: error.to_string(),
            };
        }
    };

    match manager.create_named("doctor-check", "HEAD").await {
        Ok(path) => {
            let cleanup_result = manager.cleanup(&path).await;
            DoctorCheck {
                name: "git worktree".to_string(),
                status: if cleanup_result.is_ok() {
                    CheckStatus::Passed
                } else {
                    CheckStatus::Failed
                },
                detail: if let Err(error) = cleanup_result {
                    format!("worktree 创建成功但清理失败：{error}")
                } else {
                    "可正常创建和移除 worktree。".to_string()
                },
            }
        }
        Err(error) => DoctorCheck {
            name: "git worktree".to_string(),
            status: CheckStatus::Failed,
            detail: error.to_string(),
        },
    }
}

fn check_config(project_config: &LoadedProjectConfig) -> DoctorCheck {
    DoctorCheck {
        name: "配置文件".to_string(),
        status: CheckStatus::Passed,
        detail: match &project_config.path {
            Some(path) => format!("配置有效：{}", path.display()),
            None => "未找到项目配置，已使用内置默认值。".to_string(),
        },
    }
}

async fn check_verification_commands(commands: &[String]) -> DoctorCheck {
    let mut missing = Vec::new();
    for command in commands {
        if let Some(binary) = extract_first_command(std::slice::from_ref(command))
            && which::which(&binary).is_err()
        {
            missing.push(binary);
        }
    }

    if missing.is_empty() {
        DoctorCheck {
            name: "验证命令".to_string(),
            status: CheckStatus::Passed,
            detail: format!("共检查 {} 条验证命令，均可执行。", commands.len()),
        }
    } else {
        DoctorCheck {
            name: "验证命令".to_string(),
            status: CheckStatus::Failed,
            detail: format!("以下命令缺失：{}", missing.join("、")),
        }
    }
}
