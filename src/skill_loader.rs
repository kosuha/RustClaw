use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillSource {
    Global,
    Group,
}

impl SkillSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillSource::Global => "global",
            SkillSource::Group => "group",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedSkill {
    pub name: String,
    pub source: SkillSource,
    pub instructions: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillLoaderConfig {
    pub max_skills: usize,
    pub max_bytes_per_skill: usize,
}

impl Default for SkillLoaderConfig {
    fn default() -> Self {
        Self {
            max_skills: 8,
            max_bytes_per_skill: 12_000,
        }
    }
}

pub fn load_group_skills(
    groups_dir: &Path,
    group_folder: &str,
    config: &SkillLoaderConfig,
) -> Result<Vec<LoadedSkill>, String> {
    let mut merged = BTreeMap::<String, LoadedSkill>::new();

    for skill in load_skills_from_root(
        &groups_dir.join("global").join("skills"),
        SkillSource::Global,
        config.max_bytes_per_skill,
    )? {
        merged.insert(skill.name.to_ascii_lowercase(), skill);
    }

    for skill in load_skills_from_root(
        &groups_dir.join(group_folder).join("skills"),
        SkillSource::Group,
        config.max_bytes_per_skill,
    )? {
        // Group-level skill overrides global when names collide.
        merged.insert(skill.name.to_ascii_lowercase(), skill);
    }

    let mut skills = merged.into_values().collect::<Vec<_>>();
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    if skills.len() > config.max_skills {
        skills.truncate(config.max_skills);
    }
    Ok(skills)
}

pub fn render_skill_instructions(skills: &[LoadedSkill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }

    let mut rendered = String::new();
    for skill in skills {
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        rendered.push_str(&format!(
            "### Skill: {} ({})\n{}\n",
            skill.name,
            skill.source.as_str(),
            skill.instructions
        ));
    }
    Some(rendered.trim().to_string())
}

fn load_skills_from_root(
    skills_root: &Path,
    source: SkillSource,
    max_bytes_per_skill: usize,
) -> Result<Vec<LoadedSkill>, String> {
    if !skills_root.exists() {
        return Ok(Vec::new());
    }

    let skill_files = discover_skill_files(skills_root)?;
    let mut skills = Vec::<LoadedSkill>::new();
    for path in skill_files {
        let content = fs::read_to_string(&path)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        let trimmed = content.trim();
        if trimmed.is_empty() {
            continue;
        }
        let instructions = truncate_utf8(trimmed, max_bytes_per_skill).to_string();
        let name = skill_name(&path, skills_root);
        skills.push(LoadedSkill {
            name,
            source,
            instructions,
            path,
        });
    }
    Ok(skills)
}

fn discover_skill_files(skills_root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::<PathBuf>::new();

    let direct = skills_root.join("SKILL.md");
    if direct.is_file() {
        files.push(direct);
    }

    let entries = fs::read_dir(skills_root)
        .map_err(|err| format!("failed to read {}: {err}", skills_root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "failed to read entry under {}: {err}",
                skills_root.display()
            )
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let candidate = path.join("SKILL.md");
        if candidate.is_file() {
            files.push(candidate);
        }
    }

    files.sort();
    Ok(files)
}

fn skill_name(path: &Path, skills_root: &Path) -> String {
    let Ok(relative) = path.strip_prefix(skills_root) else {
        return "default".to_string();
    };

    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let mut components = parent.components();
    if let Some(component) = components.next() {
        let value = component.as_os_str().to_string_lossy().trim().to_string();
        if !value.is_empty() {
            return value;
        }
    }

    "default".to_string()
}

fn truncate_utf8(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }

    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{SkillLoaderConfig, SkillSource, load_group_skills};

    #[test]
    fn load_group_skills_merges_global_and_group_with_override() {
        let temp = tempdir().expect("tempdir");
        let groups_dir = temp.path();

        let global_research = groups_dir
            .join("global")
            .join("skills")
            .join("research")
            .join("SKILL.md");
        let group_research = groups_dir
            .join("dev")
            .join("skills")
            .join("research")
            .join("SKILL.md");
        let group_sql = groups_dir
            .join("dev")
            .join("skills")
            .join("sql")
            .join("SKILL.md");

        fs::create_dir_all(global_research.parent().expect("global parent"))
            .expect("create global skill dir");
        fs::create_dir_all(group_research.parent().expect("group parent"))
            .expect("create group skill dir");
        fs::create_dir_all(group_sql.parent().expect("group sql parent"))
            .expect("create group sql dir");

        fs::write(&global_research, "global research").expect("write global skill");
        fs::write(&group_research, "group research override").expect("write group override");
        fs::write(&group_sql, "group sql skill").expect("write group sql");

        let skills = load_group_skills(groups_dir, "dev", &SkillLoaderConfig::default())
            .expect("load skills");

        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "research");
        assert_eq!(skills[0].source, SkillSource::Group);
        assert_eq!(skills[0].instructions, "group research override");
        assert_eq!(skills[1].name, "sql");
    }

    #[test]
    fn load_group_skills_truncates_and_limits() {
        let temp = tempdir().expect("tempdir");
        let groups_dir = temp.path();

        let alpha = groups_dir
            .join("global")
            .join("skills")
            .join("alpha")
            .join("SKILL.md");
        let beta = groups_dir
            .join("global")
            .join("skills")
            .join("beta")
            .join("SKILL.md");

        fs::create_dir_all(alpha.parent().expect("alpha parent")).expect("create alpha dir");
        fs::create_dir_all(beta.parent().expect("beta parent")).expect("create beta dir");
        fs::write(&alpha, "1234567890abcdef").expect("write alpha");
        fs::write(&beta, "beta").expect("write beta");

        let skills = load_group_skills(
            groups_dir,
            "dev",
            &SkillLoaderConfig {
                max_skills: 1,
                max_bytes_per_skill: 10,
            },
        )
        .expect("load skills");

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "alpha");
        assert_eq!(skills[0].instructions, "1234567890");
    }
}
