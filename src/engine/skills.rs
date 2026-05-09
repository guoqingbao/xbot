use std::collections::BTreeMap;
use std::env;
use std::env::consts::OS;
use std::fs;
use std::path::{Path, PathBuf};

use crate::util::workspace_state_dir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub name: String,
    pub path: PathBuf,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct SkillsLoader {
    workspace: PathBuf,
    workspace_skills: PathBuf,
    builtin_skills: Option<PathBuf>,
}

impl SkillsLoader {
    pub fn new(workspace: impl AsRef<Path>, builtin_skills_dir: Option<PathBuf>) -> Self {
        let workspace = workspace.as_ref().to_path_buf();
        Self {
            workspace_skills: workspace_state_dir(&workspace).join("skills"),
            workspace,
            builtin_skills: builtin_skills_dir.or_else(default_builtin_skills_dir),
        }
    }

    pub fn list_skills(&self, filter_unavailable: bool) -> Vec<SkillInfo> {
        let mut skills = Vec::new();

        if self.workspace_skills.exists() {
            for entry in read_skill_dirs(&self.workspace_skills) {
                if !skills
                    .iter()
                    .any(|skill: &SkillInfo| skill.name == entry.name)
                {
                    skills.push(entry);
                }
            }
        }
        let legacy_workspace_skills = self.workspace.join("skills");
        if legacy_workspace_skills.exists() {
            for entry in read_skill_dirs(&legacy_workspace_skills) {
                if !skills
                    .iter()
                    .any(|skill: &SkillInfo| skill.name == entry.name)
                {
                    skills.push(entry);
                }
            }
        }

        if let Some(builtin) = &self.builtin_skills {
            if builtin.exists() {
                for entry in read_skill_dirs(builtin) {
                    if !skills
                        .iter()
                        .any(|skill: &SkillInfo| skill.name == entry.name)
                    {
                        skills.push(entry);
                    }
                }
            }
        }

        if filter_unavailable {
            skills
                .into_iter()
                .filter(|skill| {
                    self.get_skill_metadata(&skill.name)
                        .map(|meta| self.check_requirements(&self.get_skill_meta_map(&meta)))
                        .unwrap_or(true)
                })
                .collect()
        } else {
            skills
        }
    }

    pub fn load_skill(&self, name: &str) -> Option<String> {
        let workspace_skill = self.workspace_skills.join(name).join("SKILL.md");
        if workspace_skill.exists() {
            return fs::read_to_string(workspace_skill).ok();
        }
        let legacy_workspace_skill = self.workspace.join("skills").join(name).join("SKILL.md");
        if legacy_workspace_skill.exists() {
            return fs::read_to_string(legacy_workspace_skill).ok();
        }
        let builtin_skill = self
            .builtin_skills
            .as_ref()
            .map(|dir| dir.join(name).join("SKILL.md"))?;
        fs::read_to_string(builtin_skill).ok()
    }

    pub fn load_skills_for_context(&self, skill_names: &[String]) -> String {
        let mut parts = Vec::new();
        for name in skill_names {
            if let Some(content) = self.load_skill(name) {
                parts.push(format!(
                    "### Skill: {}\n\n{}",
                    name,
                    self.strip_frontmatter(&content)
                ));
            }
        }
        parts.join("\n\n---\n\n")
    }

    pub fn build_skills_summary(&self) -> String {
        let skills = self.list_skills(false);
        if skills.is_empty() {
            return String::new();
        }
        let mut lines = vec!["<skills>".to_string()];
        for skill in skills {
            let desc = escape_xml(self.get_skill_description(&skill.name));
            let metadata = self.get_skill_metadata(&skill.name).unwrap_or_default();
            let meta = self.get_skill_meta_map(&metadata);
            let available = self.check_requirements(&meta);
            lines.push(format!(
                "  <skill available=\"{}\">",
                if available { "true" } else { "false" }
            ));
            lines.push(format!(
                "    <name>{}</name>",
                escape_xml(skill.name.clone())
            ));
            lines.push(format!("    <description>{desc}</description>"));
            lines.push(format!("    <location>{}</location>", skill.path.display()));
            if !available {
                let missing = self.get_missing_requirements(&meta);
                if !missing.is_empty() {
                    lines.push(format!("    <requires>{}</requires>", escape_xml(missing)));
                }
            }
            lines.push("  </skill>".to_string());
        }
        lines.push("</skills>".to_string());
        lines.join("\n")
    }

