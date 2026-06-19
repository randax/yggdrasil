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

    let skill = std::fs::read_to_string(
        home.path()
            .join(".claude/skills/yggdrasil-navigation/SKILL.md"),
    )
    .unwrap();
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

    assert!(
        userprofile
            .path()
            .join(".claude/skills/yggdrasil-navigation/SKILL.md")
            .is_file(),
        "empty HOME must be ignored in favor of USERPROFILE"
    );
}
