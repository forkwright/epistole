//! Tests for the `${VAR}` env-resolution + secret-strength validation
//! paths in `Config::load`.

use std::net::{IpAddr, Ipv4Addr};

use secrecy::{ExposeSecret, SecretString};

use super::{
    Config, TrustedProxyRange, is_valid_env_ref, resolve_secret_env, validate_bind_policy,
    validate_secret_strength,
};

/// A config TOML that loads cleanly — every secret clears its strength
/// floor and misses every blocked pattern, every send cap is sane. Tests
/// below mutate one line at a time to exercise a single failure mode.
fn valid_config_toml() -> String {
    r#"
bind = "127.0.0.1:9090"
data_dir = "/tmp/epistole-config-test-does-not-need-to-exist"
base_url = "https://letters.example.com"
token_secret = "Zx7Qv2Lm9Kd4Rt8Wn1Yb6Hf3Jc5Pg0Su"
send_auth_token = "Vb3Nm8Qw1Ei6Rt9Yu2Io5Pa7"
webhook_auth_token = "Ce4Ht9Ok2Sw5Xz8Bd1Fg6Ju3"
send_cap_per_hour = 500
send_cap_per_day = 2000

[brand]
name = "Test Brand"
from_address = "letters@example.com"

[smtp]
host = "smtp.postmarkapp.com"
port = 587
username = "Rn8Vt3Wc6Ym1Ap4Ez7"
password = "Kd9Fh2Lq7Zx4Cv1Bn6Mw3Jt8"
"#
    .to_owned()
}

/// Write `toml` to a tempfile and load it, so a test can assert on the
/// full `Config::load` pipeline (env resolution, strength checks, and
/// the cap sanity checks) rather than only the free helper functions.
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn load(toml: &str) -> super::Result<Config> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("epistole.toml");
    std::fs::write(&path, toml).expect("write config");
    Config::load(&path)
}

#[test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn passthrough_value_is_unchanged() {
    let v = SecretString::from("literal-value".to_owned());
    let out = resolve_secret_env(&v, "test").expect("ok");
    assert_eq!(out.expose_secret(), "literal-value");
}

