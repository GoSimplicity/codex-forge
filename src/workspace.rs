use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const DEFAULT_GIT_USER_NAME: &str = "codex-forge";
const DEFAULT_GIT_USER_EMAIL: &str = "codex-forge@local";

#[derive(Debug, Clone)]
pub struct ResolvedTargetDir {
    pub path: PathBuf,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct WorkspaceState {
    last_target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct WorkspacePreparation {
    pub target_dir: PathBuf,
    pub git_initialized: bool,
    pub local_identity_configured: bool,
}

pub fn resolve_target_dir(explicit: Option<&Path>) -> Result<ResolvedTargetDir> {
    if let Some(path) = explicit {
        let resolved = normalize_existing_dir(path)?;
        remember_target_dir(&resolved)?;
        return Ok(ResolvedTargetDir { path: resolved });
    }

    if let Some(path) = load_workspace_state()?.last_target_dir
        && path.exists()
    {
        return Ok(ResolvedTargetDir { path });
    }

    Ok(ResolvedTargetDir {
        path: env::current_dir().context("读取当前目录失败")?,
    })
}

pub fn remember_target_dir(path: &Path) -> Result<()> {
    let mut state = load_workspace_state()?;
    state.last_target_dir = Some(path.to_path_buf());
    save_workspace_state(&state)
}

pub async fn prepare_target_dir(target_dir: &Path) -> Result<WorkspacePreparation> {
    let target_dir = normalize_existing_dir(target_dir)?;
    let inside_repo = git_is_inside_work_tree(&target_dir)?;
    let mut git_initialized = false;
    let mut local_identity_configured = false;

    if !inside_repo {
        let output = Command::new("git")
            .arg("init")
            .arg(&target_dir)
            .output()
            .with_context(|| format!("执行 git init 失败：{}", target_dir.display()))?;
        if !output.status.success() {
            bail!(
                "初始化 Git 仓库失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        git_initialized = true;
    }

    let repo_root = git_toplevel_or_self(&target_dir)?;
    if ensure_local_identity(&repo_root)? {
        local_identity_configured = true;
    }
    remember_target_dir(&target_dir)?;

    Ok(WorkspacePreparation {
        target_dir,
        git_initialized,
        local_identity_configured,
    })
}

pub fn describe_git_readiness(target_dir: &Path) -> Result<String> {
    let target_dir = normalize_existing_dir(target_dir)?;
    if git_is_inside_work_tree(&target_dir)? {
        let root = git_toplevel_or_self(&target_dir)?;
        return Ok(format!("Git 已就绪：{}", root.display()));
    }
    Ok("当前目录还不是 Git 仓库，但 run/plan 时会自动执行 git init。".to_string())
}

pub fn cleanup_empty_dirs(root: &Path) -> Result<()> {
    if !root.exists() || !root.is_dir() {
        return Ok(());
    }
    cleanup_empty_dirs_inner(root)
}

fn normalize_existing_dir(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir().context("读取当前目录失败")?.join(path)
    };
    if !absolute.exists() {
        bail!("目标目录不存在：{}", absolute.display());
    }
    if !absolute.is_dir() {
        bail!("目标路径不是目录：{}", absolute.display());
    }
    fs::canonicalize(&absolute).with_context(|| format!("规范化目录失败：{}", absolute.display()))
}

fn state_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("CODEX_FORGE_HOME") {
        let root = PathBuf::from(path);
        fs::create_dir_all(&root)
            .with_context(|| format!("创建 CODEX_FORGE_HOME 失败：{}", root.display()))?;
        return Ok(root);
    }

    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        let root = PathBuf::from(path).join("codex-forge");
        if fs::create_dir_all(&root).is_ok() {
            return Ok(root);
        }
    }

    if let Some(home) = env::var_os("HOME") {
        let root = PathBuf::from(home).join(".codex-forge");
        if fs::create_dir_all(&root).is_ok() {
            return Ok(root);
        }
    }

    let fallback = env::temp_dir().join("codex-forge-state");
    fs::create_dir_all(&fallback)
        .with_context(|| format!("创建回退状态目录失败：{}", fallback.display()))?;
    Ok(fallback)
}

fn state_path() -> Result<PathBuf> {
    Ok(state_root()?.join("workspace-state.json"))
}

fn load_workspace_state() -> Result<WorkspaceState> {
    let path = state_path()?;
    if !path.exists() {
        return Ok(WorkspaceState::default());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("读取工作目录状态失败：{}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("解析工作目录状态失败：{}", path.display()))
}

fn save_workspace_state(state: &WorkspaceState) -> Result<()> {
    let path = state_path()?;
    fs::write(
        &path,
        serde_json::to_vec_pretty(state).context("序列化工作目录状态失败")?,
    )
    .with_context(|| format!("写入工作目录状态失败：{}", path.display()))
}

fn git_is_inside_work_tree(target_dir: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(target_dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .with_context(|| format!("检查 Git 工作区失败：{}", target_dir.display()))?;
    Ok(output.status.success())
}

fn git_toplevel_or_self(target_dir: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(target_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .with_context(|| format!("读取 Git 根目录失败：{}", target_dir.display()))?;

    if output.status.success() {
        Ok(PathBuf::from(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ))
    } else {
        Ok(target_dir.to_path_buf())
    }
}

fn ensure_local_identity(repo_root: &Path) -> Result<bool> {
    let has_name = git_local_config_exists(repo_root, "user.name")?;
    let has_email = git_local_config_exists(repo_root, "user.email")?;
    let mut wrote = false;

    if !has_name {
        set_git_local_config(repo_root, "user.name", DEFAULT_GIT_USER_NAME)?;
        wrote = true;
    }
    if !has_email {
        set_git_local_config(repo_root, "user.email", DEFAULT_GIT_USER_EMAIL)?;
        wrote = true;
    }

    Ok(wrote)
}

fn git_local_config_exists(repo_root: &Path, key: &str) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["config", "--local", "--get", key])
        .output()
        .with_context(|| format!("读取 Git 配置失败：{}", repo_root.display()))?;
    Ok(output.status.success())
}

fn set_git_local_config(repo_root: &Path, key: &str, value: &str) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["config", "--local", key, value])
        .output()
        .with_context(|| format!("写入 Git 配置失败：{}", repo_root.display()))?;
    if !output.status.success() {
        bail!(
            "写入 Git 配置失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn cleanup_empty_dirs_inner(path: &Path) -> Result<()> {
    let entries = fs::read_dir(path)
        .with_context(|| format!("读取目录失败：{}", path.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect::<Vec<_>>();

    for entry in &entries {
        if entry.is_dir() {
            cleanup_empty_dirs_inner(entry)?;
        }
    }

    let mut remaining = fs::read_dir(path)
        .with_context(|| format!("读取目录失败：{}", path.display()))?
        .filter_map(|entry| entry.ok());
    if remaining.next().is_none() {
        let _ = fs::remove_dir(path);
    }
    Ok(())
}
