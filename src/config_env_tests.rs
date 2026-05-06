//! Tests for the `${VAR}` env-resolution + secret-strength validation
//! paths in `Config::load`.

use secrecy::{ExposeSecret, SecretString};

use super::{is_valid_env_ref, resolve_secret_env, validate_secret_strength};

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
