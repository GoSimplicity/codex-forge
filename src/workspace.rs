use std::env;
use std::fs;
use std::path::{Path, PathBuf};

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
    Ok(ResolvedTargetDir { path: base_dir })
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;

    use super::resolve_target_dir;

    #[test]
    fn explicit_target_dir_is_not_raised_to_git_toplevel() {
        let dir = TempDir::new().expect("tempdir");
        let nested = dir.path().join("apps").join("demo");
        fs::create_dir_all(&nested).expect("mkdir");
        let status = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        assert!(status.success());

        let resolved = resolve_target_dir(Some(&nested)).expect("resolve");
        assert_eq!(
            resolved.path,
            fs::canonicalize(&nested).expect("canonical nested")
        );
    }
}
