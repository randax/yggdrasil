//! Installation and contract-version checking for the shipped Claude Skill.

use std::path::PathBuf;
use std::str::FromStr;

use yg_verbs::status::{ParseVerbContractVersionError, VerbContractVersion};

const SKILL_NAME: &str = "yggdrasil-navigation";
const SKILL_DOCUMENT: &str = include_str!("../skills/yggdrasil-navigation/SKILL.md");
const CONTRACT_VERSION_PLACEHOLDER: &str = "{{VERB_CONTRACT_VERSION}}";
const CONTRACT_VERSION_PREFIX: &str = "Verb contract version: `";

pub(crate) fn install() -> Result<(), SkillError> {
    let skill_dir = skill_dir()?;
    std::fs::create_dir_all(&skill_dir).map_err(|source| SkillError::CreateDirectory {
        path: skill_dir.clone(),
        source,
    })?;
    let skill_path = skill_dir.join("SKILL.md");
    std::fs::write(&skill_path, render_document()?).map_err(|source| SkillError::Write {
        path: skill_path.clone(),
        source,
    })?;
    println!("installed {SKILL_NAME} Skill at {}", skill_path.display());
    Ok(())
}

/// Compare the installed Skill with a version already returned by `/v1/status`.
/// Absence is fine: users who have not installed the optional Skill should not
/// receive warnings from an otherwise healthy server-status command.
pub(crate) fn warn_if_contract_mismatches(server: VerbContractVersion) {
    let path = match skill_dir() {
        Ok(directory) => directory.join("SKILL.md"),
        Err(SkillError::MissingHome) => return,
        Err(error) => {
            eprintln!("warning: unable to locate the installed {SKILL_NAME} Skill: {error}");
            return;
        }
    };
    let document = match std::fs::read_to_string(&path) {
        Ok(document) => document,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return,
        Err(source) => {
            eprintln!(
                "warning: unable to read the installed {SKILL_NAME} Skill at {}: {source}",
                path.display()
            );
            return;
        }
    };
    match stamped_contract_version(&document) {
        Ok(installed) if installed == server => {}
        Ok(installed) => eprintln!(
            "warning: installed {SKILL_NAME} Skill uses Verb contract {installed}, but the server uses {server}; run `yg skill install` to update it"
        ),
        Err(error) => eprintln!(
            "warning: installed {SKILL_NAME} Skill has no valid Verb contract stamp ({error}); run `yg skill install` to update it"
        ),
    }
}

fn skill_dir() -> Result<PathBuf, SkillError> {
    Ok(skill_home_dir()?
        .join(".claude")
        .join("skills")
        .join(SKILL_NAME))
}

fn skill_home_dir() -> Result<PathBuf, SkillError> {
    std::env::var_os("HOME")
        .filter(|value| !value.as_os_str().is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.as_os_str().is_empty()))
        .map(PathBuf::from)
        .ok_or(SkillError::MissingHome)
}

fn render_document() -> Result<String, SkillError> {
    let (before, after) = SKILL_DOCUMENT
        .split_once(CONTRACT_VERSION_PLACEHOLDER)
        .ok_or(SkillError::MissingPlaceholder)?;
    if after.contains(CONTRACT_VERSION_PLACEHOLDER) {
        return Err(SkillError::DuplicatePlaceholder);
    }
    Ok(format!(
        "{before}{}{after}",
        yg_verbs::status::VERB_CONTRACT_VERSION
    ))
}

fn stamped_contract_version(document: &str) -> Result<VerbContractVersion, SkillError> {
    let value = document
        .lines()
        .find_map(|line| {
            line.strip_prefix(CONTRACT_VERSION_PREFIX)
                .and_then(|value| value.strip_suffix('`'))
        })
        .ok_or(SkillError::MissingStamp)?;
    VerbContractVersion::from_str(value).map_err(SkillError::InvalidStamp)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SkillError {
    #[error("HOME or USERPROFILE must be set to install Claude Code skills")]
    MissingHome,
    #[error("the shipped Skill template is missing its contract-version placeholder")]
    MissingPlaceholder,
    #[error("the shipped Skill template contains more than one contract-version placeholder")]
    DuplicatePlaceholder,
    #[error("the installed Skill is missing its contract-version stamp")]
    MissingStamp,
    #[error("the installed Skill has an invalid contract-version stamp")]
    InvalidStamp(#[source] ParseVerbContractVersionError),
    #[error("creating {path}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("writing {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_skill_has_the_current_typed_contract_stamp() {
        let document = render_document().expect("rendered Skill");
        assert_eq!(
            stamped_contract_version(&document).expect("contract stamp"),
            yg_verbs::status::VERB_CONTRACT_VERSION
        );
        assert!(!document.contains(CONTRACT_VERSION_PLACEHOLDER));
    }

    #[test]
    fn contract_stamp_parser_rejects_missing_and_untyped_values() {
        assert!(matches!(
            stamped_contract_version("# unstamped"),
            Err(SkillError::MissingStamp)
        ));
        assert!(matches!(
            stamped_contract_version("Verb contract version: `latest`"),
            Err(SkillError::InvalidStamp(_))
        ));
    }
}
