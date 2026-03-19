use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use walkdir::WalkDir;

use crate::model::WorkerResult;

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Debug, Clone)]
pub struct WorktreeManager {
    repo_root: PathBuf,
    base_dir: PathBuf,
}

impl WorktreeManager {
    pub fn new(repo_root: &Path, session_id: &str) -> Result<Self> {
        let repo_name = repo_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repo")
            .replace([' ', '/'], "-");
        let base_dir = std::env::temp_dir()
            .join("codex-forge")
            .join(repo_name)
            .join(session_id);
        fs::create_dir_all(&base_dir)
            .with_context(|| format!("创建 worktree 基目录失败：{}", base_dir.display()))?;
        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            base_dir,
        })
    }

    pub async fn create(&self, agent_id: &str) -> Result<PathBuf> {
        self.create_named(agent_id, "HEAD").await
    }

    pub async fn create_named(&self, name: &str, reference: &str) -> Result<PathBuf> {
        let worktree_path = self.base_dir.join(name);
        let has_head = git_has_head(&self.repo_root).await?;
        if !has_head && reference == "HEAD" {
            create_unborn_worktree(&self.repo_root, &worktree_path).await?;
        } else {
            let mut command = Command::new("git");
            command
                .arg("-C")
                .arg(&self.repo_root)
                .arg("worktree")
                .arg("add")
                .arg("--detach")
                .arg(&worktree_path)
                .arg(reference);

            let output = command
                .output()
                .await
                .context("执行 git worktree add 失败")?;

            if !output.status.success() {
                bail!(
                    "创建 worktree 失败：{}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        ensure_worktree_ready(&worktree_path).await?;
        Ok(worktree_path)
    }

    pub async fn cleanup(&self, worktree_path: &Path) -> Result<()> {
        if is_standalone_git_repo(worktree_path) {
            fs::remove_dir_all(worktree_path)
                .with_context(|| format!("清理独立 worktree 失败：{}", worktree_path.display()))?;
            return Ok(());
        }

        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["worktree", "remove", "--force"])
            .arg(worktree_path)
            .output()
            .await
            .context("执行 git worktree remove 失败")?;

        if !output.status.success() {
            bail!(
                "清理 worktree 失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let branch_name = self.orphan_branch_name(
            worktree_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("worktree"),
        );
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["branch", "-D", &branch_name])
            .output()
            .await;
        Ok(())
    }

    fn orphan_branch_name(&self, name: &str) -> String {
        let session = self
            .base_dir
            .file_name()
            .and_then(|item| item.to_str())
            .unwrap_or("session");
        format!(
            "cf-{}-{}",
            sanitize_branch_component(session),
            sanitize_branch_component(name)
        )
    }
}

pub async fn capture_git_artifacts(
    worktree_path: &Path,
    status_path: &Path,
    diff_path: &Path,
) -> Result<Vec<String>> {
    let status_output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["status", "--short", "--untracked-files=all"])
        .output()
        .await
        .context("执行 git status 失败")?;
    fs::write(status_path, &status_output.stdout)
        .with_context(|| format!("写入 git status 失败：{}", status_path.display()))?;

    let diff_output = diff_against_base(worktree_path).await?;
    fs::write(diff_path, &diff_output.stdout)
        .with_context(|| format!("写入 diff 失败：{}", diff_path.display()))?;

    let changed_files = String::from_utf8_lossy(&status_output.stdout)
        .lines()
        .filter_map(parse_status_line)
        .collect::<Vec<_>>();
    Ok(changed_files)
}

pub async fn git_diff_binary(worktree_path: &Path, output_path: &Path) -> Result<()> {
    let diff_output = diff_against_base(worktree_path).await?;
    fs::write(output_path, &diff_output.stdout)
        .with_context(|| format!("写入最终 diff 失败：{}", output_path.display()))?;
    Ok(())
}

pub async fn git_has_head(repo_root: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .await
        .context("执行 git rev-parse --verify HEAD 失败")?;
    Ok(output.status.success())
}

