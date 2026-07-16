//! Stable process exit classes for CLI automation.

use reqwest::StatusCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitClass {
    General,
    Usage,
    Auth,
    NotFound,
    Server,
}

impl ExitClass {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::General => 1,
            Self::Usage => 2,
            Self::Auth => 3,
            Self::NotFound => 4,
            Self::Server => 5,
        }
    }

    pub(crate) fn for_error(error: &anyhow::Error) -> Self {
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<RequestError>())
            .map_or(Self::General, RequestError::exit_class)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RequestError {
    #[error("{operation}")]
    Transport {
        operation: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("the server answered {path} with {status}: {reason}")]
    Status {
        path: String,
        status: StatusCode,
        reason: String,
    },
    #[error("parsing the typed response from {path}")]
    InvalidResponse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

impl RequestError {
    pub(crate) fn transport(operation: impl Into<String>, source: reqwest::Error) -> Self {
        Self::Transport {
            operation: operation.into(),
            source,
        }
    }

    pub(crate) fn status(
        path: impl Into<String>,
        status: StatusCode,
        reason: impl Into<String>,
    ) -> Self {
        Self::Status {
            path: path.into(),
            status,
            reason: reason.into(),
        }
    }

    pub(crate) fn invalid_response(path: impl Into<String>, source: serde_json::Error) -> Self {
        Self::InvalidResponse {
            path: path.into(),
            source,
        }
    }

    fn exit_class(&self) -> ExitClass {
        match self {
            Self::Status { status, .. }
                if *status == StatusCode::UNAUTHORIZED || *status == StatusCode::FORBIDDEN =>
            {
                ExitClass::Auth
            }
            Self::Status { status, .. } if *status == StatusCode::NOT_FOUND => ExitClass::NotFound,
            Self::Status { status, .. } if status.is_server_error() => ExitClass::Server,
            Self::Transport { .. } | Self::InvalidResponse { .. } => ExitClass::Server,
            Self::Status { .. } => ExitClass::General,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_vocabulary_is_stable() {
        assert_eq!(ExitClass::General.code(), 1);
        assert_eq!(ExitClass::Usage.code(), 2);
        assert_eq!(ExitClass::Auth.code(), 3);
        assert_eq!(ExitClass::NotFound.code(), 4);
        assert_eq!(ExitClass::Server.code(), 5);
    }

    #[test]
    fn typed_http_status_selects_the_scriptable_class() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            assert_eq!(
                RequestError::status("/test", status, "denied").exit_class(),
                ExitClass::Auth
            );
        }
        assert_eq!(
            RequestError::status("/test", StatusCode::NOT_FOUND, "missing").exit_class(),
            ExitClass::NotFound
        );
        assert_eq!(
            RequestError::status("/test", StatusCode::INTERNAL_SERVER_ERROR, "broken").exit_class(),
            ExitClass::Server
        );
        assert_eq!(
            RequestError::status("/test", StatusCode::BAD_REQUEST, "bad").exit_class(),
            ExitClass::General
        );
    }
}