    pub fn get_always_skills(&self) -> Vec<String> {
        self.list_skills(true)
            .into_iter()
            .filter_map(|skill| {
                let metadata = self.get_skill_metadata(&skill.name)?;
                let meta = self.get_skill_meta_map(&metadata);
                if meta
                    .get("always")
                    .map(|value| value == "true")
                    .unwrap_or_else(|| {
                        metadata
                            .get("always")
                            .map(|value| value == "true")
                            .unwrap_or(false)
                    })
                {
                    Some(skill.name)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn suggest_skills(&self, prompt: &str, limit: usize) -> Vec<String> {
        let lowered = prompt.to_ascii_lowercase();
        let mut matches = Vec::new();
        for skill in self.list_skills(true) {
            let Some(metadata) = self.get_skill_metadata(&skill.name) else {
                continue;
            };
            let meta = self.get_skill_meta_map(&metadata);
            let triggers = meta
                .get("triggers")
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                .and_then(|value| value.as_array().cloned())
                .unwrap_or_default();
            if triggers
                .iter()
                .filter_map(|value| value.as_str())
                .any(|trigger| {
                    let trigger = trigger.to_ascii_lowercase();
                    lowered.contains(&trigger)
                })
            {
                matches.push(skill.name.clone());
            }
            if matches.len() >= limit {
                break;
            }
        }
        matches
    }

    pub fn get_skill_metadata(&self, name: &str) -> Option<BTreeMap<String, String>> {
        let content = self.load_skill(name)?;
        parse_frontmatter(&content)
    }

    /// Returns the `description` from frontmatter, or the skill name if missing.
    pub fn get_skill_description(&self, name: &str) -> String {
        self.get_skill_metadata(name)
            .and_then(|m| m.get("description").cloned())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| name.to_string())
    }

    /// Parses `allowed-tools` from frontmatter (comma-separated tool names).
    pub fn get_allowed_tools(&self, name: &str) -> Option<Vec<String>> {
        let raw = self.get_skill_metadata(name)?.get("allowed-tools")?.clone();
        let tools: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if tools.is_empty() { None } else { Some(tools) }
    }

    fn get_skill_meta_map(&self, metadata: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        metadata
            .get("metadata")
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .map(|value| skill_meta_map_from_metadata_json(&value))
            .unwrap_or_default()
    }

    fn check_requirements(&self, meta: &BTreeMap<String, String>) -> bool {
        let requires = meta.get("requires").cloned().unwrap_or_default();
        if !requires.is_empty() {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&requires) {
                if !check_requires_json(&parsed) {
                    return false;
                }
            }
        }
        if let Some(raw) = meta.get("os") {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
                if let Some(os_list) = value.as_array() {
                    if !os_requirement_satisfied(os_list) {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn get_missing_requirements(&self, meta: &BTreeMap<String, String>) -> String {
        let mut missing = Vec::new();
        let requires = meta.get("requires").cloned().unwrap_or_default();
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&requires) {
            append_missing_from_requires(&parsed, &mut missing);
        }
        if let Some(raw) = meta.get("os") {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
                if let Some(os_list) = value.as_array() {
                    if !os_requirement_satisfied(os_list) {
                        missing.push(format!(
                            "OS: one of {} (current: {})",
                            os_list
                                .iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(", "),
                            OS
                        ));
                    }
                }
            }
        }
        missing.join(", ")
    }

    fn strip_frontmatter(&self, content: &str) -> String {
        if let Some((_, body)) = split_frontmatter(content) {
            body.trim().to_string()
        } else {
            content.to_string()
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
}

fn check_requires_json(parsed: &serde_json::Value) -> bool {
    if let Some(bins) = parsed.get("bins").and_then(|value| value.as_array()) {
        for bin in bins.iter().filter_map(|value| value.as_str()) {
            if which(bin).is_none() {
                return false;
            }
        }
    }
    if let Some(envs) = parsed.get("env").and_then(|value| value.as_array()) {
        for key in envs.iter().filter_map(|value| value.as_str()) {
            if env::var_os(key).is_none() {
                return false;
            }
        }
    }
    if let Some(os_list) = parsed.get("os").and_then(|value| value.as_array()) {
        if !os_requirement_satisfied(os_list) {
            return false;
        }
    }
    true
}

fn append_missing_from_requires(parsed: &serde_json::Value, missing: &mut Vec<String>) {
    if let Some(bins) = parsed.get("bins").and_then(|value| value.as_array()) {
        for bin in bins.iter().filter_map(|value| value.as_str()) {
            if which(bin).is_none() {
                missing.push(format!("CLI: {bin}"));
            }
        }
    }
    if let Some(envs) = parsed.get("env").and_then(|value| value.as_array()) {
        for key in envs.iter().filter_map(|value| value.as_str()) {
            if env::var_os(key).is_none() {
                missing.push(format!("ENV: {key}"));
            }
        }
    }
    if let Some(os_list) = parsed.get("os").and_then(|value| value.as_array()) {
        if !os_requirement_satisfied(os_list) {
            missing.push(format!(
                "OS: one of {} (current: {})",
                os_list
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                OS
            ));
        }
    }
}

/// Validates a skill directory layout and frontmatter. Returns human-readable issues (empty if valid).
pub fn validate_skill(skill_dir: &Path) -> Vec<String> {
    let mut issues = Vec::new();
    let skill_md = skill_dir.join("SKILL.md");
    if !skill_md.is_file() {
        issues.push("SKILL.md is missing".to_string());
        return issues;
    }
    if is_symlink_path(skill_dir) {
        issues.push("skill directory must not be a symlink".to_string());
    }
    if is_symlink_path(&skill_md) {
        issues.push("SKILL.md must not be a symlink".to_string());
    }

    let Ok(content) = fs::read_to_string(&skill_md) else {
        issues.push("SKILL.md is not readable".to_string());
        return issues;
    };
    let Some(fm) = parse_frontmatter(&content) else {
        issues.push("SKILL.md must start with YAML frontmatter between --- delimiters".to_string());
        return issues;
    };

    let dir_name = skill_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let name = fm.get("name").map(|s| s.trim()).filter(|s| !s.is_empty());
    let description = fm
        .get("description")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    if name.is_none() {
        issues.push("frontmatter must include non-empty `name`".to_string());
    }
    if description.is_none() {
        issues.push("frontmatter must include non-empty `description`".to_string());
    }
    if let Some(n) = name {
        if n != dir_name {
            issues.push(format!(
                "`name` in frontmatter ({n}) must match directory name ({dir_name})"
            ));
        }
        if n.len() > 64 {
            issues.push(format!(
                "`name` must be at most 64 characters (got {})",
                n.len()
            ));
        }
        if !is_valid_skill_name(n) {
            issues.push(
                "`name` must be lowercase letters, digits, and hyphens only (e.g. my-skill)"
                    .to_string(),
            );
        }
    }

    validate_skill_entries(skill_dir, &mut issues);
    issues
}

fn validate_skill_entries(skill_dir: &Path, issues: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(skill_dir) else {
        issues.push("cannot read skill directory".to_string());
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_symlink_path(&path) {
            issues.push(format!(
                "symlinks are not allowed: {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            continue;
        }
        let fname = entry.file_name();
        let lossy = fname.to_string_lossy();
        if lossy == "SKILL.md" {
            continue;
        }
        if path.is_dir() {
            match lossy.as_ref() {
                "scripts" | "references" | "assets" => {
                    if walk_has_symlink(&path) {
                        issues.push(format!(
                            "symlinks are not allowed under {}/",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        ));
                    }
                }
                _ => issues.push(format!(
                    "unexpected directory `{lossy}` (allowed: scripts/, references/, assets/)"
                )),
            }
        } else {
            issues.push(format!("unexpected file `{lossy}` at skill root"));
        }
    }
}

fn walk_has_symlink(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_symlink_path(&path) {
            return true;
        }
        if path.is_dir() && walk_has_symlink(&path) {
            return true;
        }
    }
    false
}

fn is_symlink_path(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn is_valid_skill_name(name: &str) -> bool {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return false;
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return false;
    }
    true
}

fn skill_meta_map_from_metadata_json(value: &serde_json::Value) -> BTreeMap<String, String> {
    for key in ["xbot", "nanobot", "openclaw"] {
        if let Some(obj) = value.get(key) {
            if let Some(map) = json_object_to_string_map(obj) {
                if !map.is_empty() {
                    return map;
                }
            }
        }
    }
    BTreeMap::new()
}

fn json_object_to_string_map(value: &serde_json::Value) -> Option<BTreeMap<String, String>> {
    let obj = value.as_object()?;
    Some(
        obj.iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    value
                        .as_str()
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| value.to_string()),
                )
            })
            .collect(),
    )
}

fn os_requirement_satisfied(os_list: &[serde_json::Value]) -> bool {
    let names: Vec<&str> = os_list.iter().filter_map(|v| v.as_str()).collect();
    if names.is_empty() {
        return true;
    }
    names
        .iter()
        .any(|&required| os_name_matches_current(required))
}

fn os_name_matches_current(required: &str) -> bool {
    let cur = OS;
    required == cur
        || (required == "darwin" && cur == "macos")
        || (required == "macos" && cur == "darwin")
}

fn default_builtin_skills_dir() -> Option<PathBuf> {
    first_existing_builtin_skills_dir(default_builtin_skill_dir_candidates())
}

fn default_builtin_skill_dir_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = env::var_os("XBOT_BUILTIN_SKILLS_DIR") {
        if !dir.is_empty() {
            candidates.push(PathBuf::from(dir));
        }
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("skills"));
    if let Ok(exe) = env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            candidates.push(bin_dir.join("../share/xbot/skills"));
        }
    }
    candidates.push(PathBuf::from("/usr/share/xbot/skills"));
    candidates
}