pub async fn git_is_clean(repo_root: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--short"])
        .output()
        .await
        .context("执行 git status 失败")?;

    if !output.status.success() {
        bail!(
            "获取仓库状态失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let status_text = String::from_utf8_lossy(&output.stdout).to_string();
    let dirty_lines = status_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.ends_with(".codex-forge") && !line.contains(".codex-forge/"))
        .collect::<Vec<_>>();

    Ok(dirty_lines.is_empty())
}

pub async fn apply_patch_file(target_dir: &Path, patch_path: &Path) -> Result<()> {
    run_git_with_path(target_dir, ["apply", "--check", "--binary"], patch_path).await?;
    run_git_with_path(target_dir, ["apply", "--binary"], patch_path).await
}

pub async fn apply_patch_file_for_paths(
    target_dir: &Path,
    patch_path: &Path,
    included_paths: &[String],
) -> Result<()> {
    run_git_apply_with_filters(target_dir, patch_path, included_paths, true).await?;
    run_git_apply_with_filters(target_dir, patch_path, included_paths, false).await
}

pub async fn git_commit_paths(
    repo_root: &Path,
    message: &str,
    included_paths: &[String],
    allow_empty: bool,
) -> Result<String> {
    if !included_paths.is_empty() {
        let mut add_command = Command::new("git");
        add_command
            .arg("-C")
            .arg(repo_root)
            .args(["add", "-A", "--"]);
        for path in included_paths {
            add_command.arg(path);
        }
        let add_output = add_command.output().await.context("执行 git add 失败")?;
        if !add_output.status.success() {
            bail!("{}", String::from_utf8_lossy(&add_output.stderr).trim());
        }
    }

    let mut commit_command = Command::new("git");
    commit_command.arg("-C").arg(repo_root).arg("commit");
    if allow_empty {
        commit_command.arg("--allow-empty");
    }
    commit_command.args(["-m", message]);
    let commit_output = commit_command
        .output()
        .await
        .context("执行 git commit 失败")?;
    if !commit_output.status.success() {
        bail!("{}", String::from_utf8_lossy(&commit_output.stderr).trim());
    }

    let rev_output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        .context("读取最新 commit 失败")?;
    if !rev_output.status.success() {
        bail!("{}", String::from_utf8_lossy(&rev_output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&rev_output.stdout)
        .trim()
        .to_string())
}

pub async fn materialize_dependency_patches(
    target_dir: &Path,
    dependency_results: &[&WorkerResult],
) -> Result<(Vec<String>, Vec<String>)> {
    let mut applied = Vec::new();
    let mut failed = Vec::new();

    for result in dependency_results {
        let Some(diff_path) = result.diff_path.as_ref() else {
            continue;
        };
        if !diff_path.exists() {
            continue;
        }
        let metadata = fs::metadata(diff_path)
            .with_context(|| format!("读取依赖 patch 元数据失败：{}", diff_path.display()))?;
        if metadata.len() == 0 {
            continue;
        }

        match apply_patch_file(target_dir, diff_path).await {
            Ok(()) => applied.push(result.agent_id.clone()),
            Err(error) => failed.push(format!("{}: {}", result.agent_id, error)),
        }
    }

    Ok((applied, failed))
}

pub async fn run_git_with_path<I, S>(cwd: &Path, args: I, path: &Path) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args.into_iter().map(|item| item.as_ref().to_string()))
        .arg(path)
        .output()
        .await
        .context("执行 git 命令失败")?;

    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

async fn run_git_apply_with_filters(
    cwd: &Path,
    patch_path: &Path,
    included_paths: &[String],
    check_only: bool,
) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(cwd).arg("apply");
    if check_only {
        command.arg("--check");
    }
    command.arg("--binary");
    for path in included_paths {
        command.arg(format!("--include={path}"));
    }
    command.arg(patch_path);

    let output = command.output().await.context("执行 git apply 失败")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

fn parse_status_line(line: &str) -> Option<String> {
    if line.trim().is_empty() {
        return None;
    }

    let payload = line.get(3..).unwrap_or(line).trim();
    if let Some((_, new_path)) = payload.split_once(" -> ") {
        return Some(new_path.to_string());
    }
    Some(payload.to_string())
}

async fn diff_against_base(worktree_path: &Path) -> Result<std::process::Output> {
    if git_has_head(worktree_path).await? {
        let add_output = Command::new("git")
            .arg("-C")
            .arg(worktree_path)
            .args(["add", "-A"])
            .output()
            .await
            .context("执行 git add -A 失败")?;
        if !add_output.status.success() {
            bail!(
                "暂存 worktree 改动失败：{}",
                String::from_utf8_lossy(&add_output.stderr).trim()
            );
        }

        Command::new("git")
            .arg("-C")
            .arg(worktree_path)
            .args(["diff", "--binary", "--cached", "HEAD"])
            .output()
            .await
            .context("执行 git diff 失败")
    } else {
        let add_output = Command::new("git")
            .arg("-C")
            .arg(worktree_path)
            .args(["add", "-A"])
            .output()
            .await
            .context("执行 git add -A 失败")?;
        if !add_output.status.success() {
            bail!(
                "暂存 unborn worktree 改动失败：{}",
                String::from_utf8_lossy(&add_output.stderr).trim()
            );
        }

        Command::new("git")
            .arg("-C")
            .arg(worktree_path)
            .args(["diff", "--binary", "--cached", EMPTY_TREE])
            .output()
            .await
            .context("执行 unborn git diff 失败")
    }
}

async fn create_unborn_worktree(repo_root: &Path, worktree_path: &Path) -> Result<()> {
    if worktree_path.exists() {
        fs::remove_dir_all(worktree_path).with_context(|| {
            format!("清理旧的 unborn worktree 失败：{}", worktree_path.display())
        })?;
    }
    if let Some(parent) = worktree_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建 unborn worktree 父目录失败：{}", parent.display()))?;
    }

    let init_output = Command::new("git")
        .arg("init")
        .arg(worktree_path)
        .output()
        .await
        .context("初始化 unborn worktree 失败")?;
    if !init_output.status.success() {
        bail!(
            "初始化 unborn worktree 失败：{}",
            String::from_utf8_lossy(&init_output.stderr).trim()
        );
    }

    materialize_unborn_worktree(repo_root, worktree_path).await
}

