use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::model::ApplyMode;

pub const DEFAULT_RUST_VERIFICATION_COMMANDS: &[&str] = &[
    "cargo fmt --check",
    "cargo clippy --all-targets --all-features -- -D warnings",
    "cargo test",
];

#[derive(Debug, Clone)]
pub struct LoadedProjectConfig {
    pub path: Option<PathBuf>,
    pub settings: ProjectSettings,
}

#[derive(Debug, Clone)]
pub struct ProjectSettings {
    pub role_set: String,
    pub model: Option<String>,
    pub workers: usize,
    pub apply_mode: ApplyMode,
    pub max_retries: usize,
    pub fail_fast: bool,
    pub cleanup_success: bool,
    pub verification_commands: Vec<String>,
    pub role_overrides: HashMap<String, RoleOverride>,
}

#[derive(Debug, Clone, Default)]
pub struct RoleOverride {
    pub title: Option<String>,
    pub mission: Option<String>,
    pub skills: Option<Vec<String>>,
    pub working_style: Option<String>,
    pub can_edit: Option<bool>,
    pub max_concurrency: Option<usize>,
    pub dependency_policy: Option<String>,
    pub prompt_preamble: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProjectConfig {
    #[serde(default)]
    defaults: RawDefaults,
    #[serde(default)]
    roles: HashMap<String, RawRoleOverride>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDefaults {
    role_set: Option<String>,
    model: Option<String>,
    workers: Option<usize>,
    apply_mode: Option<ApplyModeSerde>,
    max_retries: Option<usize>,
    fail_fast: Option<bool>,
    cleanup_success: Option<bool>,
    verification_commands: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ApplyModeSerde {
    AutoSafe,
    Bundle,
    None,
}

impl From<ApplyModeSerde> for ApplyMode {
    fn from(value: ApplyModeSerde) -> Self {
        match value {
            ApplyModeSerde::AutoSafe => ApplyMode::AutoSafe,
            ApplyModeSerde::Bundle => ApplyMode::Bundle,
            ApplyModeSerde::None => ApplyMode::None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawRoleOverride {
    title: Option<String>,
    mission: Option<String>,
    skills: Option<Vec<String>>,
    working_style: Option<String>,
    can_edit: Option<bool>,
    max_concurrency: Option<usize>,
    dependency_policy: Option<String>,
    prompt_preamble: Option<String>,
}

pub fn load_project_config(
    target_dir: &Path,
    explicit_path: Option<&Path>,
) -> Result<LoadedProjectConfig> {
    let config_path = match explicit_path {
        Some(path) => Some(path.to_path_buf()),
        None => {
            let candidate = target_dir.join("codex-forge.toml");
            if candidate.exists() {
                Some(candidate)
            } else {
                None
            }
        }
    };

    if let Some(path) = config_path {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
        let raw: RawProjectConfig = toml::from_str(&content)
            .with_context(|| format!("解析 TOML 配置失败：{}", path.display()))?;
        let settings = validate_and_build(raw)?;
        Ok(LoadedProjectConfig {
            path: Some(path),
            settings,
        })
    } else {
        Ok(LoadedProjectConfig {
            path: None,
            settings: ProjectSettings::default(),
        })
    }
}

pub fn validate_project_config(
    target_dir: &Path,
    explicit_path: Option<&Path>,
) -> Result<LoadedProjectConfig> {
    load_project_config(target_dir, explicit_path)
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            role_set: "core".to_string(),
            model: None,
            workers: 4,
            apply_mode: ApplyMode::AutoSafe,
            max_retries: 2,
            fail_fast: false,
            cleanup_success: false,
            verification_commands: DEFAULT_RUST_VERIFICATION_COMMANDS
                .iter()
                .map(|item| item.to_string())
                .collect(),
            role_overrides: HashMap::new(),
        }
    }
}

fn validate_and_build(raw: RawProjectConfig) -> Result<ProjectSettings> {
    let defaults = ProjectSettings::default();
    let workers = raw.defaults.workers.unwrap_or(defaults.workers).max(1);
    let max_retries = raw.defaults.max_retries.unwrap_or(defaults.max_retries);
    if max_retries > 8 {
        bail!("max_retries 过大（>{}），请使用更合理的值", 8);
    }

    let verification_commands = raw
        .defaults
        .verification_commands
        .unwrap_or(defaults.verification_commands.clone());
    if verification_commands.is_empty() {
        bail!("verification_commands 不能为空");
    }

    let role_overrides = raw
        .roles
        .into_iter()
        .map(|(key, value)| {
            (
                key,
                RoleOverride {
                    title: value.title,
                    mission: value.mission,
                    skills: value.skills,
                    working_style: value.working_style,
                    can_edit: value.can_edit,
                    max_concurrency: value.max_concurrency,
                    dependency_policy: value.dependency_policy,
                    prompt_preamble: value.prompt_preamble,
                },
            )
        })
        .collect::<HashMap<_, _>>();

    Ok(ProjectSettings {
        role_set: raw.defaults.role_set.unwrap_or(defaults.role_set),
        model: raw.defaults.model.or(defaults.model),
        workers,
        apply_mode: raw
            .defaults
            .apply_mode
            .map(ApplyMode::from)
            .unwrap_or(defaults.apply_mode),
        max_retries,
        fail_fast: raw.defaults.fail_fast.unwrap_or(defaults.fail_fast),
        cleanup_success: raw
            .defaults
            .cleanup_success
            .unwrap_or(defaults.cleanup_success),
        verification_commands,
        role_overrides,
    })
}

#[cfg(test)]
mod tests {
    use super::load_project_config;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn loads_defaults_without_file() {
        let dir = TempDir::new().expect("tempdir");
        let loaded = load_project_config(dir.path(), None).expect("load config");
        assert!(loaded.path.is_none());
        assert_eq!(loaded.settings.workers, 4);
        assert_eq!(loaded.settings.apply_mode.label(), "auto-safe");
    }

    #[test]
    fn loads_and_overrides_values() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("codex-forge.toml");
        fs::write(
            &path,
            r#"
[defaults]
workers = 2
apply_mode = "bundle"
max_retries = 1
verification_commands = ["cargo test -q"]

[roles.reviewer]
can_edit = false
working_style = "只做审阅"
"#,
        )
        .expect("write config");

        let loaded = load_project_config(dir.path(), None).expect("load config");
        assert_eq!(loaded.settings.workers, 2);
        assert_eq!(loaded.settings.apply_mode.label(), "bundle");
        assert_eq!(loaded.settings.max_retries, 1);
        assert_eq!(
            loaded
                .settings
                .role_overrides
                .get("reviewer")
                .and_then(|role| role.can_edit),
            Some(false)
        );
    }
}