fn first_existing_builtin_skills_dir(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|dir| dir.exists())
}

fn read_skill_dirs(dir: &Path) -> Vec<SkillInfo> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_file = path.join("SKILL.md");
        if !skill_file.exists() {
            continue;
        }
        out.push(SkillInfo {
            name: entry.file_name().to_string_lossy().to_string(),
            path: skill_file,
            source: if dir.ends_with("skills") && dir.parent().is_some() {
                if dir.ends_with("skills") && dir.to_string_lossy().contains(".xbot") {
                    "workspace".to_string()
                } else {
                    "builtin".to_string()
                }
            } else {
                "builtin".to_string()
            },
        });
    }
    out
}

fn split_frontmatter(content: &str) -> Option<(String, String)> {
    let mut lines = content.lines();
    if lines.next()? != "---" {
        return None;
    }
    let mut frontmatter = Vec::new();
    let mut body = Vec::new();
    let mut in_frontmatter = true;
    for line in content.lines().skip(1) {
        if in_frontmatter && line == "---" {
            in_frontmatter = false;
            continue;
        }
        if in_frontmatter {
            frontmatter.push(line);
        } else {
            body.push(line);
        }
    }
    (!in_frontmatter).then(|| (frontmatter.join("\n"), body.join("\n")))
}

