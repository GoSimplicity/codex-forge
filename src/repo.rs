use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::model::RepoSnapshot;

pub fn discover_repo(target_dir: &Path) -> Result<RepoSnapshot> {
    let repo_root = git_toplevel(target_dir)?;
    let display_name = repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown-repo")
        .to_string();

    let mut top_level_entries = fs::read_dir(&repo_root)
        .with_context(|| format!("读取仓库目录失败：{}", repo_root.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name != ".git")
        .collect::<Vec<_>>();
    top_level_entries.sort();
    top_level_entries.truncate(16);

    let mut detected_stacks = Vec::new();
    let markers = [
        ("Cargo.toml", "Rust"),
        ("package.json", "Node.js"),
        ("pnpm-lock.yaml", "pnpm"),
        ("go.mod", "Go"),
        ("pyproject.toml", "Python"),
        ("requirements.txt", "Python"),
        ("docker-compose.yml", "Docker"),
        ("docker-compose.yaml", "Docker"),
        ("Makefile", "Make"),
    ];

    for (marker, label) in markers {
        if repo_root.join(marker).exists() && !detected_stacks.iter().any(|item| item == label) {
            detected_stacks.push(label.to_string());
        }
    }

    let readme_excerpt = find_readme(&repo_root)?;

    Ok(RepoSnapshot {
        repo_root,
        display_name,
        top_level_entries,
        detected_stacks,
        readme_excerpt,
    })
}

fn git_toplevel(target_dir: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(target_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .with_context(|| format!("执行 git rev-parse 失败：{}", target_dir.display()))?;

    if !output.status.success() {
        bail!(
            "目标目录不是 Git 仓库：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

fn find_readme(repo_root: &Path) -> Result<Option<String>> {
    let candidates = ["README.md", "README", "readme.md"];

    for name in candidates {
        let path = repo_root.join(name);
        if path.exists() {
            let content = fs::read_to_string(&path)
                .with_context(|| format!("读取 README 失败：{}", path.display()))?;
            let snippet = content
                .lines()
                .filter(|line| !line.trim().is_empty())
                .take(20)
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(Some(snippet));
        }
    }

    Ok(None)
}
