use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::BackendProvider;
use crate::harness::types::SkillSummary;

#[derive(Debug, Default, Clone, Copy)]
pub struct SkillAdapter;

impl SkillAdapter {
    pub fn list(provider: BackendProvider) -> Vec<SkillSummary> {
        let mut skills = discover_skill_dirs(provider)
            .into_iter()
            .flat_map(find_skill_files)
            .filter_map(read_skill_summary)
            .collect::<Vec<_>>();
        skills.sort_by(|left, right| left.name.cmp(&right.name));
        skills.dedup_by(|left, right| left.path == right.path || left.name == right.name);
        skills
    }

    pub fn find_by_name(provider: BackendProvider, name: &str) -> Option<SkillSummary> {
        let wanted = name.trim().to_lowercase();
        Self::list(provider).into_iter().find(|skill| {
            skill.name.to_lowercase() == wanted
                || skill
                    .path
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(&wanted)
        })
    }

    pub fn read_body(provider: BackendProvider, name_or_path: &str) -> Option<String> {
        if let Some(skill) = Self::find_by_name(provider, name_or_path) {
            return fs::read_to_string(skill.path).ok();
        }
        let path = PathBuf::from(name_or_path);
        fs::read_to_string(path).ok()
    }
}

fn discover_skill_dirs(provider: BackendProvider) -> Vec<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let mut dirs = Vec::new();
    if let Some(home) = home {
        let path = match provider {
            BackendProvider::Codex => home.join(".codex").join("skills"),
            BackendProvider::OpenAiCompatible => home.join(".agents").join("skills"),
        };
        dirs.push(path);
    }
    dirs
}

fn find_skill_files(root: PathBuf) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    walkdir::WalkDir::new(root)
        .max_depth(3)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.file_name() == "SKILL.md")
        .map(|entry| entry.path().to_path_buf())
        .collect()
}

fn read_skill_summary(path: PathBuf) -> Option<SkillSummary> {
    let raw = fs::read_to_string(&path).ok()?;
    let mut name = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|item| item.to_str())
        .unwrap_or("skill")
        .to_string();
    let mut description = String::new();

    for line in raw.lines().take(20) {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("name:") {
            name = value.trim().trim_matches('"').to_string();
        } else if let Some(value) = trimmed.strip_prefix("description:") {
            description = value.trim().trim_matches('"').to_string();
        }
    }

    if description.is_empty() {
        description = "本地可用 skill".to_string();
    }

    Some(SkillSummary {
        name,
        description,
        path,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::config::BackendProvider;

    use super::SkillAdapter;

    #[test]
    fn discovers_skills_from_provider_specific_home_dirs() {
        let dir = TempDir::new().expect("tempdir");
        let codex_skill_dir = dir.path().join(".codex").join("skills").join("demo");
        let agents_skill_dir = dir.path().join(".agents").join("skills").join("agent-demo");
        fs::create_dir_all(&codex_skill_dir).expect("mkdir codex");
        fs::create_dir_all(&agents_skill_dir).expect("mkdir agents");
        fs::write(
            codex_skill_dir.join("SKILL.md"),
            "---\nname: demo-skill\ndescription: demo desc\n---\nbody",
        )
        .expect("write codex");
        fs::write(
            agents_skill_dir.join("SKILL.md"),
            "---\nname: agent-skill\ndescription: agent desc\n---\nbody",
        )
        .expect("write agents");
        unsafe {
            std::env::set_var("HOME", dir.path());
        }
        let codex_skills = SkillAdapter::list(BackendProvider::Codex);
        let openai_skills = SkillAdapter::list(BackendProvider::OpenAiCompatible);
        assert!(codex_skills.iter().any(|skill| skill.name == "demo-skill"));
        assert!(
            openai_skills
                .iter()
                .any(|skill| skill.name == "agent-skill")
        );
        assert!(!openai_skills.iter().any(|skill| skill.name == "demo-skill"));
        unsafe {
            std::env::remove_var("HOME");
        }
    }
}
