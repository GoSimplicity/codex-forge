use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct ResolvedTargetDir {
    pub path: PathBuf,
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
    Ok(ResolvedTargetDir { path: resolved })
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
