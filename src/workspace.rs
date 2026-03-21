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
    let base_dir = match explicit {
        Some(path) => normalize_existing_dir(path)?,
        None => {
            let cwd = env::current_dir().context("读取当前目录失败")?;
            normalize_existing_dir(&cwd)?
        }
    };
    let resolved = git_toplevel_or_self(&base_dir)?;
    if explicit.is_some() {
        remember_target_dir(&resolved)?;
    }

    Ok(ResolvedTargetDir { path: resolved })
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

fn state_root_candidates() -> Vec<PathBuf> {
    collect_state_roots(
        env::var_os("CODEX_FORGE_HOME").map(PathBuf::from),
        env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        env::var_os("HOME").map(PathBuf::from),
        env::temp_dir(),
    )
}

fn collect_state_roots(
    codex_forge_home: Option<PathBuf>,
    xdg_state_home: Option<PathBuf>,
    home: Option<PathBuf>,
    temp_dir: PathBuf,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Some(path) = codex_forge_home {
        roots.push(path);
    }
    if let Some(path) = xdg_state_home {
        roots.push(path.join("codex-forge"));
    }
    if let Some(path) = home {
        roots.push(path.join(".codex-forge"));
    }

    roots.push(temp_dir.join("codex-forge-state"));
    roots.dedup();
    roots
}

fn load_workspace_state() -> Result<WorkspaceState> {
    for root in state_root_candidates() {
        let path = root.join("workspace-state.json");
        if !path.exists() {
            continue;
        }
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        if let Ok(state) = serde_json::from_str(&raw) {
            return Ok(state);
        }
    }

    Ok(WorkspaceState::default())
}

fn save_workspace_state(state: &WorkspaceState) -> Result<()> {
    let payload = serde_json::to_vec_pretty(state).context("序列化工作目录状态失败")?;
    let mut last_error = None;

    for root in state_root_candidates() {
        if let Err(error) = fs::create_dir_all(&root) {
            last_error = Some(format!("创建状态目录失败：{} / {}", root.display(), error));
            continue;
        }
        let path = root.join("workspace-state.json");
        match fs::write(&path, &payload) {
            Ok(_) => return Ok(()),
            Err(error) => {
                last_error = Some(format!(
                    "写入工作目录状态失败：{} / {}",
                    path.display(),
                    error
                ))
            }
        }
    }

    bail!(
        "{}",
        last_error.unwrap_or_else(|| "写入工作目录状态失败：没有可用状态目录".to_string())
    )
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

#[cfg(test)]
mod tests {
    use super::collect_state_roots;
    use std::path::PathBuf;

    #[test]
    fn collects_state_roots_in_priority_order_with_temp_fallback() {
        let roots = collect_state_roots(
            Some(PathBuf::from("/cfg")),
            Some(PathBuf::from("/xdg")),
            Some(PathBuf::from("/home/demo")),
            PathBuf::from("/tmp"),
        );

        assert_eq!(
            roots,
            vec![
                PathBuf::from("/cfg"),
                PathBuf::from("/xdg/codex-forge"),
                PathBuf::from("/home/demo/.codex-forge"),
                PathBuf::from("/tmp/codex-forge-state"),
            ]
        );
    }

    #[test]
    fn deduplicates_overlapping_state_roots() {
        let roots = collect_state_roots(
            Some(PathBuf::from("/tmp/codex-forge-state")),
            None,
            None,
            PathBuf::from("/tmp"),
        );

        assert_eq!(roots, vec![PathBuf::from("/tmp/codex-forge-state")]);
    }
}
