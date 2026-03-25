use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::store::make_id;
use super::types::{HarnessRunManifest, SandboxState};

#[derive(Debug, Clone)]
pub struct DockerSandboxProvider {
    pub image: String,
}

#[derive(Debug, Clone)]
pub struct ShellExecResult {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub exit_code: Option<i32>,
}

impl DockerSandboxProvider {
    pub fn ensure_available(&self) -> Result<()> {
        let bin = docker_bin();
        let output = Command::new(&bin)
            .arg("--version")
            .output()
            .with_context(|| format!("执行 `{}` 失败", bin.display()))?;
        if !output.status.success() {
            bail!(
                "Docker 不可用：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    pub fn start(&self, repo_root: &Path, run: &HarnessRunManifest) -> Result<SandboxState> {
        self.ensure_available()?;
        let workspace_root = run.run_dir.join("sandbox").join("workspace");
        let repo_workdir = workspace_root.join("repo");
        if repo_workdir.exists() {
            fs::remove_dir_all(&repo_workdir)
                .with_context(|| format!("清理旧 sandbox 工作区失败：{}", repo_workdir.display()))?;
        }
        fs::create_dir_all(&workspace_root)
            .with_context(|| format!("创建 sandbox 工作区失败：{}", workspace_root.display()))?;
        clone_or_copy_repo(repo_root, &repo_workdir)?;

        let container_name = sanitize_container_name(&format!("cf-{}", run.id));
        let bin = docker_bin();
        let output = Command::new(&bin)
            .arg("run")
            .arg("-d")
            .arg("--rm")
            .arg("--name")
            .arg(&container_name)
            .arg("-v")
            .arg(format!("{}:/workspace", workspace_root.display()))
            .arg("-w")
            .arg("/workspace/repo")
            .arg(&self.image)
            .arg("sh")
            .arg("-lc")
            .arg("while true; do sleep 3600; done")
            .output()
            .with_context(|| format!("启动 Docker 沙箱失败：{}", workspace_root.display()))?;
        if !output.status.success() {
            bail!(
                "启动 Docker 沙箱失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok(SandboxState {
            provider: "docker".to_string(),
            image: self.image.clone(),
            container_name,
            workspace_root,
            repo_workdir,
            active: true,
        })
    }

    pub fn exec_shell(&self, sandbox: &SandboxState, command: &str) -> Result<ShellExecResult> {
        let bin = docker_bin();
        let output = Command::new(&bin)
            .arg("exec")
            .arg(&sandbox.container_name)
            .arg("sh")
            .arg("-lc")
            .arg(command)
            .output()
            .with_context(|| format!("执行 Docker 命令失败：{}", sandbox.container_name))?;

        Ok(ShellExecResult {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            success: output.status.success(),
            exit_code: output.status.code(),
        })
    }

    pub fn destroy(&self, sandbox: &SandboxState) -> Result<()> {
        let bin = docker_bin();
        let output = Command::new(&bin)
            .arg("rm")
            .arg("-f")
            .arg(&sandbox.container_name)
            .output()
            .with_context(|| format!("销毁 Docker 沙箱失败：{}", sandbox.container_name))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("No such container") {
                bail!("销毁 Docker 沙箱失败：{}", stderr.trim());
            }
        }
        Ok(())
    }
}

fn docker_bin() -> PathBuf {
    std::env::var_os("CODEX_FORGE_DOCKER_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("docker"))
}

fn clone_or_copy_repo(repo_root: &Path, dst: &Path) -> Result<()> {
    if repo_root.join(".git").exists() {
        let output = Command::new("git")
            .arg("clone")
            .arg("--quiet")
            .arg(repo_root)
            .arg(dst)
            .output()
            .with_context(|| format!("克隆仓库到 sandbox 失败：{}", dst.display()))?;
        if output.status.success() {
            return Ok(());
        }
    }
    copy_dir_recursive(repo_root, dst)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("创建目录失败：{}", dst.display()))?;
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry.with_context(|| format!("遍历目录失败：{}", src.display()))?;
        let path = entry.path();
        let relative = match path.strip_prefix(src) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        if should_skip(relative) {
            continue;
        }
        let target = dst.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("创建目录失败：{}", target.display()))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("创建父目录失败：{}", parent.display()))?;
            }
            fs::copy(path, &target).with_context(|| {
                format!("复制文件失败：{} -> {}", path.display(), target.display())
            })?;
        }
    }
    Ok(())
}

fn should_skip(relative: &Path) -> bool {
    relative.components().any(|component| {
        let value = component.as_os_str();
        value == OsStr::new(".codex-forge")
            || value == OsStr::new("target")
            || value == OsStr::new(".fake-bin")
    })
}

fn sanitize_container_name(value: &str) -> String {
    let base = value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .to_lowercase();
    format!("{}-{}", base, make_id("box"))
}
