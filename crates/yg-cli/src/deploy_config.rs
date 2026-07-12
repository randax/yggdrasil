//! One typed view of the deployment's `YG_*` environment (issue #50).
//!
//! Every server and worker setting resolves here, so this module is the
//! single place to see, validate, and document the configuration. The
//! one exception is Forge tokens (`YG_GITHUB_TOKEN` by default): their
//! env var names live in the control plane's Forge records, so the Sync
//! worker reads them per job — resolution here cannot know them without
//! connecting, and config-check never connects.
//! Resolution never touches the network or the database; `yg
//! config-check` reuses it to report the resolved configuration and
//! every validation error without starting the server.

use std::net::SocketAddr;
use std::time::Duration;

use yg_api::ObjectStoreConfig;

use crate::Role;

/// Ceiling on an env-configured duration. A poll/GC cadence is fed to
/// Postgres `make_interval`, which errors (out of range) on absurd
/// values — and that error propagates out of the poll loop and kills
/// the worker. Ten years is far beyond any sane cadence and far inside
/// `make_interval`'s range.
pub const MAX_DURATION_SECS: u64 = 10 * 365 * 24 * 3600;

/// The whole deployment's configuration, typed. Defaults (documented on
/// each resolver call in [`resolve`]) point at the in-repo dev compose
/// stack; only the bootstrap Admin token has no safe default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployConfig {
    pub listen: SocketAddr,
    pub database_url: String,
    /// Required for roles that serve the API (`api`, `all`).
    pub bootstrap_token: Option<String>,
    pub shard_cache: std::path::PathBuf,
    pub git_cache: std::path::PathBuf,
    pub object_store: ObjectStoreConfig,
    pub poll_interval: Duration,
    pub discovery_interval: Duration,
    pub gc_grace: Duration,
    pub gc_interval: Duration,
}

/// Where a resolved setting came from. `Unset` marks a setting with no
/// value at all — a required secret has no default, so calling its
/// absence a "default" would mislead anyone reading the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    Default,
    Unset,
}

/// One resolved setting, ready to print: secrets arrive here already
/// redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub var: &'static str,
    pub shown: String,
    pub source: Source,
}

/// Everything wrong with the environment, reported together so an
/// Admin fixes one deploy, not one variable per boot attempt.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "{var}: {value:?} does not parse as IP:port \
         (hostnames are not resolved), e.g. 127.0.0.1:7311"
    )]
    InvalidListenAddr { var: &'static str, value: String },
    #[error("{var}: {value:?} must be a whole number of seconds, 1..={MAX_DURATION_SECS}")]
    InvalidDurationSecs { var: &'static str, value: String },
    #[error(
        "{var} must be set to a non-empty token; the server refuses to boot without an Admin token"
    )]
    MissingBootstrapToken { var: &'static str },
}

/// The outcome of resolving the environment: the settings report (for
/// `config-check`), every validation error, and — when there are no
/// errors — the typed config.
pub struct Resolution {
    pub settings: Vec<Setting>,
    pub errors: Vec<ConfigError>,
    config: DeployConfig,
}

impl Resolution {
    /// The typed config, or all validation errors joined into one
    /// boot-refusing error.
    pub fn into_config(self) -> anyhow::Result<DeployConfig> {
        if self.errors.is_empty() {
            return Ok(self.config);
        }
        let lines: Vec<String> = self.errors.iter().map(|e| format!("  - {e}")).collect();
        anyhow::bail!("invalid YG_* configuration:\n{}", lines.join("\n"))
    }
}

