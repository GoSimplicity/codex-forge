use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub backend: BackendConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            backend: BackendConfig::default(),
            sandbox: SandboxConfig::default(),
            runtime: RuntimeConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    #[serde(default)]
    pub default_model: Option<String>,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            default_model: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default = "default_docker_image")]
    pub docker_image: String,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            docker_image: default_docker_image(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    #[serde(default)]
    pub auto_approve_readonly: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_turns: default_max_turns(),
            auto_approve_readonly: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedProjectConfig {
    pub path: PathBuf,
    pub value: ProjectConfig,
}

pub fn load_project_config(repo_root: &Path) -> Result<LoadedProjectConfig> {
    let path = repo_root.join("codex-forge.toml");
    if !path.exists() {
        return Ok(LoadedProjectConfig {
            path,
            value: ProjectConfig::default(),
        });
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
    let value: ProjectConfig = toml::from_str(&raw)
        .with_context(|| format!("解析配置文件失败：{}", path.display()))?;
    Ok(LoadedProjectConfig { path, value })
}

pub fn init_default_config(repo_root: &Path) -> Result<PathBuf> {
    let path = repo_root.join("codex-forge.toml");
    if path.exists() {
        return Ok(path);
    }
    let content = toml::to_string_pretty(&ProjectConfig::default()).context("序列化默认配置失败")?;
    fs::write(&path, content).with_context(|| format!("写入默认配置失败：{}", path.display()))?;
    Ok(path)
}

fn default_docker_image() -> String {
    "codex-forge-sandbox:latest".to_string()
}

fn default_max_turns() -> usize {
    6
}
