//! Tests for the `${VAR}` env-resolution + secret-strength validation
//! paths in `Config::load`.

use secrecy::{ExposeSecret, SecretString};

use super::{resolve_secret_env, validate_secret_strength};

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