/// Resolve the deployment configuration for `role` from `lookup`
/// (production passes the process environment; tests inject maps).
/// The defaults below are the whole deployment's default table: they
/// point at the in-repo dev compose stack, and `docs/DEVELOPMENT.md`
/// mirrors them.
pub fn resolve(role: Role, lookup: impl Fn(&str) -> Option<String>) -> Resolution {
    // A role only resolves — validates, reports — the settings its
    // process actually uses: a fleet-wide YG_LISTEN typo must not
    // refuse a worker boot, and an api report must not advertise poll
    // knobs the process ignores. Unresolved settings keep their
    // defaults in the typed config; the role never reads them.
    let api = matches!(role, Role::Api | Role::All);
    let worker = matches!(role, Role::Worker | Role::All);
    let mut r = Resolver {
        lookup: &lookup,
        settings: Vec::new(),
        errors: Vec::new(),
    };
    const DEFAULT_LISTEN: &str = "127.0.0.1:7311";
    let listen = if api {
        r.listen_addr("YG_LISTEN", DEFAULT_LISTEN)
    } else {
        DEFAULT_LISTEN
            .parse()
            .expect("default listen address parses")
    };
    let bootstrap_token = if api {
        let token = r.secret("YG_BOOTSTRAP_TOKEN");
        if token.is_none() {
            r.errors.push(ConfigError::MissingBootstrapToken {
                var: "YG_BOOTSTRAP_TOKEN",
            });
        }
        token
    } else {
        None
    };
    let config = DeployConfig {
        listen,
        bootstrap_token,
        database_url: r.database_url("YG_DATABASE_URL", yg_control::DEFAULT_DATABASE_URL),
        shard_cache: if api {
            r.string("YG_SHARD_CACHE", "./data/shard-cache")
        } else {
            "./data/shard-cache".to_string()
        }
        .into(),
        git_cache: if worker {
            r.string("YG_GIT_CACHE", "./data/git")
        } else {
            "./data/git".to_string()
        }
        .into(),
        object_store: ObjectStoreConfig {
            endpoint: r.string("YG_S3_ENDPOINT", "http://localhost:9000"),
            bucket: r.string("YG_S3_BUCKET", "yggdrasil"),
            access_key: r.redacted_string("YG_S3_ACCESS_KEY", "yggdrasil"),
            secret_key: r.redacted_string("YG_S3_SECRET_KEY", "yggdrasil"),
            region: r.string("YG_S3_REGION", "us-east-1"),
            key_prefix: r.string("YG_S3_PREFIX", ""),
        },
        poll_interval: r.worker_duration(worker, "YG_POLL_INTERVAL", 5 * 60),
        discovery_interval: r.worker_duration(worker, "YG_DISCOVERY_INTERVAL", 60 * 60),
        gc_grace: r.worker_duration(worker, "YG_GC_GRACE", 60 * 60),
        gc_interval: r.worker_duration(worker, "YG_GC_INTERVAL", 10 * 60),
    };
    Resolution {
        settings: r.settings,
        errors: r.errors,
        config,
    }
}

/// Accumulates the settings report and every validation error while the
/// typed config is built; a setting that fails to parse records its
/// error and falls back to the default so resolution keeps going and
/// reports everything wrong at once.
struct Resolver<'a> {
    lookup: &'a dyn Fn(&str) -> Option<String>,
    settings: Vec<Setting>,
    errors: Vec<ConfigError>,
}

