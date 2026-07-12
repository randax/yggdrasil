#[test]
fn skill_install_places_navigation_skill_for_claude_code() {
    let home = tempfile::tempdir().unwrap();

    assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("HOME", home.path())
        .arg("skill")
        .arg("install")
        .assert()
        .success();

    let skill = installed_skill(home.path());
    assert_eq!(
        skill,
        include_str!("../skills/yggdrasil-navigation/SKILL.md")
    );
    assert!(skill.contains("name: yggdrasil-navigation"));
    assert!(skill.contains("Server/Verb version"));
    assert!(skill.contains("RFC 0001 §7"));
    assert!(skill.contains("Knowledge Graph vs reading files"));
    assert!(skill.contains("Division of truth"));
    assert!(skill.contains("Search-first orientation"));
    assert!(skill.contains("map Verb arrives in M1"));
    assert!(skill.contains("Provenance trust rules"));
    assert!(skill.contains("Verb cookbook"));
    assert!(skill.contains("Failure etiquette"));
}

#[test]
fn skill_install_twice_produces_byte_identical_files() {
    let home = tempfile::tempdir().unwrap();
    let install = || {
        assert_cmd::Command::cargo_bin("yg")
            .unwrap()
            .env("HOME", home.path())
            .arg("skill")
            .arg("install")
            .assert()
            .success();
        installed_skill(home.path())
    };

    // The Skill sits as standing early-position context in the client's
    // prompt; a byte that drifts between installs of the same contract
    // version invalidates every user's prompt-cache prefix.
    assert_eq!(install(), install());
}

#[test]
fn skill_install_falls_back_to_userprofile_when_home_is_empty() {
    let userprofile = tempfile::tempdir().unwrap();

    assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("HOME", "")
        .env("USERPROFILE", userprofile.path())
        .arg("skill")
        .arg("install")
        .assert()
        .success();

    assert_eq!(
        installed_skill(userprofile.path()),
        include_str!("../skills/yggdrasil-navigation/SKILL.md"),
        "empty HOME must be ignored in favor of USERPROFILE"
    );
}

#[test]
fn skill_install_falls_back_to_userprofile_when_home_is_missing() {
    let userprofile = tempfile::tempdir().unwrap();

    assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env_remove("HOME")
        .env("USERPROFILE", userprofile.path())
        .arg("skill")
        .arg("install")
        .assert()
        .success();

    assert_eq!(
        installed_skill(userprofile.path()),
        include_str!("../skills/yggdrasil-navigation/SKILL.md"),
        "missing HOME must fall back to USERPROFILE"
    );
}

#[test]
fn skill_install_requires_a_home_directory() {
    assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .arg("skill")
        .arg("install")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "HOME or USERPROFILE must be set to install Claude Code skills",
        ));
}

fn installed_skill(home: &std::path::Path) -> String {
    std::fs::read_to_string(home.join(".claude/skills/yggdrasil-navigation/SKILL.md")).unwrap()
}