async fn materialize_unborn_worktree(repo_root: &Path, worktree_path: &Path) -> Result<()> {
    mirror_repo_snapshot(repo_root, worktree_path)?;

    let add_output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["add", "-A"])
        .output()
        .await
        .context("暂存 unborn worktree 基线失败")?;
    if !add_output.status.success() {
        bail!(
            "暂存 unborn worktree 基线失败：{}",
            String::from_utf8_lossy(&add_output.stderr).trim()
        );
    }

    let commit_output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args([
            "-c",
            "user.name=codex-forge",
            "-c",
            "user.email=codex-forge@local",
            "commit",
            "--allow-empty",
            "-m",
            "codex-forge baseline",
        ])
        .output()
        .await
        .context("提交 unborn worktree 基线失败")?;
    if !commit_output.status.success() {
        bail!(
            "提交 unborn worktree 基线失败：{}",
            String::from_utf8_lossy(&commit_output.stderr).trim()
        );
    }

    Ok(())
}

fn mirror_repo_snapshot(repo_root: &Path, worktree_path: &Path) -> Result<()> {
    for entry in WalkDir::new(repo_root)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        if path == repo_root {
            continue;
        }

        let relative = path
            .strip_prefix(repo_root)
            .with_context(|| format!("计算相对路径失败：{}", path.display()))?;
        if should_skip_snapshot_path(relative) {
            continue;
        }

        let target = worktree_path.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("创建目录失败：{}", target.display()))?;
            continue;
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建父目录失败：{}", parent.display()))?;
        }
        fs::copy(path, &target)
            .with_context(|| format!("复制文件失败：{} -> {}", path.display(), target.display()))?;
    }

    Ok(())
}

fn should_skip_snapshot_path(relative: &Path) -> bool {
    let first = relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .unwrap_or_default();

    matches!(first, ".git" | ".codex-forge" | "target")
}

fn is_standalone_git_repo(worktree_path: &Path) -> bool {
    worktree_path.join(".git").is_dir()
}

fn sanitize_branch_component(text: &str) -> String {
    let sanitized = text
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    sanitized.trim_matches('-').to_string()
}

async fn ensure_worktree_ready(worktree_path: &Path) -> Result<()> {
    if !worktree_path.exists() {
        bail!("worktree 目录不存在：{}", worktree_path.display());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["status", "--short"])
        .output()
        .await
        .context("执行 worktree 可用性检查失败")?;

    if !output.status.success() {
        bail!(
            "worktree 可用性检查失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{parse_status_line, sanitize_branch_component, should_skip_snapshot_path};

    #[test]
    fn parses_renamed_path() {
        assert_eq!(
            parse_status_line("R  old.txt -> new.txt").as_deref(),
            Some("new.txt")
        );
    }

    #[test]
    fn sanitizes_branch_component() {
        assert_eq!(
            sanitize_branch_component("20260318-abc/worker 1"),
            "20260318-abc-worker-1"
        );
    }

    #[test]
    fn skips_internal_snapshot_paths() {
        assert!(should_skip_snapshot_path(Path::new(".git/config")));
        assert!(should_skip_snapshot_path(Path::new(
            ".codex-forge/sessions/x"
        )));
        assert!(should_skip_snapshot_path(Path::new("target/debug/app")));
        assert!(!should_skip_snapshot_path(Path::new("src/main.rs")));
    }
}
