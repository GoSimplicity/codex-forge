use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::model::RoleConfig;

const ROLE_SETS_FILE: &str = "sets.toml";

#[derive(Debug, Clone)]
pub struct ResourceOrigin {
    pub layer: &'static str,
    pub path: PathBuf,
}

impl ResourceOrigin {
    pub fn describe(&self) -> String {
        format!("{}:{}", self.layer, self.path.display())
    }
}

#[derive(Debug, Clone)]
pub struct LoadedRoleDefinition {
    pub role: RoleConfig,
    pub origin: ResourceOrigin,
}

#[derive(Debug, Clone)]
pub struct LoadedRoleSet {
    pub roles: Vec<String>,
    pub origin: ResourceOrigin,
}

#[derive(Debug, Clone)]
pub struct RuleCatalog {
    pub global: String,
    pub reviewer: Option<String>,
    pub global_origin: ResourceOrigin,
    pub reviewer_origin: Option<ResourceOrigin>,
}

#[derive(Debug, Clone)]
pub struct ResourceCatalog {
    pub roles: HashMap<String, LoadedRoleDefinition>,
    pub role_sets: HashMap<String, LoadedRoleSet>,
    pub rules: RuleCatalog,
}

#[derive(Debug, Clone)]
struct ResourceLayer {
    name: &'static str,
    root: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct RawRoleFile {
    key: Option<String>,
    title: String,
    mission: String,
    skills: Vec<String>,
    working_style: String,
    can_edit: bool,
    max_concurrency: Option<usize>,
    dependency_policy: Option<String>,
    prompt_preamble: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawRoleSetsFile {
    #[serde(default)]
    role_sets: HashMap<String, RawRoleSet>,
}

#[derive(Debug, Default, Deserialize)]
struct RawRoleSet {
    roles: Vec<String>,
}

pub fn load_resource_catalog(target_root: &Path) -> Result<ResourceCatalog> {
    let home_root = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"));
    load_resource_catalog_from_roots(
        Path::new(env!("CARGO_MANIFEST_DIR")),
        target_root,
        &home_root,
    )
}

pub fn resolve_role_set(catalog: &ResourceCatalog, name: &str) -> Result<Vec<RoleConfig>> {
    let role_set = catalog
        .role_sets
        .get(name)
        .with_context(|| format!("未找到角色集合 `{name}`，请检查 `.roles/{ROLE_SETS_FILE}`"))?;

    let mut resolved = Vec::new();
    for role_key in &role_set.roles {
        let role = catalog.roles.get(role_key).with_context(|| {
            format!(
                "角色集合 `{name}` 引用了不存在的角色 `{role_key}`，定义来源：{}",
                role_set.origin.describe()
            )
        })?;
        if role.role.key == "reviewer" && catalog.rules.reviewer.is_none() {
            bail!("角色集合 `{name}` 启用了 `reviewer`，但未找到 `.rules/reviewer.md`");
        }
        resolved.push(role.role.clone());
    }
    if resolved.is_empty() {
        bail!("角色集合 `{name}` 为空，无法运行");
    }
    Ok(resolved)
}

pub fn load_resource_catalog_from_roots(
    forge_root: &Path,
    target_root: &Path,
    home_root: &Path,
) -> Result<ResourceCatalog> {
    let layers = vec![
        ResourceLayer {
            name: "home",
            root: home_root.to_path_buf(),
        },
        ResourceLayer {
            name: "target",
            root: target_root.to_path_buf(),
        },
        ResourceLayer {
            name: "forge",
            root: forge_root.to_path_buf(),
        },
    ];

    let mut raw_roles = HashMap::<String, (RawRoleFile, ResourceOrigin)>::new();
    let mut role_sets = HashMap::<String, LoadedRoleSet>::new();
    let mut rule_map = HashMap::<String, (String, ResourceOrigin)>::new();

    for layer in &layers {
        load_roles_from_layer(layer, &mut raw_roles)?;
        load_role_sets_from_layer(layer, &mut role_sets)?;
        load_rules_from_layer(layer, &mut rule_map)?;
    }

    let (global_rule, global_origin) = rule_map
        .remove("global")
        .context("未找到 `.rules/global.md`，无法构建全局规则")?;
    let reviewer_rule = rule_map.remove("reviewer");

    let mut roles = HashMap::<String, LoadedRoleDefinition>::new();
    for (role_key, (raw_role, origin)) in raw_roles {
        let resolved_key = raw_role.key.unwrap_or_else(|| role_key.clone());
        roles.insert(
            resolved_key.clone(),
            LoadedRoleDefinition {
                role: RoleConfig {
                    key: resolved_key,
                    title: raw_role.title,
                    mission: raw_role.mission,
                    skills: raw_role.skills,
                    working_style: raw_role.working_style,
                    can_edit: raw_role.can_edit,
                    max_concurrency: raw_role.max_concurrency,
                    dependency_policy: raw_role.dependency_policy,
                    prompt_preamble: raw_role.prompt_preamble,
                },
                origin,
            },
        );
    }

    Ok(ResourceCatalog {
        roles,
        role_sets,
        rules: RuleCatalog {
            global: global_rule,
            reviewer: reviewer_rule.as_ref().map(|(content, _)| content.clone()),
            global_origin,
            reviewer_origin: reviewer_rule.map(|(_, origin)| origin),
        },
    })
}

fn load_roles_from_layer(
    layer: &ResourceLayer,
    store: &mut HashMap<String, (RawRoleFile, ResourceOrigin)>,
) -> Result<()> {
    let dir = layer.root.join(".roles");
    if !dir.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(&dir).with_context(|| format!("读取目录失败：{}", dir.display()))?
    {
        let entry = entry.with_context(|| format!("读取目录项失败：{}", dir.display()))?;
        let path = entry.path();
        if !is_toml_file(&path) {
            continue;
        }
        if path.file_name().and_then(|name| name.to_str()) == Some(ROLE_SETS_FILE) {
            continue;
        }
        let key = file_stem(&path)?;
        let content = fs::read_to_string(&path)
            .with_context(|| format!("读取角色定义失败：{}", path.display()))?;
        let raw: RawRoleFile = toml::from_str(&content)
            .with_context(|| format!("解析角色定义失败：{}", path.display()))?;
        store.insert(
            key,
            (
                raw,
                ResourceOrigin {
                    layer: layer.name,
                    path,
                },
            ),
        );
    }
    Ok(())
}

fn load_role_sets_from_layer(
    layer: &ResourceLayer,
    store: &mut HashMap<String, LoadedRoleSet>,
) -> Result<()> {
    let path = layer.root.join(".roles").join(ROLE_SETS_FILE);
    if !path.is_file() {
        return Ok(());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("读取角色集合失败：{}", path.display()))?;
    let raw: RawRoleSetsFile = toml::from_str(&content)
        .with_context(|| format!("解析角色集合失败：{}", path.display()))?;
    for (key, role_set) in raw.role_sets {
        store.insert(
            key,
            LoadedRoleSet {
                roles: role_set.roles,
                origin: ResourceOrigin {
                    layer: layer.name,
                    path: path.clone(),
                },
            },
        );
    }
    Ok(())
}

fn load_rules_from_layer(
    layer: &ResourceLayer,
    store: &mut HashMap<String, (String, ResourceOrigin)>,
) -> Result<()> {
    let dir = layer.root.join(".rules");
    if !dir.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(&dir).with_context(|| format!("读取目录失败：{}", dir.display()))?
    {
        let entry = entry.with_context(|| format!("读取目录项失败：{}", dir.display()))?;
        let path = entry.path();
        if !is_markdown_file(&path) {
            continue;
        }
        let key = file_stem(&path)?;
        let content = fs::read_to_string(&path)
            .with_context(|| format!("读取规则失败：{}", path.display()))?
            .trim()
            .to_string();
        if content.is_empty() {
            bail!("规则文件不能为空：{}", path.display());
        }
        store.insert(
            key,
            (
                content,
                ResourceOrigin {
                    layer: layer.name,
                    path,
                },
            ),
        );
    }
    Ok(())
}

fn is_markdown_file(path: &Path) -> bool {
    path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("md")
}

fn is_toml_file(path: &Path) -> bool {
    path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("toml")
}

fn file_stem(path: &Path) -> Result<String> {
    path.file_stem()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
        .with_context(|| format!("无法识别文件名：{}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{load_resource_catalog_from_roots, resolve_role_set};
    use std::fs;
    use tempfile::TempDir;

    fn write_file(path: &std::path::Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, content).expect("write file");
    }

    fn sample_role_toml(title: &str, can_edit: bool, skills: &[&str]) -> String {
        format!(
            "title = \"{title}\"\nmission = \"推进任务\"\nworking_style = \"直接推进\"\ncan_edit = {can_edit}\nskills = [{}]\n",
            skills
                .iter()
                .map(|item| format!("\"{item}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    #[test]
    fn forge_role_overrides_lower_priority_roles() {
        let forge = TempDir::new().expect("forge");
        let target = TempDir::new().expect("target");
        let home = TempDir::new().expect("home");

        write_file(
            &home.path().join(".roles/implementer.toml"),
            &sample_role_toml("家庭实现者", true, &["coding"]),
        );
        write_file(
            &target.path().join(".roles/implementer.toml"),
            &sample_role_toml("目标实现者", true, &["coding"]),
        );
        write_file(&forge.path().join(".rules/global.md"), "global");
        write_file(
            &forge.path().join(".roles/implementer.toml"),
            &sample_role_toml("仓库实现者", true, &["coding"]),
        );
        write_file(
            &forge.path().join(".roles/sets.toml"),
            "[role_sets.default]\nroles = [\"implementer\"]\n",
        );

        let catalog = load_resource_catalog_from_roots(forge.path(), target.path(), home.path())
            .expect("catalog");
        let roles = resolve_role_set(&catalog, "default").expect("role set");
        assert_eq!(roles[0].title, "仓库实现者");
    }

    #[test]
    fn target_role_overrides_home_role() {
        let forge = TempDir::new().expect("forge");
        let target = TempDir::new().expect("target");
        let home = TempDir::new().expect("home");

        write_file(&target.path().join(".rules/global.md"), "target global");
        write_file(
            &target.path().join(".roles/implementer.toml"),
            &sample_role_toml("目标实现者", true, &["coding"]),
        );
        write_file(
            &target.path().join(".roles/sets.toml"),
            "[role_sets.default]\nroles = [\"implementer\"]\n",
        );

        let catalog = load_resource_catalog_from_roots(forge.path(), target.path(), home.path())
            .expect("catalog");
        let roles = resolve_role_set(&catalog, "default").expect("role set");
        assert_eq!(roles[0].title, "目标实现者");
    }

    #[test]
    fn reviewer_role_requires_reviewer_rule() {
        let forge = TempDir::new().expect("forge");
        let target = TempDir::new().expect("target");
        let home = TempDir::new().expect("home");

        write_file(&forge.path().join(".rules/global.md"), "global");
        write_file(
            &forge.path().join(".roles/reviewer.toml"),
            &sample_role_toml("审阅者", false, &["review"]),
        );
        write_file(
            &forge.path().join(".roles/sets.toml"),
            "[role_sets.default]\nroles = [\"reviewer\"]\n",
        );

        let catalog = load_resource_catalog_from_roots(forge.path(), target.path(), home.path())
            .expect("catalog");
        let error = resolve_role_set(&catalog, "default").expect_err("should fail");
        assert!(error.to_string().contains(".rules/reviewer.md"));
    }

    #[test]
    fn resolves_all_builtin_role_sets() {
        let forge = TempDir::new().expect("forge");
        let target = TempDir::new().expect("target");
        let home = TempDir::new().expect("home");

        write_file(&forge.path().join(".rules/global.md"), "global");
        write_file(&forge.path().join(".rules/reviewer.md"), "reviewer");
        write_file(
            &forge.path().join(".roles/architect.toml"),
            &sample_role_toml(
                "架构师",
                false,
                &["system-design", "dependency-analysis", "risk-analysis"],
            ),
        );
        write_file(
            &forge.path().join(".roles/implementer.toml"),
            &sample_role_toml(
                "实现者",
                true,
                &[
                    "coding",
                    "engineering-delivery",
                    "integration-collaboration",
                ],
            ),
        );
        write_file(
            &forge.path().join(".roles/tester.toml"),
            &sample_role_toml("测试员", true, &["test-design", "verification"]),
        );
        write_file(
            &forge.path().join(".roles/reviewer.toml"),
            &sample_role_toml(
                "审阅者",
                false,
                &["code-review", "conflict-analysis", "integration-gate"],
            ),
        );
        write_file(
            &forge.path().join(".roles/sets.toml"),
            r#"[role_sets.default]
roles = ["architect", "implementer", "tester", "reviewer"]

[role_sets.fast-path]
roles = ["implementer", "reviewer"]

[role_sets.delivery]
roles = ["architect", "implementer", "reviewer"]

[role_sets.hardening]
roles = ["implementer", "tester", "reviewer"]
"#,
        );

        let catalog = load_resource_catalog_from_roots(forge.path(), target.path(), home.path())
            .expect("catalog");

        assert_eq!(
            resolve_role_set(&catalog, "default")
                .expect("default")
                .len(),
            4
        );
        assert_eq!(
            resolve_role_set(&catalog, "fast-path")
                .expect("fast-path")
                .len(),
            2
        );
        assert_eq!(
            resolve_role_set(&catalog, "delivery")
                .expect("delivery")
                .len(),
            3
        );
        assert_eq!(
            resolve_role_set(&catalog, "hardening")
                .expect("hardening")
                .len(),
            3
        );
    }
}
