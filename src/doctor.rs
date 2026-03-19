use std::path::Path;

use anyhow::Result;

use crate::config::LoadedProjectConfig;
use crate::model::{ApplyMode, CheckStatus, DoctorCheck, DoctorReadiness, DoctorReport};
use crate::repo::discover_repo;
use crate::resources::{ResourceCatalog, resolve_role_set};
use crate::verify::extract_first_command;
use crate::workspace::describe_git_readiness;
use crate::worktree::{WorktreeManager, git_is_clean};

pub async fn run_doctor(
    target_dir: &Path,
    project_config: &LoadedProjectConfig,
    resources: &ResourceCatalog,
    apply_mode: Option<ApplyMode>,
    demo_mode: bool,
) -> Result<DoctorReport> {
    let repo = discover_repo(target_dir).ok();
    let effective_apply_mode = apply_mode.unwrap_or(project_config.settings.apply_mode);
    let mut checks = Vec::new();

    checks.push(check_codex().await);
    checks.push(check_git_repo(target_dir).await?);
    if let Some(repo) = &repo {
        checks.push(check_worktree(&repo.repo_root).await);
    } else {
        checks.push(DoctorCheck {
            name: "git worktree".to_string(),
            status: CheckStatus::Skipped,
            detail: "当前目录还不是 Git 仓库；run/plan 时会先自动 git init。".to_string(),
        });
    }
    checks.push(check_config(project_config));
    checks.push(check_resources(
        resources,
        &project_config.settings.role_set,
    ));
    checks.push(check_verification_commands(&project_config.settings.verification_commands).await);

    if matches!(effective_apply_mode, ApplyMode::AutoSafe) {
        if let Some(repo) = &repo {
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
        } else {
            checks.push(DoctorCheck {
                name: "目标工作区状态".to_string(),
                status: CheckStatus::Skipped,
                detail: "非 Git 目录将先自动初始化仓库，然后再进入 auto-safe 流程。".to_string(),
            });
        }
    }

    let failed = checks
        .iter()
        .filter(|check| check.status == CheckStatus::Failed)
        .count();
    let skipped = checks
        .iter()
        .filter(|check| check.status == CheckStatus::Skipped)
        .count();
    let ok = failed == 0;
    let readiness = if failed > 0 {
        DoctorReadiness::Red
    } else if skipped > 0 {
        DoctorReadiness::Yellow
    } else {
        DoctorReadiness::Green
    };
    let summary = if demo_mode {
        match readiness {
            DoctorReadiness::Green => "适合现场跑黄金路径 demo。".to_string(),
            DoctorReadiness::Yellow => "可演示，但建议先处理可预见阻塞项。".to_string(),
            DoctorReadiness::Red => "当前不适合直接 demo，建议先修复红灯项。".to_string(),
        }
    } else if ok {
        "运行前检查通过。".to_string()
    } else {
        "存在阻塞问题，请先处理失败项。".to_string()
    };

    Ok(DoctorReport {
        checks,
        ok,
        readiness,
        summary,
        demo_mode,
        recommended_role_set: if demo_mode {
            "default".to_string()
        } else {
            project_config.settings.role_set.clone()
        },
        recommended_apply_mode: if demo_mode {
            ApplyMode::AutoSafe
        } else {
            effective_apply_mode
        },
    })
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
    let detail = describe_git_readiness(repo_root)?;
    Ok(DoctorCheck {
        name: "Git 仓库".to_string(),
        status: if detail.contains("自动执行 git init") {
            CheckStatus::Skipped
        } else {
            CheckStatus::Passed
        },
        detail,
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

fn check_resources(resources: &ResourceCatalog, role_set: &str) -> DoctorCheck {
    match resolve_role_set(resources, role_set) {
        Ok(roles) => DoctorCheck {
            name: "资源目录".to_string(),
            status: CheckStatus::Passed,
            detail: format!(
                "role_set `{role_set}` 有效，共 {} 个角色；global 规则来源：{}",
                roles.len(),
                resources.rules.global_origin.describe()
            ),
        },
        Err(error) => DoctorCheck {
            name: "资源目录".to_string(),
            status: CheckStatus::Failed,
            detail: error.to_string(),
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