fn parse_frontmatter(content: &str) -> Option<BTreeMap<String, String>> {
    let (frontmatter, _) = split_frontmatter(content)?;
    Some(parse_frontmatter_body(&frontmatter))
}

fn parse_frontmatter_body(frontmatter: &str) -> BTreeMap<String, String> {
    let lines: Vec<&str> = frontmatter.lines().collect();
    let mut i = 0;
    let mut map = BTreeMap::new();
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        let Some(colon) = trimmed.find(':') else {
            i += 1;
            continue;
        };
        let key = trimmed[..colon].trim();
        if key.is_empty() {
            i += 1;
            continue;
        }
        let mut value = trimmed[colon + 1..].trim().to_string();
        i += 1;

        if value.is_empty() && i < lines.len() {
            let nt = lines[i].trim_start();
            if nt.starts_with('{') || nt.starts_with('[') {
                value = nt.to_string();
                i += 1;
            }
        }

        if value == "|" || value == ">" || value == ">-" {
            let base_indent = line.len().saturating_sub(line.trim_start().len());
            let mut parts = Vec::new();
            while i < lines.len() {
                let l = lines[i];
                if l.trim().is_empty() {
                    parts.push(String::new());
                    i += 1;
                    continue;
                }
                let ind = l.len().saturating_sub(l.trim_start().len());
                let t = l.trim_start();
                if ind <= base_indent && t.contains(':') {
                    if let Some((k, _)) = t.split_once(':') {
                        let key = k.trim();
                        if !key.is_empty()
                            && key
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                        {
                            break;
                        }
                    }
                }
                parts.push(t.to_string());
                i += 1;
            }
            value = parts.join("\n");
        } else if (value.starts_with('{') || value.starts_with('['))
            && !json_container_balanced(&value)
        {
            while i < lines.len() && !json_container_balanced(&value) {
                value.push('\n');
                value.push_str(lines[i]);
                i += 1;
            }
        }

        map.insert(key.to_string(), normalize_frontmatter_value(&value));
    }
    map
}