#[test]
#[expect(
    unsafe_code,
    reason = "set_var is unsafe in 2024 edition; test isolates the var"
)]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn env_substitution_resolves() {
    // SAFETY: test is single-threaded; we set a unique var name.
    unsafe {
        std::env::set_var("EPISTOLE_TEST_TOKEN_X1", "the-real-secret");
    }
    let v = SecretString::from("${EPISTOLE_TEST_TOKEN_X1}".to_owned());
    let out = resolve_secret_env(&v, "test").expect("ok");
    assert_eq!(out.expose_secret(), "the-real-secret");
    unsafe {
        std::env::remove_var("EPISTOLE_TEST_TOKEN_X1");
    }
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn unset_env_var_errors() {
    let v = SecretString::from("${EPISTOLE_TEST_DEFINITELY_UNSET}".to_owned());
    let err = resolve_secret_env(&v, "test").unwrap_err();
    assert!(
        format!("{err}").contains("EPISTOLE_TEST_DEFINITELY_UNSET"),
        "error mentions the missing var"
    );
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn validate_strength_rejects_too_short() {
    let v = SecretString::from("short".to_owned());
    let err = validate_secret_strength(&v, "test", 32).unwrap_err();
    assert!(format!("{err}").contains("too short"));
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn validate_strength_rejects_phase_0_stub_pattern() {
    let v = SecretString::from("phase-0-stub-replace-with-postmark-token-padding".to_owned());
    let err = validate_secret_strength(&v, "test", 32).unwrap_err();
    assert!(format!("{err}").contains("phase-0-stub"));
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn validate_strength_rejects_replace_with_pattern() {
    let v = SecretString::from("REPLACE_WITH_BASE64_RANDOM_32_padding-padding-padding".to_owned());
    let err = validate_secret_strength(&v, "test", 32).unwrap_err();
    assert!(format!("{err}").contains("REPLACE_WITH") || format!("{err}").contains("replace_with"));
}

#[test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn validate_strength_accepts_strong_secret() {
    // 32 bytes of /dev/urandom-ish content, no banned substrings.
    let v = SecretString::from("Yk9mNTBjZWE3OTIzNzg5YzkzMjg0NWE2YWRkOWM4MTM".to_owned());
    validate_secret_strength(&v, "test", 32).expect("strong");
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn validate_strength_rejects_runbook_template_literal() {
    // Reaudit #29: the literal `<SEND_AUTH_TOKEN from step 4>` from
    // DEPLOY.md passes the 24-byte length floor; without the new
    // 'from step' blocklist entry it would boot with the documented
    // public string as the bearer.
    let v = SecretString::from("<SEND_AUTH_TOKEN from step 4>padding-to-make-this-long".to_owned());
    let err = validate_secret_strength(&v, "test", 24).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("from step") || msg.contains("<send_auth_token"),
        "expected blocked-pattern error, got: {msg}"
    );
}

#[test]
fn env_ref_grammar_accepts_valid_names() {
    assert!(is_valid_env_ref("${TOKEN_SECRET}"));
    assert!(is_valid_env_ref("${SEND_AUTH_TOKEN}"));
    assert!(is_valid_env_ref("${A}"));
    assert!(is_valid_env_ref("${_LEADING_UNDERSCORE}"));
    assert!(is_valid_env_ref("${X1_2_3}"));
}

#[test]
fn env_ref_grammar_rejects_malformed() {
    // Reaudit #29: each of these used to silently become a literal
    // secret because the old strip_prefix/strip_suffix logic treated
    // any unmatched shape as "not an env ref, use as literal".
    assert!(!is_valid_env_ref("  ${TOKEN_SECRET}"), "leading whitespace");
    assert!(
        !is_valid_env_ref("${TOKEN_SECRET}  "),
        "trailing whitespace"
    );
    assert!(!is_valid_env_ref("${TOKEN_SECRET}}"), "extra brace");
    assert!(
        !is_valid_env_ref("${TOKEN_SECRET}padding"),
        "trailing garbage"
    );
    assert!(!is_valid_env_ref("${TOKEN_SECRET}-${OTHER}"), "two refs");
    assert!(!is_valid_env_ref("${1_LEADING_DIGIT}"), "leading digit");
    assert!(!is_valid_env_ref("${lowercase}"), "lowercase");
    assert!(!is_valid_env_ref("${WITH-HYPHEN}"), "hyphen");
    assert!(!is_valid_env_ref("${}"), "empty name");
    assert!(!is_valid_env_ref("$TOKEN_SECRET"), "missing braces");
    assert!(!is_valid_env_ref(""), "empty string");
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn resolve_secret_env_rejects_partial_substitution() {
    // Reaudit #29: the literal `${TOKEN_SECRET}padding` (operator typo)
    // would have been used verbatim as the HMAC key. Now: hard error.
    let v = SecretString::from("${TOKEN_SECRET}padding".to_owned());
    let err = resolve_secret_env(&v, "test").unwrap_err();
    assert!(
        format!("{err}").contains("malformed env reference"),
        "{err}"
    );
}

#[test]
#[expect(
    unsafe_code,
    reason = "set_var is unsafe in 2024 edition; test isolates the var"
)]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn empty_env_var_errors() {
    unsafe {
        std::env::set_var("EPISTOLE_TEST_EMPTY", "");
    }
    let v = SecretString::from("${EPISTOLE_TEST_EMPTY}".to_owned());
    let err = resolve_secret_env(&v, "test").unwrap_err();
    assert!(format!("{err}").contains("empty"));
    unsafe {
        std::env::remove_var("EPISTOLE_TEST_EMPTY");
    }
}

#[test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn valid_config_loads() {
    load(&valid_config_toml()).expect("baseline config must load");
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn smtp_username_placeholder_is_refused() {
    // forkwright/epistole#42: this used to boot cleanly - username was
    // exempt from every check the sibling secrets go through.
    let toml = valid_config_toml().replace(
        r#"username = "Rn8Vt3Wc6Ym1Ap4Ez7""#,
        r#"username = "REPLACE_WITH_POSTMARK_TOKEN""#,
    );
    let err = load(&toml).unwrap_err().to_string();
    assert!(
        err.contains("smtp.username"),
        "expected the refusal to name smtp.username, got: {err}"
    );
}

#[test]
#[expect(
    unsafe_code,
    reason = "set_var is unsafe in 2024 edition; test isolates the var"
)]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn smtp_username_env_ref_resolves() {
    // SAFETY: unique var name, value read back within this test only.
    unsafe {
        std::env::set_var("EPISTOLE_TEST_SMTP_USERNAME", "Rn8Vt3Wc6Ym1Ap4Ez7Resolved");
    }
    let toml = valid_config_toml().replace(
        r#"username = "Rn8Vt3Wc6Ym1Ap4Ez7""#,
        r#"username = "${EPISTOLE_TEST_SMTP_USERNAME}""#,
    );
    let cfg = load(&toml).unwrap();
    assert_eq!(
        cfg.smtp.username.expose_secret(),
        "Rn8Vt3Wc6Ym1Ap4Ez7Resolved"
    );
    unsafe {
        std::env::remove_var("EPISTOLE_TEST_SMTP_USERNAME");
    }
}