impl Resolver<'_> {
    fn raw(&mut self, var: &'static str) -> Option<String> {
        (self.lookup)(var)
    }

    fn record(&mut self, var: &'static str, shown: String, source: Source) {
        self.settings.push(Setting { var, shown, source });
    }

    fn string(&mut self, var: &'static str, default: &str) -> String {
        fn shown(value: &str) -> String {
            if value.is_empty() {
                "(empty)".to_string()
            } else {
                value.to_string()
            }
        }
        match self.raw(var) {
            Some(value) => {
                self.record(var, shown(&value), Source::Env);
                value
            }
            None => {
                self.record(var, shown(default), Source::Default);
                default.to_string()
            }
        }
    }

    /// Like [`Self::string`] but never shows the value: credentials
    /// appear in the report as set-or-default only.
    fn redacted_string(&mut self, var: &'static str, default: &str) -> String {
        match self.raw(var) {
            Some(value) => {
                self.record(var, REDACTED.into(), Source::Env);
                value
            }
            None => {
                self.record(var, REDACTED.into(), Source::Default);
                default.to_string()
            }
        }
    }

    /// A required-by-some-roles secret with no default — recorded as
    /// [`Source::Unset`] when absent. `None` when unset or blank
    /// (whitespace-padded tokens are trimmed — env files commonly leak
    /// whitespace, and HTTP strips it from header values, so a padded
    /// token could never be presented by any client).
    fn secret(&mut self, var: &'static str) -> Option<String> {
        let value = self
            .raw(var)
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty());
        match value {
            Some(value) => {
                self.record(var, REDACTED.into(), Source::Env);
                Some(value)
            }
            None => {
                self.record(var, "—".into(), Source::Unset);
                None
            }
        }
    }

    /// Like [`Self::string`] but shown with any URL password masked: a
    /// database URL routinely embeds a credential, and the settings
    /// report must never print one.
    fn database_url(&mut self, var: &'static str, default: &str) -> String {
        match self.raw(var) {
            Some(value) => {
                self.record(var, redact_url_password(&value), Source::Env);
                value
            }
            None => {
                self.record(var, redact_url_password(default), Source::Default);
                default.to_string()
            }
        }
    }

    fn listen_addr(&mut self, var: &'static str, default: &str) -> SocketAddr {
        let fallback: SocketAddr = default.parse().expect("default listen address parses");
        let Some(value) = self.raw(var) else {
            self.record(var, default.into(), Source::Default);
            return fallback;
        };
        match value.parse() {
            Ok(addr) => {
                self.record(var, value, Source::Env);
                addr
            }
            Err(_) => {
                self.record(var, value.clone(), Source::Env);
                self.errors
                    .push(ConfigError::InvalidListenAddr { var, value });
                fallback
            }
        }
    }

    /// A worker-cadence duration: resolved only for worker-running
    /// roles, otherwise the default passes through untouched (the
    /// process never reads it).
    fn worker_duration(&mut self, worker: bool, var: &'static str, default_secs: u64) -> Duration {
        if worker {
            self.duration_secs(var, default_secs)
        } else {
            Duration::from_secs(default_secs)
        }
    }

    /// A whole number of seconds in `1..=MAX_DURATION_SECS`; anything
    /// else is a validation error (it used to degrade to the default
    /// with a warning, which config-check would render invisible).
    fn duration_secs(&mut self, var: &'static str, default_secs: u64) -> Duration {
        let Some(value) = self.raw(var) else {
            self.record(var, format!("{default_secs}s"), Source::Default);
            return Duration::from_secs(default_secs);
        };
        match value.trim().parse::<u64>() {
            Ok(secs) if (1..=MAX_DURATION_SECS).contains(&secs) => {
                self.record(var, format!("{secs}s"), Source::Env);
                Duration::from_secs(secs)
            }
            _ => {
                self.record(var, value.clone(), Source::Env);
                self.errors
                    .push(ConfigError::InvalidDurationSecs { var, value });
                Duration::from_secs(default_secs)
            }
        }
    }
}

/// What the settings report shows in place of any credential.
pub const REDACTED: &str = "(redacted)";

