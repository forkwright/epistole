//! Deployment-contract tests.
//!
//! The deploy bundle is four artifacts that must agree with one another:
//! `epistole.example.toml`, `deploy/epistole.service`,
//! `deploy/Caddyfile.snippet`, and `DEPLOY.md`. Each can drift on its own,
//! and drift is only visible to an operator halfway through a deploy. These
//! tests pin the agreements the runbook asks an operator to rely on.
//!
//! WHY: the artifacts are embedded with `include_str!` so a rename breaks the
//! build rather than skipping the check at run time.

use epistole::Config;

const EXAMPLE_TOML: &str = include_str!("../epistole.example.toml");
const CADDY_SNIPPET: &str = include_str!("../deploy/Caddyfile.snippet");
const SERVICE_UNIT: &str = include_str!("../deploy/epistole.service");
const RUNBOOK: &str = include_str!("../DEPLOY.md");

/// Secrets that clear the strength floors and miss every blocked pattern.
const TOKEN_SECRET: &str = "Zx7Qv2Lm9Kd4Rt8Wn1Yb6Hf3Jc5Pg0Su";
const SEND_AUTH_TOKEN: &str = "Vb3Nm8Qw1Ei6Rt9Yu2Io5Pa7";
const SMTP_PASSWORD: &str = "Kd9Fh2Lq7Zx4Cv1Bn6Mw3Jt8";

/// The shipped example with its placeholders filled in, as step 5 of the
/// runbook instructs.
fn filled_example() -> String {
    EXAMPLE_TOML
        .replace("REPLACE_WITH_BASE64_RANDOM_32", TOKEN_SECRET)
        .replace("REPLACE_WITH_BASE64_RANDOM_24", SEND_AUTH_TOKEN)
        .replace("REPLACE_WITH_POSTMARK_TOKEN", SMTP_PASSWORD)
}

/// The bind port the bundle is expected to agree on, read from the example
/// config so that file stays the single source. Empty when unparseable,
/// which fails the assertions below rather than the helper.
fn documented_port() -> String {
    EXAMPLE_TOML
        .lines()
        .find(|l| l.trim_start().starts_with("bind ="))
        .and_then(|l| l.rsplit(':').next())
        .map(|p| p.trim_matches(|c: char| !c.is_ascii_digit()).to_owned())
        .unwrap_or_default()
}

#[test]
#[expect(clippy::expect_used, reason = "a failed setup step is a test failure")]
fn example_config_loads_once_its_placeholders_are_filled_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("epistole.toml");
    std::fs::write(&path, filled_example()).expect("write config");

    let cfg = Config::load(&path).expect("filled example config must load");

    assert_eq!(cfg.bind, format!("127.0.0.1:{}", documented_port()));
}

#[test]
#[expect(clippy::expect_used, reason = "a failed setup step is a test failure")]
fn example_config_is_refused_for_its_secrets_not_its_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("epistole.toml");
    std::fs::write(&path, EXAMPLE_TOML).expect("write config");

    let err = Config::load(&path)
        .expect_err("the shipped example must not boot with placeholder secrets")
        .to_string();

    assert!(
        err.contains("token_secret"),
        "expected a secret-strength refusal naming the field, got: {err}"
    );
    assert!(
        !err.contains("unknown field") && !err.contains("parse"),
        "the example must be structurally valid; got a shape error: {err}"
    );
}

#[test]
fn deploy_artifacts_agree_on_the_bind_port() {
    let port = documented_port();
    assert!(!port.is_empty(), "example config must declare a bind port");

    for line in CADDY_SNIPPET
        .lines()
        .filter(|l| l.contains("reverse_proxy"))
    {
        assert!(
            line.contains(&format!("127.0.0.1:{port}")),
            "Caddy snippet proxies to a different port than the example config: {line}"
        );
    }

    let forward = RUNBOOK
        .lines()
        .find(|l| l.contains("Forward Port"))
        .unwrap_or_default();
    assert!(
        forward.contains(&port),
        "runbook forward port disagrees with the example config: {forward}"
    );
}

#[test]
fn caddy_site_label_does_not_append_a_second_tld() {
    assert!(
        !CADDY_SNIPPET.contains("<consumer-domain>."),
        "the placeholder is an apex domain, so appending a TLD yields example.com.com"
    );
}

#[test]
fn systemd_unit_tolerates_an_absent_env_file() {
    assert!(
        SERVICE_UNIT.contains("EnvironmentFile=-"),
        "a required EnvironmentFile makes the unit fail to start when a Phase 0 \
         deploy inlines its secrets instead"
    );
}

#[test]
fn runbook_does_not_promise_a_name_addressed_keyspace_directory() {
    assert!(
        !RUNBOOK.contains("data/subscribers"),
        "fjall stores keyspaces by numeric id, so no subscribers/ directory exists"
    );
}
