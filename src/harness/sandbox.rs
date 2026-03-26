use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::{SandboxConfig, SandboxMountStrategy};

use super::store::make_id;
use super::types::{HarnessRunManifest, SandboxState};

#[derive(Debug, Clone)]
pub struct DockerSandboxProvider {
    pub image: String,
    pub mount_strategy: SandboxMountStrategy,
    pub privileged: bool,
    pub run_as_root: bool,
    pub repair_owner_on_exit: bool,
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
        ensure_host_repo_writable(repo_root)?;

        let workspace_root = run.run_dir.join("sandbox");
        let container_repo_workdir = PathBuf::from("/workspace/repo");
        if workspace_root.exists() {
            fs::remove_dir_all(&workspace_root).with_context(|| {
                format!("清理旧 sandbox 工作区失败：{}", workspace_root.display())
            })?;
        }
        fs::create_dir_all(&workspace_root)
            .with_context(|| format!("创建 sandbox 运行目录失败：{}", workspace_root.display()))?;

        let repo_workdir = match self.mount_strategy {
            SandboxMountStrategy::DirectRw => repo_root.to_path_buf(),
            SandboxMountStrategy::SnapshotCopy => {
                let repo_snapshot = workspace_root.join("repo");
                clone_or_copy_repo(repo_root, &repo_snapshot)?;
                repo_snapshot
            }
        };

        let container_name = sanitize_container_name(&format!("cf-{}", run.id));
        let bin = docker_bin();
        let mut command = Command::new(&bin);
        command
            .arg("run")
            .arg("-d")
            .arg("--rm")
            .arg("--name")
            .arg(&container_name);
        if self.privileged {
            command.arg("--privileged");
        }
        if self.run_as_root {
            command.arg("--user").arg("0:0");
        }
        command
            .arg("-v")
            .arg(format!("{}:/workspace/repo", repo_workdir.display()))
            .arg("-v")
            .arg(format!("{}:/workspace/run", workspace_root.display()))
            .arg("-w")
            .arg(&container_repo_workdir)
            .arg(&self.image)
            .arg("sh")
            .arg("-lc")
            .arg("while true; do sleep 3600; done");
        let output = command.output().with_context(|| {
            format!(
                "启动 Docker 沙箱失败：host_repo={} workspace={}",
                repo_root.display(),
                workspace_root.display()
            )
        })?;
        if !output.status.success() {
            bail!(
                "启动 Docker 沙箱失败：host_repo={} mounted_repo={} workspace={} stderr={}",
                repo_root.display(),
                repo_workdir.display(),
                workspace_root.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let (host_uid, host_gid) = host_identity()?;

        Ok(SandboxState {
            provider: "docker".to_string(),
            image: self.image.clone(),
            container_name,
            workspace_root,
            repo_workdir,
            container_repo_workdir,
            mount_strategy: mount_strategy_label(self.mount_strategy).to_string(),
            repair_owner_on_exit: self.repair_owner_on_exit,
            host_uid,
            host_gid,
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
        if sandbox.repair_owner_on_exit {
            repair_repo_owner(sandbox)?;
        }
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

impl From<&SandboxConfig> for DockerSandboxProvider {
    fn from(value: &SandboxConfig) -> Self {
        Self {
            image: value.docker_image.clone(),
            mount_strategy: value.mount_strategy,
            privileged: value.privileged,
            run_as_root: value.run_as_root,
            repair_owner_on_exit: value.repair_owner_on_exit,
        }
    }
}

impl From<&SandboxState> for DockerSandboxProvider {
    fn from(value: &SandboxState) -> Self {
        Self {
            image: value.image.clone(),
            mount_strategy: match value.mount_strategy.as_str() {
                "snapshot_copy" => SandboxMountStrategy::SnapshotCopy,
                _ => SandboxMountStrategy::DirectRw,
            },
            privileged: true,
            run_as_root: true,
            repair_owner_on_exit: value.repair_owner_on_exit,
        }
    }
}

fn docker_bin() -> PathBuf {
    std::env::var_os("CODEX_FORGE_DOCKER_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("docker"))
}

fn mount_strategy_label(value: SandboxMountStrategy) -> &'static str {
    match value {
        SandboxMountStrategy::DirectRw => "direct_rw",
        SandboxMountStrategy::SnapshotCopy => "snapshot_copy",
    }
}

fn ensure_host_repo_writable(repo_root: &Path) -> Result<()> {
    fs::create_dir_all(repo_root)
        .with_context(|| format!("目标目录不可创建：{}", repo_root.display()))?;
    let probe_path = repo_root.join(format!(".codex-forge-write-probe-{}", make_id("probe")));
    fs::write(&probe_path, "probe\n")
        .with_context(|| format!("目标目录不可写，无法启动沙箱：{}", repo_root.display()))?;
    fs::remove_file(&probe_path)
        .with_context(|| format!("清理写权限探针失败：{}", probe_path.display()))?;
    Ok(())
}

fn host_identity() -> Result<(Option<u32>, Option<u32>)> {
    Ok((command_id_value("-u")?, command_id_value("-g")?))
}

fn command_id_value(flag: &str) -> Result<Option<u32>> {
    let output = Command::new("id")
        .arg(flag)
        .output()
        .with_context(|| format!("读取宿主 {} 失败", flag))?;
    if !output.status.success() {
        bail!(
            "读取宿主 {} 失败：{}",
            flag,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        return Ok(None);
    }
    let parsed = value
        .parse::<u32>()
        .with_context(|| format!("解析宿主 {} 失败：{}", flag, value))?;
    Ok(Some(parsed))
}

fn repair_repo_owner(sandbox: &SandboxState) -> Result<()> {
    let Some(host_uid) = sandbox.host_uid else {
        return Ok(());
    };
    let Some(host_gid) = sandbox.host_gid else {
        return Ok(());
    };
    let bin = docker_bin();
    let output = Command::new(&bin)
        .arg("exec")
        .arg(&sandbox.container_name)
        .arg("sh")
        .arg("-lc")
        .arg(format!(
            "chown -R {}:{} {}",
            host_uid,
            host_gid,
            sandbox.container_repo_workdir.display()
        ))
        .output()
        .with_context(|| format!("修复宿主文件属主失败：{}", sandbox.container_name))?;
    if !output.status.success() {
        bail!(
            "修复宿主文件属主失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
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
