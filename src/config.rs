use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectConfig {
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BackendProvider {
    #[default]
    Codex,
    #[serde(rename = "openai_compatible", alias = "open_ai_compatible")]
    OpenAiCompatible,
}

impl BackendProvider {
    pub fn config_value(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OpenAiCompatible => "openai_compatible",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::OpenAiCompatible => "OpenAI Compatible",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Codex => Self::OpenAiCompatible,
            Self::OpenAiCompatible => Self::Codex,
        }
    }

    pub fn parse_config_value(value: &str) -> Result<Self> {
        match value.trim() {
            "codex" => Ok(Self::Codex),
            "openai_compatible" | "open_ai_compatible" => Ok(Self::OpenAiCompatible),
            other => bail!(
                "不支持的 backend.provider：`{other}`；目前仅支持 `codex` 或 `openai_compatible`"
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    #[serde(default)]
    pub provider: BackendProvider,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_turn_timeout_secs")]
    pub turn_timeout_secs: u64,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            provider: BackendProvider::Codex,
            key: None,
            base_url: None,
            model: None,
            turn_timeout_secs: default_turn_timeout_secs(),
        }
    }
}

impl BackendConfig {
    pub fn validate(&self) -> Result<()> {
        if self.turn_timeout_secs == 0 {
            bail!("backend.turn_timeout_secs 必须大于 0");
        }
        match self.provider {
            BackendProvider::Codex => Ok(()),
            BackendProvider::OpenAiCompatible => {
                if self
                    .key
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty())
                {
                    bail!("backend.key 不能为空");
                }
                if self
                    .base_url
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty())
                {
                    bail!("backend.base_url 不能为空");
                }
                if self
                    .model
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty())
                {
                    bail!("backend.model 不能为空");
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalConfig {
    #[serde(default)]
    pub backend: BackendConfig,
}

#[derive(Debug, Clone, Default)]
pub struct AppConfig {
    pub backend: BackendConfig,
    pub sandbox: SandboxConfig,
    pub runtime: RuntimeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default = "default_docker_image")]
    pub docker_image: String,
    #[serde(default = "default_mount_strategy")]
    pub mount_strategy: SandboxMountStrategy,
    #[serde(default = "default_true")]
    pub privileged: bool,
    #[serde(default = "default_true")]
    pub run_as_root: bool,
    #[serde(default = "default_true")]
    pub repair_owner_on_exit: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            docker_image: default_docker_image(),
            mount_strategy: default_mount_strategy(),
            privileged: default_true(),
            run_as_root: default_true(),
            repair_owner_on_exit: default_true(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMountStrategy {
    DirectRw,
    SnapshotCopy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    #[serde(default = "default_max_generator_turns")]
    pub max_generator_turns: usize,
    #[serde(default = "default_max_feature_retries")]
    pub max_feature_retries: usize,
    #[serde(default = "default_max_evaluator_loops")]
    pub max_evaluator_loops: usize,
    #[serde(default = "default_bootstrap_message_limit")]
    pub bootstrap_message_limit: usize,
    #[serde(default = "default_enable_long_running_delivery")]
    pub enable_long_running_delivery: bool,
    #[serde(default)]
    pub auto_approve_readonly: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_turns: default_max_turns(),
            max_generator_turns: default_max_generator_turns(),
            max_feature_retries: default_max_feature_retries(),
            max_evaluator_loops: default_max_evaluator_loops(),
            bootstrap_message_limit: default_bootstrap_message_limit(),
            enable_long_running_delivery: default_enable_long_running_delivery(),
            auto_approve_readonly: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedProjectConfig {
    pub path: PathBuf,
    pub value: ProjectConfig,
}

#[derive(Debug, Clone)]
pub struct LoadedGlobalConfig {
    pub path: PathBuf,
    pub value: GlobalConfig,
}

impl AppConfig {
    pub fn from_parts(project: ProjectConfig, global: GlobalConfig) -> Self {
        Self {
            backend: global.backend,
            sandbox: project.sandbox,
            runtime: project.runtime,
        }
    }
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
    let value: ProjectConfig =
        toml::from_str(&raw).with_context(|| format!("解析配置文件失败：{}", path.display()))?;
    Ok(LoadedProjectConfig { path, value })
}

pub fn load_global_config() -> Result<LoadedGlobalConfig> {
    let path = global_config_path()?;
    if !path.exists() {
        return Ok(LoadedGlobalConfig {
            path,
            value: GlobalConfig::default(),
        });
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("读取全局配置文件失败：{}", path.display()))?;
    let value: GlobalConfig = toml::from_str(&raw)
        .with_context(|| format!("解析全局配置文件失败：{}", path.display()))?;
    Ok(LoadedGlobalConfig { path, value })
}

pub fn load_app_config(repo_root: &Path) -> Result<AppConfig> {
    let project = load_project_config(repo_root)?;
    let global = load_global_config()?;
    Ok(AppConfig::from_parts(project.value, global.value))
}

pub fn init_default_config(repo_root: &Path) -> Result<PathBuf> {
    let path = repo_root.join("codex-forge.toml");
    if path.exists() {
        return Ok(path);
    }
    let content =
        toml::to_string_pretty(&ProjectConfig::default()).context("序列化默认配置失败")?;
    fs::write(&path, content).with_context(|| format!("写入默认配置失败：{}", path.display()))?;
    Ok(path)
}

pub fn init_global_config() -> Result<PathBuf> {
    let path = global_config_path()?;
    if path.exists() {
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建全局配置目录失败：{}", parent.display()))?;
    }
    let content =
        toml::to_string_pretty(&GlobalConfig::default()).context("序列化全局默认配置失败")?;
    fs::write(&path, content).with_context(|| format!("写入全局配置失败：{}", path.display()))?;
    Ok(path)
}

pub fn save_global_config(config: &GlobalConfig) -> Result<PathBuf> {
    validate_global_config(config)?;
    let path = global_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建全局配置目录失败：{}", parent.display()))?;
    }
    let content = toml::to_string_pretty(config).context("序列化全局配置失败")?;
    fs::write(&path, content).with_context(|| format!("写入全局配置失败：{}", path.display()))?;
    Ok(path)
}

pub fn set_global_backend_provider(provider: BackendProvider) -> Result<LoadedGlobalConfig> {
    let mut loaded = load_global_config()?;
    loaded.value.backend.provider = provider;
    save_global_config(&loaded.value)?;
    Ok(loaded)
}

pub fn validate_project_config(config: &ProjectConfig) -> Result<()> {
    if config.runtime.max_turns == 0 {
        bail!("runtime.max_turns 必须大于 0");
    }
    if config.runtime.max_generator_turns == 0 {
        bail!("runtime.max_generator_turns 必须大于 0");
    }
    if config.runtime.max_feature_retries == 0 {
        bail!("runtime.max_feature_retries 必须大于 0");
    }
    if config.runtime.max_evaluator_loops == 0 {
        bail!("runtime.max_evaluator_loops 必须大于 0");
    }
    if config.runtime.bootstrap_message_limit == 0 {
        bail!("runtime.bootstrap_message_limit 必须大于 0");
    }
    if config.sandbox.docker_image.trim().is_empty() {
        bail!("sandbox.docker_image 不能为空");
    }
    Ok(())
}

pub fn validate_global_config(config: &GlobalConfig) -> Result<()> {
    config.backend.validate()
}

pub fn global_config_path() -> Result<PathBuf> {
    Ok(global_config_dir()?.join("config.toml"))
}

fn global_config_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("CODEX_FORGE_HOME").map(PathBuf::from) {
        return Ok(path);
    }

    let home = env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        anyhow!("未找到 HOME 环境变量，且未设置 CODEX_FORGE_HOME，无法定位全局配置目录")
    })?;
    Ok(home.join(".codex-forge"))
}