fn json_container_balanced(s: &str) -> bool {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    for c in s.chars() {
        if in_string {
            if escape {
                escape = false;
                continue;
            }
            if c == '\\' {
                escape = true;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                if stack.pop() != Some(c) {
                    return false;
                }
            }
            _ => {}
        }
    }
    stack.is_empty() && !in_string
}

fn normalize_frontmatter_value(raw: &str) -> String {
    let t = raw.trim();
    if (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']')) {
        if json_container_balanced(t) {
            return t.to_string();
        }
    }
    if t.len() >= 2
        && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\'')))
    {
        return t[1..t.len() - 1].to_string();
    }
    t.to_string()
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let candidate_exe = dir.join(format!("{bin}.exe"));
            if candidate_exe.is_file() {
                return Some(candidate_exe);
            }
        }
        None
    })
}

fn escape_xml(value: String) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::{
        SkillsLoader, default_builtin_skill_dir_candidates, first_existing_builtin_skills_dir,
        validate_skill,
    };
    use serial_test::serial;
    use tempfile::tempdir;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[test]
    fn builtin_skill_dir_uses_first_existing_candidate() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("missing");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        assert_eq!(
            first_existing_builtin_skills_dir(vec![missing, first.clone(), second]),
            Some(first)
        );
    }

    #[test]
    #[serial]
    fn builtin_skill_dir_candidates_prefer_env_override() {
        let dir = tempdir().unwrap();
        let skills = dir.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let previous = std::env::var_os("XBOT_BUILTIN_SKILLS_DIR");
        unsafe {
            std::env::set_var("XBOT_BUILTIN_SKILLS_DIR", &skills);
        }

        let candidates = default_builtin_skill_dir_candidates();

        match previous {
            Some(value) => unsafe {
                std::env::set_var("XBOT_BUILTIN_SKILLS_DIR", value);
            },
            None => unsafe {
                std::env::remove_var("XBOT_BUILTIN_SKILLS_DIR");
            },
        }
        assert_eq!(candidates.first(), Some(&skills));
    }

    #[test]
    fn workspace_skill_overrides_builtin_skill() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let builtin = dir.path().join("builtin");
        std::fs::create_dir_all(workspace.join(".xbot/skills/echo")).unwrap();
        std::fs::create_dir_all(builtin.join("echo")).unwrap();
        std::fs::write(
            workspace.join(".xbot/skills/echo/SKILL.md"),
            "---\ndescription: Workspace Echo\n---\nworkspace skill",
        )
        .unwrap();
        std::fs::write(
            builtin.join("echo/SKILL.md"),
            "---\ndescription: Builtin Echo\n---\nbuiltin skill",
        )
        .unwrap();

        let loader = SkillsLoader::new(&workspace, Some(builtin));
        let skills = loader.list_skills(false);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "echo");
        assert!(
            loader
                .load_skill("echo")
                .unwrap()
                .contains("workspace skill")
        );
    }

    #[test]
    fn summary_reflects_xbot_metadata() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let builtin = dir.path().join("builtin");
        std::fs::create_dir_all(builtin.join("always")).unwrap();
        std::fs::write(
            builtin.join("always/SKILL.md"),
            "---\ndescription: Always skill\nmetadata: {\"xbot\":{\"always\":true}}\n---\nhello",
        )
        .unwrap();

        let loader = SkillsLoader::new(&workspace, Some(builtin));
        let summary = loader.build_skills_summary();
        assert!(summary.contains("<skills>"));
        assert!(summary.contains("Always skill"));
        assert_eq!(loader.get_always_skills(), vec!["always".to_string()]);
        assert_eq!(loader.get_skill_description("always"), "Always skill");
        assert!(
            loader
                .load_skills_for_context(&["always".to_string()])
                .contains("### Skill: always")
        );
    }

    #[test]
    fn nanobot_metadata_fallback() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let builtin = dir.path().join("builtin");
        std::fs::create_dir_all(builtin.join("nb")).unwrap();
        std::fs::write(
            builtin.join("nb/SKILL.md"),
            "---\ndescription: Nanobot skill\nmetadata: {\"nanobot\":{\"always\":true}}\n---\n",
        )
        .unwrap();
        let loader = SkillsLoader::new(&workspace, Some(builtin));
        assert_eq!(loader.get_always_skills(), vec!["nb".to_string()]);
    }

    #[test]
    fn openclaw_metadata_fallback() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let builtin = dir.path().join("builtin");
        std::fs::create_dir_all(builtin.join("oc")).unwrap();
        std::fs::write(
            builtin.join("oc/SKILL.md"),
            "---\ndescription: OC\nmetadata: {\"openclaw\":{\"always\":true}}\n---\n",
        )
        .unwrap();
        let loader = SkillsLoader::new(&workspace, Some(builtin));
        assert_eq!(loader.get_always_skills(), vec!["oc".to_string()]);
    }

    #[test]
    fn xbot_takes_precedence_over_nanobot() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let builtin = dir.path().join("builtin");
        std::fs::create_dir_all(builtin.join("both")).unwrap();
        std::fs::write(
            builtin.join("both/SKILL.md"),
            "---\ndescription: Both\nmetadata: {\"xbot\":{\"always\":false},\"nanobot\":{\"always\":true}}\n---\n",
        )
        .unwrap();
        let loader = SkillsLoader::new(&workspace, Some(builtin));
        assert!(loader.get_always_skills().is_empty());
    }

    #[test]
    fn metadata_json_with_colons_in_strings() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let builtin = dir.path().join("builtin");
        std::fs::create_dir_all(builtin.join("colon")).unwrap();
        std::fs::write(
            builtin.join("colon/SKILL.md"),
            r#"---
description: Colon skill
metadata: {"xbot":{"always":true,"triggers":["https://example.com:8080/path"]}}
---
"#,
        )
        .unwrap();
        let loader = SkillsLoader::new(&workspace, Some(builtin));
        assert_eq!(loader.get_always_skills(), vec!["colon".to_string()]);
        let summary = loader.build_skills_summary();
        assert!(summary.contains("Colon skill"));
    }

    #[test]
    fn os_requirement_filters_unavailable_skill() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let builtin = dir.path().join("builtin");
        std::fs::create_dir_all(builtin.join("linux-only")).unwrap();
        std::fs::write(
            builtin.join("linux-only/SKILL.md"),
            r#"---
description: Linux only
metadata: {"xbot":{"requires":"{\"os\":[\"__not_a_real_os__\"]}"}}
---
"#,
        )
        .unwrap();
        let loader = SkillsLoader::new(&workspace, Some(builtin));
        assert!(
            !loader
                .list_skills(true)
                .iter()
                .any(|s| s.name == "linux-only")
        );
        let all = loader.list_skills(false);
        assert!(all.iter().any(|s| s.name == "linux-only"));
        let summary = loader.build_skills_summary();
        assert!(summary.contains("available=\"false\""));
        assert!(summary.contains("linux-only"));
    }

    #[test]
    fn multiline_metadata_json() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let builtin = dir.path().join("builtin");
        std::fs::create_dir_all(builtin.join("ml")).unwrap();
        let md = r#"---
