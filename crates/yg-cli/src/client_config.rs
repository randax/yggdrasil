//! Client-side config file (`~/.config/yg/config.toml`): where the
//! Index Server lives and the bearer token to present. Parsed with a
//! real TOML parser so a key inside an unrelated section can never be
//! read as the credential (issue #50).

use serde::Deserialize;

/// The two client settings. `YG_SERVER` / `YG_TOKEN` override the file.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    pub server: Option<String>,
    pub token: Option<String>,
}

/// A parse failure that never carries the file's contents: the file
/// holds the bearer token, and toml's rendered errors quote the
/// offending source line — an unquoted token would land in stderr, CI
/// logs, and shell transcripts.
#[derive(Debug, thiserror::Error)]
#[error("invalid TOML at line {line}: {message}")]
pub struct ClientConfigError {
    line: usize,
    message: String,
}

/// Parse the config file's contents. Only the top-level `server` and
/// `token` keys are read; anything else — including keys nested inside
/// sections — is ignored.
pub fn parse_client_config(contents: &str) -> Result<ClientConfig, ClientConfigError> {
    toml::from_str(contents).map_err(|e| ClientConfigError {
        line: e
            .span()
            .map(|span| contents[..span.start].lines().count().max(1))
            .unwrap_or(0),
        message: drop_backtick_quoted(e.message()),
    })
}

/// serde's type errors quote the offending value in backticks ("invalid
/// type: integer `123456789`, expected a string" — that integer could
/// be a numeric token written unquoted), so every backtick-quoted
/// segment is elided.
fn drop_backtick_quoted(message: &str) -> String {
    message
        .split('`')
        .enumerate()
        .map(|(i, part)| if i % 2 == 0 { part } else { "…" })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_server_and_token_are_read() {
        let config = parse_client_config(
            r#"
server = "https://yg.example.test"
token = "ygt_secret"
"#,
        )
        .unwrap();
        assert_eq!(config.server.as_deref(), Some("https://yg.example.test"));
        assert_eq!(config.token.as_deref(), Some("ygt_secret"));
    }

    #[test]
    fn token_inside_unrelated_section_is_not_the_credential() {
        let config = parse_client_config(
            r#"
server = "https://yg.example.test"

[some.other.tool]
token = "not-my-credential"
"#,
        )
        .unwrap();
        assert_eq!(config.token, None);
    }

    #[test]
    fn double_quoted_escapes_decode_per_toml() {
        let config = parse_client_config(r#"token = "line\nnext\t\"quoted\"å""#).unwrap();
        assert_eq!(config.token.as_deref(), Some("line\nnext\t\"quoted\"å"));
    }

    #[test]
    fn literal_and_multi_line_strings_parse_per_toml() {
        let config = parse_client_config(
            "server = 'literal#not-a-comment'\ntoken = \"\"\"\nfirst\nsecond\"\"\"\n",
        )
        .unwrap();
        assert_eq!(config.server.as_deref(), Some("literal#not-a-comment"));
        assert_eq!(config.token.as_deref(), Some("first\nsecond"));
    }

    #[test]
    fn comments_after_values_are_not_part_of_the_value() {
        let config =
            parse_client_config("server = \"https://yg.example.test/mcp#fragment\" # comment\n")
                .unwrap();
        assert_eq!(
            config.server.as_deref(),
            Some("https://yg.example.test/mcp#fragment")
        );
    }

    #[test]
    fn unknown_top_level_keys_are_ignored() {
        let config = parse_client_config("editor = \"vim\"\ntoken = \"ygt_secret\"\n").unwrap();
        assert_eq!(config.token.as_deref(), Some("ygt_secret"));
    }

    #[test]
    fn malformed_toml_is_an_error_not_a_silent_default() {
        assert!(parse_client_config("token = unquoted-bareword\n").is_err());
        assert!(parse_client_config("token = \"unterminated\n").is_err());
    }

    #[test]
    fn parse_errors_never_quote_the_file_contents() {
        // The most likely typo: an unquoted bearer token. The rendered
        // error must locate the problem without echoing the credential.
        for contents in [
            "token = ygt_SUPERSECRET_bare\n",
            "server = \"ok\"\ntoken = \"ygt_SUPERSECRET_bare\" trailing\n",
            "token = \"ygt_SUPERSECRET_bare\nserver = 3\n",
        ] {
            let message = parse_client_config(contents).unwrap_err().to_string();
            assert!(
                !message.contains("ygt_SUPERSECRET_bare"),
                "error echoes the credential: {message}"
            );
            assert!(message.contains("line"), "error should locate the problem");
        }
        // serde type errors backtick-quote primitive values — a numeric
        // PIN or token written unquoted must not survive into stderr.
        for contents in ["token = 90210411\n", "server = 90210411\n"] {
            let message = parse_client_config(contents).unwrap_err().to_string();
            assert!(
                !message.contains("90210411"),
                "error echoes the credential: {message}"
            );
            assert!(message.contains("line"), "error should locate the problem");
        }
    }
}
