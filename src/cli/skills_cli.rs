use std::fs;
use std::path::Path;

use anyhow::Result;
use console::Style;

use xbot::config::Config;
use xbot::engine::SkillsLoader;

pub async fn run_skills_list(config_path: Option<&Path>) -> Result<()> {
    let config = Config::load(config_path)?;
    let workspace = config.workspace_path();
    let loader = SkillsLoader::new(&workspace, None);

    println!(
        "{}",
        Style::new().cyan().bold().apply_to("─ Available Skills")
    );
    println!();

    let all_skills = loader.list_skills(false);
    let available_skills = loader.list_skills(true);
    let available_names: Vec<_> = available_skills.iter().map(|s| &s.name).collect();

    for skill in &all_skills {
        let desc = loader.get_skill_description(&skill.name);
        let is_available = available_names.contains(&&skill.name);
        let always = loader.get_always_skills().iter().any(|n| n == &skill.name);

        let status = if !is_available {
            Style::new().red().apply_to("unavailable")
        } else if always {
            Style::new().green().apply_to("always-on")
        } else {
            Style::new().dim().apply_to("available")
        };

        println!(
            "  {:<24} [{status}]  {}",
            Style::new().bold().apply_to(&skill.name),
            Style::new().dim().apply_to(&desc),
        );
        println!(
            "  {:<24} source: {}",
            "",
            Style::new().dim().apply_to(&skill.source),
        );
    }

    println!(
        "\n{} skills ({} available).",
        all_skills.len(),
        available_skills.len()
    );
    Ok(())
}

pub async fn run_skills_init(name: &str, config_path: Option<&Path>) -> Result<()> {
    let config = Config::load(config_path)?;
    let workspace = config.workspace_path();
    let state_dir = xbot::util::workspace_state_dir(&workspace);
    let skill_dir = state_dir.join("skills").join(name);

    if skill_dir.exists() {
        anyhow::bail!("skill '{name}' already exists at {}", skill_dir.display());
    }

    fs::create_dir_all(&skill_dir)?;

    let skill_md = format!(
        r#"---
description: {name} skill
metadata: {{"xbot": {{"description": "{name} skill", "triggers": ["{name}"]}}}}
---

# {name}

Describe what this skill does and when to use it.

## Usage

Document the workflow, tools, and patterns this skill provides.
"#
    );

    fs::write(skill_dir.join("SKILL.md"), skill_md)?;

    println!(
        "{}",
        Style::new()
            .green()
            .bold()
            .apply_to(format!("Skill '{name}' created at {}", skill_dir.display()))
    );
    println!("Edit {}/SKILL.md to customize.", skill_dir.display());

    Ok(())
}
