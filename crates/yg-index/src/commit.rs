use std::fmt;

/// A full, lowercase git object id that is safe to pass as a revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommitSha(Box<str>);

impl CommitSha {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for CommitSha {
    type Error = InvalidCommitSha;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if is_valid_commit_sha(value) {
            Ok(Self(value.into()))
        } else {
            Err(InvalidCommitSha(value.into()))
        }
    }
}

impl fmt::Display for CommitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct InvalidCommitSha(Box<str>);

impl fmt::Display for InvalidCommitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid commit sha {:?}", self.0)
    }
}

impl std::error::Error for InvalidCommitSha {}

/// Git's full object ids are 40 (SHA-1) or 64 (SHA-256) lowercase hex
/// characters. Keeping this check here gives queued revisions and parsed
/// history records one definition of a valid full object id.
pub(crate) fn is_valid_commit_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_full_lowercase_sha1_and_sha256_object_ids() {
        assert!(CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").is_ok());
        assert!(
            CommitSha::try_from("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .is_ok()
        );
    }

    #[test]
    fn rejects_option_like_short_non_hex_and_uppercase_values() {
        for value in [
            "--exec=x",
            "deadbeef",
            "g123456789abcdef0123456789abcdef01234567",
            "0123456789ABCDEF0123456789ABCDEF01234567",
        ] {
            let error = CommitSha::try_from(value).expect_err("malformed sha must be rejected");
            assert_eq!(error.to_string(), format!("invalid commit sha {value:?}"));
        }
    }
}