#[test]
fn smtp_debug_redacts_username_and_password() {
    let smtp = super::Smtp {
        host: "smtp.postmarkapp.com".to_owned(),
        port: 587,
        username: SecretString::from("a-real-username-token".to_owned()),
        password: SecretString::from("a-real-password-token".to_owned()),
    };
    let debug = format!("{smtp:?}");
    assert!(debug.contains("<redacted>"), "{debug}");
    assert!(!debug.contains("a-real-username-token"), "{debug}");
    assert!(!debug.contains("a-real-password-token"), "{debug}");
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn zero_hourly_cap_is_refused() {
    let toml = valid_config_toml().replace("send_cap_per_hour = 500", "send_cap_per_hour = 0");
    let err = load(&toml).unwrap_err().to_string();
    assert!(err.contains("send_cap_per_hour"), "{err}");
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn zero_daily_cap_is_refused() {
    let toml = valid_config_toml().replace("send_cap_per_day = 2000", "send_cap_per_day = 0");
    let err = load(&toml).unwrap_err().to_string();
    assert!(err.contains("send_cap_per_day"), "{err}");
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn daily_cap_smaller_than_hourly_cap_is_refused() {
    // An unreachable daily budget (tighter than the hourly one) is
    // almost certainly a transposed pair of numbers, not intent. Only
    // the hourly cap moves - the baseline day cap (2000) stays put and
    // is now the smaller of the two.
    let toml = valid_config_toml().replace("send_cap_per_hour = 500", "send_cap_per_hour = 5000");
    let err = load(&toml).unwrap_err().to_string();
    assert!(err.contains("send_cap_per_day"), "{err}");
    assert!(err.contains("send_cap_per_hour"), "{err}");
}

#[test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn bind_policy_allows_loopback_with_no_trusted_proxies() {
    validate_bind_policy("127.0.0.1:9090", &[]).expect("loopback bind needs no policy");
    validate_bind_policy("[::1]:9090", &[]).expect("ipv6 loopback bind needs no policy");
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn bind_policy_rejects_non_loopback_bind_with_no_trusted_proxies() {
    // Issue #67 desired-correction: a public bind with an empty
    // trusted_proxies collapses every visitor behind an unconfigured
    // reverse proxy onto one rate-limit bucket, and silently defeats the
    // whole point of configuring a trust boundary. Startup refuses this
    // combination rather than booting into it quietly.
    let err = validate_bind_policy("0.0.0.0:9090", &[]).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("trusted_proxies") && msg.contains("loopback"),
        "expected a policy refusal naming both fields, got: {msg}"
    );
}

#[test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn bind_policy_allows_non_loopback_bind_once_trusted_proxies_is_set() {
    let proxies: Vec<TrustedProxyRange> = vec!["192.168.1.10".parse().expect("range")];
    validate_bind_policy("192.168.1.20:9090", &proxies)
        .expect("a declared trust policy clears a non-loopback bind");
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - fixture setup, not the assertion target"
)]
fn config_load_refuses_to_boot_with_non_loopback_bind_and_no_trusted_proxies() {
    // Asserted-vs-exhibited gap (PR #103 adversarial review, Finding 1):
    // `bind_policy_rejects_non_loopback_bind_with_no_trusted_proxies`
    // above calls `validate_bind_policy` directly — it never proves
    // `Config::load` (the entrypoint `main.rs` actually calls) refuses
    // to boot. A refactor that moved the `validate_bind_policy?` call
    // (src/config.rs's `Config::load`) behind an early return, or added
    // a second `Config`-construction path, would silently drop the
    // check while every prior test kept passing. This fixture parses a
    // full epistole.toml-shaped file through the real entrypoint.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let toml_path = dir.path().join("epistole.toml");
    let toml = format!(
        r#"
bind = "0.0.0.0:9090"
data_dir = "{data_dir}"
base_url = "https://letters.example.com"
token_secret = "Yk9mNTBjZWE3OTIzNzg5YzkzMjg0NWE2YWRkOWM4MTM"
send_auth_token = "4f3e2d1c0b9a8f7e6d5c4b3a2f1e0d9c8b7a6f5e"
webhook_auth_token = "9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b3a2f1e0d"
send_cap_per_hour = 500
send_cap_per_day = 2000

[brand]
name = "Test Brand"
from_address = "letters@example.com"

[smtp]
host = "smtp.example.com"
port = 587
username = "user"
password = "9f8e7d6c5b4a3f2e1d0c"
"#,
        data_dir = dir.path().join("data").display(),
    );
    std::fs::write(&toml_path, toml).expect("write config");

    let err = Config::load(&toml_path).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("trusted_proxies") && msg.contains("loopback"),
        "Config::load must itself surface the bind-policy refusal, got: {msg}"
    );
}

