//! SkillsLoader requirement filtering, always-on skills, and trigger-based suggestions.

use std::fs;
use tempfile::tempdir;
use xbot::engine::SkillsLoader;
use xbot::util::workspace_state_dir;

fn write_skill(workspace: &std::path::Path, name: &str, body: &str) {
    let dir = workspace_state_dir(workspace).join("skills").join(name);
    fs::create_dir_all(&dir).expect("mkdir skill");
    fs::write(dir.join("SKILL.md"), body).expect("write SKILL.md");
}

#[test]
fn filters_skills_with_missing_binary_requirement() {
    let dir = tempdir().unwrap();
    let empty_builtin = dir.path().join("empty_builtin");
    fs::create_dir_all(&empty_builtin).unwrap();

    write_skill(
        dir.path(),
        "needs-missing-bin",
        r#"---
name: needs-missing-bin
description: requires a binary that does not exist
metadata: {"xbot":{"requires":"{\"bins\":[\"__no_such_binary_xbot_test__\"]}"}}
---

Body
"#,
    );

    let loader = SkillsLoader::new(dir.path(), Some(empty_builtin.clone()));
    let all = loader.list_skills(false);
    assert!(
        all.iter().any(|s| s.name == "needs-missing-bin"),
        "skill should exist when not filtering"
    );

    let filtered = loader.list_skills(true);
    assert!(
        !filtered.iter().any(|s| s.name == "needs-missing-bin"),
        "unavailable binary should exclude skill from filtered list: {filtered:?}"
    );
}

#[test]
fn filters_skills_with_missing_env_requirement() {
    let dir = tempdir().unwrap();
    let empty_builtin = dir.path().join("empty_builtin");
    fs::create_dir_all(&empty_builtin).unwrap();

    write_skill(
        dir.path(),
        "needs-env",
        r#"---
name: needs-env
description: needs env var
metadata: {"xbot":{"requires":"{\"env\":[\"__XBOT_NO_SUCH_ENV_VAR__\"]}"}}
---

Body
"#,
    );

    let loader = SkillsLoader::new(dir.path(), Some(empty_builtin));
    assert!(loader.list_skills(true).is_empty());
}

#[test]
fn filters_skills_on_wrong_os_requirement() {
    let dir = tempdir().unwrap();
    let empty_builtin = dir.path().join("empty_builtin");
    fs::create_dir_all(&empty_builtin).unwrap();

    write_skill(
        dir.path(),
        "linux-only",
        r#"---
name: linux-only
description: linux only
metadata: {"xbot":{"os":"[\"linux\"]"}}
---

Body
"#,
    );

    let loader = SkillsLoader::new(dir.path(), Some(empty_builtin));
    let present = loader
        .list_skills(true)
        .iter()
        .any(|s| s.name == "linux-only");
    if std::env::consts::OS == "linux" {
        assert!(present, "linux-only skill should be available on linux");
    } else {
        assert!(!present, "linux-only skill should be filtered on non-linux");
    }
}

#[test]
fn available_skill_passes_checks() {
    let dir = tempdir().unwrap();
    let empty_builtin = dir.path().join("empty_builtin");
    fs::create_dir_all(&empty_builtin).unwrap();

    write_skill(
        dir.path(),
        "ok-skill",
        r#"---
name: ok-skill
description: uses /bin/sh which exists on unix
metadata: {"xbot":{"requires":"{\"bins\":[\"sh\"]}"}}
---

Body
"#,
    );

    let loader = SkillsLoader::new(dir.path(), Some(empty_builtin));
    let names: Vec<_> = loader
        .list_skills(true)
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(
        names.contains(&"ok-skill".to_string()),
        "expected ok-skill to pass requirements, got {names:?}"
    );
}

#[test]
fn always_on_skills_are_listed() {
    let dir = tempdir().unwrap();
    let empty_builtin = dir.path().join("empty_builtin");
    fs::create_dir_all(&empty_builtin).unwrap();

    write_skill(
        dir.path(),
        "always-skill",
        r#"---
name: always-skill
description: always inject
metadata: {"xbot":{"always":"true","requires":"{\"bins\":[\"sh\"]}"}}
---

Body
"#,
    );

    let loader = SkillsLoader::new(dir.path(), Some(empty_builtin));
    let always = loader.get_always_skills();
    assert!(
        always.contains(&"always-skill".to_string()),
        "expected always skill in get_always_skills(), got {always:?}"
    );
}

#[test]
fn suggest_skills_matches_trigger_words() {
    let dir = tempdir().unwrap();
    let empty_builtin = dir.path().join("empty_builtin");
    fs::create_dir_all(&empty_builtin).unwrap();

    write_skill(
        dir.path(),
        "triggered",
        r#"---
name: triggered
description: triggered by phrase
metadata: {"xbot":{"triggers":"[\"uniquepineappletoken\"]","requires":"{\"bins\":[\"sh\"]}"}}
---

Body
"#,
    );

    let loader = SkillsLoader::new(dir.path(), Some(empty_builtin));
    let hits = loader.suggest_skills("Please use uniquepineappletoken in your answer", 5);
    assert!(
        hits.contains(&"triggered".to_string()),
        "expected trigger match, got {hits:?}"
    );

    let miss = loader.suggest_skills("no matching words here", 5);
    assert!(
        !miss.contains(&"triggered".to_string()),
        "unexpected suggestion: {miss:?}"
    );
}