description: ML
metadata: {
  "xbot": {
    "always": true
  }
}
---
"#;
        std::fs::write(builtin.join("ml/SKILL.md"), md).unwrap();
        let loader = SkillsLoader::new(&workspace, Some(builtin));
        assert_eq!(loader.get_always_skills(), vec!["ml".to_string()]);
    }

    #[test]
    fn get_allowed_tools_parses_comma_list() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let builtin = dir.path().join("builtin");
        std::fs::create_dir_all(builtin.join("tools")).unwrap();
        std::fs::write(
            builtin.join("tools/SKILL.md"),
            "---\ndescription: T\nallowed-tools: read, write, bash\n---\n",
        )
        .unwrap();
        let loader = SkillsLoader::new(&workspace, Some(builtin));
        assert_eq!(
            loader.get_allowed_tools("tools"),
            Some(vec![
                "read".to_string(),
                "write".to_string(),
                "bash".to_string()
            ])
        );
    }

    #[test]
    fn validate_skill_accepts_minimal_valid_layout() {
        let dir = tempdir().unwrap();
        let skill = dir.path().join("my-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: my-skill\ndescription: A test skill\n---\n",
        )
        .unwrap();
        assert!(validate_skill(&skill).is_empty());
    }

    #[test]
    fn validate_skill_reports_name_mismatch_and_bad_dirs() {
        let dir = tempdir().unwrap();
        let skill = dir.path().join("good-name");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: wrong-name\ndescription: x\n---\n",
        )
        .unwrap();
        let issues = validate_skill(&skill);
        assert!(issues.iter().any(|i| i.contains("match directory")));

        let skill2 = dir.path().join("ok-skill");
        std::fs::create_dir_all(skill2.join("scripts")).unwrap();
        std::fs::write(
            skill2.join("SKILL.md"),
            "---\nname: ok-skill\ndescription: y\n---\n",
        )
        .unwrap();
        std::fs::write(skill2.join("extra.txt"), "nope").unwrap();
        let issues2 = validate_skill(&skill2);
        assert!(issues2.iter().any(|i| i.contains("extra.txt")));
    }

    #[test]
    #[cfg(unix)]
    fn validate_skill_rejects_symlink() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("real-skill");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(
            target.join("SKILL.md"),
            "---\nname: real-skill\ndescription: z\n---\n",
        )
        .unwrap();
        let link = dir.path().join("link-skill");
        symlink(&target, &link).unwrap();
        let issues = validate_skill(&link);
        assert!(issues.iter().any(|i| i.contains("symlink")));
    }
}