fn default_docker_image() -> String {
    "codex-forge-sandbox:latest".to_string()
}

fn default_mount_strategy() -> SandboxMountStrategy {
    SandboxMountStrategy::DirectRw
}

fn default_true() -> bool {
    true
}

fn default_turn_timeout_secs() -> u64 {
    600
}

fn default_max_turns() -> usize {
    6
}

fn default_max_generator_turns() -> usize {
    16
}

fn default_max_feature_retries() -> usize {
    2
}

fn default_max_evaluator_loops() -> usize {
    3
}

fn default_bootstrap_message_limit() -> usize {
    8
}

fn default_enable_long_running_delivery() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use once_cell::sync::Lazy;
    use tempfile::TempDir;

    use super::{RuntimeConfig, global_config_path};

    static ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    #[test]
    fn generator_budget_defaults_to_more_than_main_turns() {
        let runtime = RuntimeConfig::default();
        assert!(runtime.max_generator_turns > runtime.max_turns);
    }

    #[test]
    fn global_config_path_prefers_codex_forge_home() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = TempDir::new().expect("tempdir");
        let expected = dir.path().join("config.toml");
        unsafe {
            std::env::set_var("CODEX_FORGE_HOME", dir.path());
        }

        let resolved = global_config_path().expect("global config path");
        assert_eq!(resolved, expected);

        unsafe {
            std::env::remove_var("CODEX_FORGE_HOME");
        }
    }
}