/// Mask the credential-bearing parts of a URL, leaving the user, host,
/// and path visible. Clients treat the last `@` in the authority as
/// the userinfo delimiter, so redaction does too. The query string is
/// masked wholesale: libpq-style URLs accept `?password=...`, so
/// nothing after `?` can be vetted. A value that has an `@` but cannot
/// be confidently parsed (no scheme, or the `@` lands outside the
/// authority because of an unencoded `/` in the password) is replaced
/// wholesale — a report must never gamble with a credential.
fn redact_url_password(url: &str) -> String {
    let (url, query_mask) = match url.split_once('?') {
        Some((base, _)) => (base, format!("?{REDACTED}")),
        None => (url, String::new()),
    };
    let Some(scheme_end) = url.find("://") else {
        return if url.contains('@') {
            REDACTED.to_string()
        } else {
            format!("{url}{query_mask}")
        };
    };
    let rest = &url[scheme_end + 3..];
    let authority = &rest[..rest.find('/').unwrap_or(rest.len())];
    let Some(at) = authority.rfind('@') else {
        return if rest.contains('@') {
            REDACTED.to_string()
        } else {
            format!("{url}{query_mask}")
        };
    };
    let userinfo = &authority[..at];
    let Some(colon) = userinfo.find(':') else {
        return format!("{url}{query_mask}");
    };
    format!(
        "{}{}{}{}",
        &url[..scheme_end + 3 + colon + 1],
        REDACTED,
        &rest[at..],
        query_mask
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |var| {
            pairs
                .iter()
                .find(|(name, _)| *name == var)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn defaults_point_at_the_dev_compose_stack() {
        let config = resolve(Role::All, env(&[("YG_BOOTSTRAP_TOKEN", "ygt_admin")]))
            .into_config()
            .unwrap();
        assert_eq!(config.listen, "127.0.0.1:7311".parse().unwrap());
        assert_eq!(config.database_url, yg_control::DEFAULT_DATABASE_URL);
        assert_eq!(config.bootstrap_token.as_deref(), Some("ygt_admin"));
        assert_eq!(
            config.shard_cache,
            std::path::Path::new("./data/shard-cache")
        );
        assert_eq!(config.git_cache, std::path::Path::new("./data/git"));
        assert_eq!(config.object_store.endpoint, "http://localhost:9000");
        assert_eq!(config.object_store.bucket, "yggdrasil");
        assert_eq!(config.object_store.access_key, "yggdrasil");
        assert_eq!(config.object_store.secret_key, "yggdrasil");
        assert_eq!(config.object_store.region, "us-east-1");
        assert_eq!(config.object_store.key_prefix, "");
        assert_eq!(config.poll_interval, Duration::from_secs(5 * 60));
        assert_eq!(config.discovery_interval, Duration::from_secs(60 * 60));
        assert_eq!(config.gc_grace, Duration::from_secs(60 * 60));
        assert_eq!(config.gc_interval, Duration::from_secs(10 * 60));
    }

    #[test]
    fn every_setting_resolves_from_its_env_var() {
        let config = resolve(
            Role::All,
            env(&[
                ("YG_LISTEN", "0.0.0.0:8000"),
                ("YG_BOOTSTRAP_TOKEN", " ygt_admin \n"),
                ("YG_DATABASE_URL", "postgres://db.example.test/yg"),
                ("YG_SHARD_CACHE", "/var/cache/yg-shards"),
                ("YG_GIT_CACHE", "/var/cache/yg-git"),
                ("YG_S3_ENDPOINT", "https://s3.example.test"),
                ("YG_S3_BUCKET", "prod-shards"),
                ("YG_S3_ACCESS_KEY", "AKIAEXAMPLE"),
                ("YG_S3_SECRET_KEY", "s3cret"),
                ("YG_S3_REGION", "eu-north-1"),
                ("YG_S3_PREFIX", "prod/"),
                ("YG_POLL_INTERVAL", "30"),
                ("YG_DISCOVERY_INTERVAL", "120"),
                ("YG_GC_GRACE", "10"),
                ("YG_GC_INTERVAL", "5"),
            ]),
        )
        .into_config()
        .unwrap();
        assert_eq!(config.listen, "0.0.0.0:8000".parse().unwrap());
        // Trimmed: env files commonly leak whitespace around tokens.
        assert_eq!(config.bootstrap_token.as_deref(), Some("ygt_admin"));
        assert_eq!(config.database_url, "postgres://db.example.test/yg");
        assert_eq!(
            config.shard_cache,
            std::path::Path::new("/var/cache/yg-shards")
        );
        assert_eq!(config.git_cache, std::path::Path::new("/var/cache/yg-git"));
        assert_eq!(config.object_store.endpoint, "https://s3.example.test");
        assert_eq!(config.object_store.bucket, "prod-shards");
        assert_eq!(config.object_store.access_key, "AKIAEXAMPLE");
        assert_eq!(config.object_store.secret_key, "s3cret");
        assert_eq!(config.object_store.region, "eu-north-1");
        assert_eq!(config.object_store.key_prefix, "prod/");
        assert_eq!(config.poll_interval, Duration::from_secs(30));
        assert_eq!(config.discovery_interval, Duration::from_secs(120));
        assert_eq!(config.gc_grace, Duration::from_secs(10));
        assert_eq!(config.gc_interval, Duration::from_secs(5));
    }

    #[test]
    fn bootstrap_token_is_required_for_api_serving_roles_only() {
        for role in [Role::Api, Role::All] {
            let resolution = resolve(role, env(&[]));
            assert!(
                resolution
                    .errors
                    .iter()
                    .any(|e| matches!(e, ConfigError::MissingBootstrapToken { .. })),
                "missing token must be an error for API-serving roles"
            );
        }
        let config = resolve(Role::Worker, env(&[])).into_config().unwrap();
        assert_eq!(config.bootstrap_token, None);

        // The token has no default, so its absence is reported as
        // unset, not as a default.
        let resolution = resolve(Role::Api, env(&[]));
        let token = resolution
            .settings
            .iter()
            .find(|s| s.var == "YG_BOOTSTRAP_TOKEN")
            .unwrap();
        assert_eq!(token.source, Source::Unset);
    }

    #[test]
    fn a_blank_bootstrap_token_counts_as_missing() {
        let resolution = resolve(Role::All, env(&[("YG_BOOTSTRAP_TOKEN", "  \n")]));
        assert!(
            resolution
                .errors
                .iter()
                .any(|e| matches!(e, ConfigError::MissingBootstrapToken { .. }))
        );
    }

    #[test]
    fn all_validation_errors_are_reported_together() {
        let resolution = resolve(
            Role::All,
            env(&[
                ("YG_LISTEN", "not-an-address"),
                ("YG_POLL_INTERVAL", "soon"),
                ("YG_GC_GRACE", "0"),
                ("YG_GC_INTERVAL", "99999999999999999999"),
            ]),
        );
        let errored: Vec<&'static str> = resolution
            .errors
            .iter()
            .map(|e| match e {
                ConfigError::InvalidListenAddr { var, .. }
                | ConfigError::InvalidDurationSecs { var, .. }
                | ConfigError::MissingBootstrapToken { var } => *var,
            })
            .collect();
        assert_eq!(
            errored,
            [
                "YG_LISTEN",
                "YG_BOOTSTRAP_TOKEN",
                "YG_POLL_INTERVAL",
                "YG_GC_GRACE",
                "YG_GC_INTERVAL"
            ]
        );
        let message = format!("{:#}", resolution.into_config().unwrap_err());
        assert!(message.contains("YG_LISTEN"), "{message}");
        assert!(message.contains("YG_POLL_INTERVAL"), "{message}");
    }

    #[test]
    fn each_role_resolves_only_the_settings_it_uses() {
        // A worker never binds a listen address or checks tokens
        // (docs/DEVELOPMENT.md says so), so an api-only variable that is
        // invalid fleet-wide must not refuse a worker boot — and must
        // not clutter its report.
        let worker = resolve(
            Role::Worker,
            env(&[("YG_LISTEN", "not-an-address"), ("YG_SHARD_CACHE", "/x")]),
        );
        assert!(worker.errors.is_empty(), "{:?}", worker.errors);
        let worker_vars: Vec<&str> = worker.settings.iter().map(|s| s.var).collect();
        assert!(!worker_vars.contains(&"YG_LISTEN"));
        assert!(!worker_vars.contains(&"YG_BOOTSTRAP_TOKEN"));
        assert!(!worker_vars.contains(&"YG_SHARD_CACHE"));
        assert!(worker_vars.contains(&"YG_GIT_CACHE"));
        assert!(worker_vars.contains(&"YG_DATABASE_URL"));

        // And the api role ignores worker-only settings the same way.
        let api = resolve(
            Role::Api,
            env(&[
                ("YG_BOOTSTRAP_TOKEN", "ygt_admin"),
                ("YG_POLL_INTERVAL", "soon"),
            ]),
        );
        assert!(api.errors.is_empty(), "{:?}", api.errors);
        let api_vars: Vec<&str> = api.settings.iter().map(|s| s.var).collect();
        assert!(!api_vars.contains(&"YG_POLL_INTERVAL"));
        assert!(!api_vars.contains(&"YG_GIT_CACHE"));
        assert!(api_vars.contains(&"YG_LISTEN"));
        assert!(api_vars.contains(&"YG_SHARD_CACHE"));
    }

    #[test]
    fn credentials_never_appear_in_the_settings_report() {
        let resolution = resolve(
            Role::All,
            env(&[
                ("YG_BOOTSTRAP_TOKEN", "ygt_admin_secret"),
                ("YG_S3_ACCESS_KEY", "s3_access_value"),
                ("YG_S3_SECRET_KEY", "s3_secret_value"),
            ]),
        );
        for setting in &resolution.settings {
            assert!(
                !setting.shown.contains("ygt_admin_secret")
                    && !setting.shown.contains("s3_access_value")
                    && !setting.shown.contains("s3_secret_value"),
                "{}: {} leaks a credential",
                setting.var,
                setting.shown
            );
        }
        let shown = |var: &str| {
            resolution
                .settings
                .iter()
                .find(|s| s.var == var)
                .map(|s| s.shown.clone())
                .unwrap()
        };
        assert_eq!(shown("YG_BOOTSTRAP_TOKEN"), REDACTED);
        assert_eq!(shown("YG_S3_ACCESS_KEY"), REDACTED);
        assert_eq!(shown("YG_S3_SECRET_KEY"), REDACTED);
    }

    /// Assemble credential-shaped fixtures at runtime so no source line
    /// carries something a secret scanner mistakes for a real
    /// credential.
    fn fake_db_url(user: &str, password: &str, tail: &str) -> String {
        format!("postgres://{user}:{password}@{tail}")
    }

    #[test]
    fn the_database_url_password_is_redacted_in_the_settings_report() {
        let url = fake_db_url("yg_user", "db_password", "db.example.test:5432/yg");
        let resolution = resolve(Role::Worker, env(&[("YG_DATABASE_URL", url.as_str())]));
        let shown = &resolution
            .settings
            .iter()
            .find(|s| s.var == "YG_DATABASE_URL")
            .unwrap()
            .shown;
        assert!(!shown.contains("db_password"), "{shown} leaks the password");
        assert!(
            shown.contains("yg_user") && shown.contains("db.example.test:5432/yg"),
            "{shown} should still identify the database"
        );
        // The config itself keeps the working URL.
        let config = resolution.into_config().unwrap();
        assert_eq!(config.database_url, url);
    }

    #[test]
    fn ambiguous_database_urls_are_fully_redacted_never_leaked() {
        // Unencoded '@' in the password: clients treat the LAST '@' in
        // the authority as the userinfo delimiter, so everything up to
        // it is credential.
        assert_eq!(
            super::redact_url_password(&fake_db_url("user", "p@ss", "host/yg")),
            format!("postgres://user:{REDACTED}@host/yg")
        );
        // Unencoded '/' in the password puts the '@' outside what looks
        // like the authority; the value is ambiguous, so nothing of it
        // may be shown.
        assert_eq!(
            super::redact_url_password(&fake_db_url("user", "pa/ss", "host/yg")),
            REDACTED
        );
        // Scheme-less but credential-shaped: same rule.
        assert_eq!(super::redact_url_password("user:pw@host/db"), REDACTED);
        // libpq-style URLs take the password as a query parameter, so
        // the query string is masked wholesale.
        assert_eq!(
            super::redact_url_password(&format!(
                "postgres://user@host/yg?{}=qp_secret",
                "password"
            )),
            format!("postgres://user@host/yg?{REDACTED}")
        );
        assert_eq!(
            super::redact_url_password(&format!(
                "{}?{}=qp_secret&sslmode=x",
                fake_db_url("u", "pw", "host/yg"),
                "password"
            )),
            format!("postgres://u:{REDACTED}@host/yg?{REDACTED}")
        );
        // IPv6 host with a port parses fine and has no credential.
        assert_eq!(
            super::redact_url_password("postgres://[::1]:5432/yg"),
            "postgres://[::1]:5432/yg"
        );
    }

    #[test]
    fn a_database_url_without_a_password_is_shown_as_is() {
        let resolution = resolve(
            Role::Worker,
            env(&[("YG_DATABASE_URL", "postgres://db.example.test/yg")]),
        );
        let shown = &resolution
            .settings
            .iter()
            .find(|s| s.var == "YG_DATABASE_URL")
            .unwrap()
            .shown;
        assert!(shown.contains("postgres://db.example.test/yg"), "{shown}");
    }

    #[test]
    fn the_settings_report_names_every_variable_with_its_source() {
        let resolution = resolve(Role::All, env(&[("YG_LISTEN", "0.0.0.0:80")]));
        let vars: Vec<&str> = resolution.settings.iter().map(|s| s.var).collect();
        assert_eq!(
            vars,
            [
                "YG_LISTEN",
                "YG_BOOTSTRAP_TOKEN",
                "YG_DATABASE_URL",
                "YG_SHARD_CACHE",
                "YG_GIT_CACHE",
                "YG_S3_ENDPOINT",
                "YG_S3_BUCKET",
                "YG_S3_ACCESS_KEY",
                "YG_S3_SECRET_KEY",
                "YG_S3_REGION",
                "YG_S3_PREFIX",
                "YG_POLL_INTERVAL",
                "YG_DISCOVERY_INTERVAL",
                "YG_GC_GRACE",
                "YG_GC_INTERVAL"
            ]
        );
        let listen = &resolution.settings[0];
        assert_eq!(listen.source, Source::Env);
        assert!(
            resolution.settings[2..]
                .iter()
                .all(|s| s.source == Source::Default)
        );
    }
}
