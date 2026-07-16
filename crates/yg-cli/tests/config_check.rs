//! `yg config-check` reports the resolved deployment configuration and
//! its validation errors without starting the server (issue #50).
//! Unlike the other e2e targets this one needs no compose stack — the
//! whole point of the command is that it never connects to anything,
//! which the tests prove by pointing every endpoint at unreachable
//! hosts and still expecting an immediate answer.

use predicates::boolean::PredicateBooleanExt;
use predicates::str::contains;

/// A `yg` invocation whose environment carries no ambient YG_* noise
/// from the developer's shell.
fn yg() -> assert_cmd::Command {
    let mut cmd = assert_cmd::Command::cargo_bin("yg").unwrap();
    cmd.env_clear();
    cmd
}

#[test]
fn config_check_reports_resolved_config_without_starting_the_server() {
    yg()
        // Unreachable database and object store: config-check succeeding
        // anyway is what proves it never connects.
        .env("YG_BOOTSTRAP_TOKEN", "ygt_admin_credential")
        .env("YG_DATABASE_URL", "postgres://db.invalid:5432/yg")
        .env("YG_S3_ENDPOINT", "http://s3.invalid:9000")
        .env("YG_SHARD_CACHE_MAX_BYTES", "1048576")
        .env("YG_POLL_INTERVAL", "30")
        .arg("config-check")
        .assert()
        .success()
        .stdout(
            contains("YG_DATABASE_URL")
                .and(contains("postgres://db.invalid:5432/yg"))
                .and(contains("YG_POLL_INTERVAL"))
                .and(contains("30s"))
                .and(contains("YG_SHARD_CACHE_MAX_BYTES"))
                .and(contains("1048576"))
                .and(contains("(env)"))
                // Untouched settings resolve to documented defaults.
                .and(contains("YG_GC_INTERVAL"))
                .and(contains("600s"))
                .and(contains("(default)"))
                .and(contains("configuration valid")),
        );
}

#[test]
fn config_check_never_prints_credentials() {
    yg().env("YG_BOOTSTRAP_TOKEN", "ygt_admin_credential")
        .env("YG_S3_ACCESS_KEY", "s3_access_credential")
        .env("YG_S3_SECRET_KEY", "s3_secret_credential")
        // Assembled at runtime so no source line carries something a
        // secret scanner mistakes for a real credential.
        .env(
            "YG_DATABASE_URL",
            format!(
                "postgres://yg:{}@db.invalid:5432/yg",
                "db_password_credential"
            ),
        )
        .arg("config-check")
        .assert()
        .success()
        .stdout(
            contains("ygt_admin_credential")
                .not()
                .and(contains("s3_access_credential").not())
                .and(contains("s3_secret_credential").not())
                .and(contains("db_password_credential").not())
                .and(contains("YG_BOOTSTRAP_TOKEN"))
                .and(contains("YG_S3_SECRET_KEY"))
                .and(contains("db.invalid:5432/yg")),
        );
}

#[test]
fn config_check_reports_every_validation_error_and_exits_nonzero() {
    yg()
        // No bootstrap token, a bad listen address, and a bad duration:
        // all three must be reported in one run.
        .env("YG_LISTEN", "not-an-address")
        .env("YG_GC_GRACE", "soon")
        .env("YG_SHARD_CACHE_MAX_BYTES", "0")
        .arg("config-check")
        .assert()
        .failure()
        .stderr(
            contains("YG_BOOTSTRAP_TOKEN")
                .and(contains("YG_LISTEN"))
                .and(contains("not-an-address"))
                .and(contains("YG_GC_GRACE"))
                .and(contains("soon"))
                .and(contains("YG_SHARD_CACHE_MAX_BYTES"))
                .and(contains("positive whole number of bytes")),
        );
}

#[test]
fn config_check_for_the_worker_role_needs_no_bootstrap_token() {
    yg().arg("config-check")
        .arg("--role")
        .arg("worker")
        .assert()
        .success()
        .stdout(contains("configuration valid"));
}