#[test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn bind_policy_defers_an_unparseable_bind_to_the_caller() {
    // Not this function's job to reject a malformed bind — main.rs's
    // own SocketAddr parse surfaces that error with its own message.
    validate_bind_policy("not-an-address", &[]).expect("unparseable bind is not this check's job");
}

#[test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn trusted_proxy_range_bare_address_is_host_length() {
    let range: TrustedProxyRange = "203.0.113.5".parse().expect("valid address");
    assert!(range.contains(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5))));
    assert!(!range.contains(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 6))));
}

#[test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn trusted_proxy_range_cidr_matches_whole_subnet() {
    // Issue #67 desired-correction: "typed CIDRs or addresses" — a
    // proxy pool behind a load balancer rarely presents one stable
    // address, so a single /32 literal per entry isn't enough.
    let range: TrustedProxyRange = "10.0.0.0/24".parse().expect("valid cidr");
    assert!(range.contains(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
    assert!(range.contains(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 254))));
    assert!(!range.contains(IpAddr::V4(Ipv4Addr::new(10, 0, 1, 1))));
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn trusted_proxy_range_rejects_prefix_exceeding_family_max() {
    let err = "10.0.0.0/33".parse::<TrustedProxyRange>().unwrap_err();
    assert!(err.contains("exceeds"), "{err}");
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test scaffolding - the err path is the assertion target"
)]
fn trusted_proxy_range_rejects_garbage_address() {
    let err = "not-an-ip/24".parse::<TrustedProxyRange>().unwrap_err();
    assert!(err.contains("invalid address"), "{err}");
}

#[test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn trusted_proxy_range_contains_normalizes_ipv4_mapped_ipv6_peer() {
    // PR #103 adversarial review, Finding 2: a dual-stack listener can
    // report an IPv4 proxy's peer address as `::ffff:a.b.c.d` rather
    // than plain IPv4. Without normalizing both sides to canonical
    // form, this comparison against a plain-IPv4 `trusted_proxies`
    // entry fails the address-family match and the legitimate proxy
    // silently drops into the untrusted branch.
    let range: TrustedProxyRange = "198.51.100.1".parse().expect("valid address");
    let mapped = IpAddr::V6(Ipv4Addr::new(198, 51, 100, 1).to_ipv6_mapped());
    assert!(range.contains(mapped));
}

#[test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn trusted_proxy_range_family_mismatch_never_matches() {
    let range: TrustedProxyRange = "203.0.113.0/24".parse().expect("valid cidr");
    assert!(!range.contains(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)));
}
